//! Whole folders: finding the videos, one clip described with what grouping needs
//! ([`describe_clip`]), a budget cap ([`Budget`]), and a run over many clips with several in flight
//! that skips what the cache already has ([`describe_folder`]). See the design notes in
//! `docs/design/whole-folders.md`.
//!
//! The building blocks are public on their own so a program with its own progress display
//! (frename) can drive them: [`describe_folder`] with its [`FolderEvent`]s, or its own loop over
//! [`crate::CacheKey`], [`crate::Cache`], [`describe_clip`] and [`Budget`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use crate::describe::{frame_tokens, Description, Model, FRAME_LONG_SIDE};
use crate::provider::{AiContent, AiRequest, AiUsage, Provider};
use crate::tags::{Tag, TagSuggestions};

/// Extensions of the files a folder contributes (compared without regard to case).
pub const VIDEO_EXTENSIONS: [&str; 12] = [
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "mts", "m2ts", "wmv", "mpg", "mpeg", "3gp",
];

/// Clips in flight at once in a folder run, unless [`RunOptions::jobs`] says otherwise.
pub const DEFAULT_JOBS: usize = 4;

/// Characters of prompt text per token for [`request_cost_bound`], counted in UTF-8 bytes: an
/// overestimate for English (about 4 characters per token) and for Cyrillic (2 bytes a character).
const BYTES_PER_TOKEN: f64 = 3.5;
/// Input tokens a provider adds to a request around its content (the answer's JSON schema, its
/// own instructions for structured output), for [`request_cost_bound`].
const REQUEST_OVERHEAD_TOKENS: u64 = 500;
/// How much [`request_cost_bound`] adds to its input estimate, for what it cannot see.
const INPUT_MARGIN: f64 = 1.1;

/// The videos of `inputs`: files as given, folders as the videos directly in them (by
/// [`VIDEO_EXTENSIONS`]), sorted. An input that does not exist is an error.
pub fn find_videos(inputs: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
    let mut videos = Vec::new();
    for input in inputs {
        if input.is_dir() {
            let mut found: Vec<PathBuf> = std::fs::read_dir(input)?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| path.is_file() && is_video(path))
                .collect();
            found.sort();
            videos.extend(found);
        } else if input.is_file() {
            videos.push(input.clone());
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} not found", input.display()),
            ));
        }
    }
    Ok(videos)
}

/// Whether `path` has one of [`VIDEO_EXTENSIONS`].
pub(crate) fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The fingerprint of one frame sent to the model: an 8×8 grid of average luma (64 bytes, rows
/// packed) of the upright frame, what [`crate::group_clips`] compares.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameFingerprint {
    /// Where the frame is in the clip, in seconds from its start.
    pub time_s: f64,
    /// 64 bytes: the average luma (0–255) of each cell of an 8×8 grid, row by row.
    pub fingerprint: Vec<u8>,
}

/// A clip described by [`describe_clip`]: what [`crate::describe`] (or
/// [`crate::describe_with_tags`]) gives, plus the fingerprints of the frames it was described
/// from, for grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct DescribedClip {
    /// The summary and the moments, as [`crate::describe`] gives them.
    pub description: Description,
    /// Tag suggestions, when a vocabulary was given.
    pub tags: Option<TagSuggestions>,
    /// What the request was billed for; see [`Model::cost_usd`].
    pub usage: AiUsage,
    /// The clip's length, in seconds.
    pub duration_s: f64,
    /// One per frame sent, in time order.
    pub frames: Vec<FrameFingerprint>,
}

/// A spending cap shared by the requests of a run: what has been spent, plus what the requests in
/// flight could still cost. See [`Budget::reserve`] and [`Budget::reserve_or_wait`].
#[derive(Debug, Default)]
pub struct Budget {
    max_usd: Option<f64>,
    state: Mutex<Spending>,
    /// Signalled whenever a reservation is settled or released, for [`Budget::reserve_or_wait`].
    changed: Condvar,
}

#[derive(Debug, Default)]
struct Spending {
    spent: f64,
    reserved: f64,
    /// The bound of the last request [`Budget::reserve_or_wait`] found over budget.
    refused: Option<f64>,
    /// How many times [`Budget::reserve_or_wait`] had to wait for other reservations to settle.
    waits: usize,
}

/// What [`Budget::reserve_or_wait`] got.
#[derive(Debug)]
pub enum Reserved<'a> {
    /// The money is set aside: send the request.
    Yes(Reservation<'a>),
    /// Even with nothing else in flight, what is spent and this request could pass the cap: do not
    /// send it.
    OverBudget,
    /// `cancel` was set while waiting for other requests to settle.
    Cancelled,
}

/// How often [`Budget::reserve_or_wait`] looks at the cancel flag while it waits.
const CANCEL_CHECK: std::time::Duration = std::time::Duration::from_millis(50);

impl Budget {
    /// A budget of at most `max_usd` US dollars; `None` is no cap (spending is still counted).
    pub fn new(max_usd: Option<f64>) -> Self {
        Self {
            max_usd,
            state: Mutex::default(),
            changed: Condvar::new(),
        }
    }

    /// The cap in US dollars; `None` when there is none.
    pub fn max_usd(&self) -> Option<f64> {
        self.max_usd
    }

    /// Spent so far, in US dollars (reservations not settled yet left out).
    pub fn spent_usd(&self) -> f64 {
        self.lock().spent
    }

    /// What the last request found [`Reserved::OverBudget`] by [`Budget::reserve_or_wait`] could
    /// have cost (its bound, see [`request_cost_bound`]), in US dollars: why a run stopped short of
    /// the cap. `None` when no request was refused.
    pub fn refused_usd(&self) -> Option<f64> {
        self.lock().refused
    }

    /// Set aside `usd` for a request about to be sent, or `None` when what is spent, what is set
    /// aside for other requests and `usd` together would pass the cap: then the request must not be
    /// sent now. Never waits; see [`Budget::reserve_or_wait`] for a request that should wait for
    /// the others in flight instead. The reservation is released when dropped, or replaced by what
    /// the request really cost with [`Reservation::settle`].
    pub fn reserve(&self, usd: f64) -> Option<Reservation<'_>> {
        let usd = usd.max(0.0);
        let mut state = self.lock();
        if let Some(max) = self.max_usd {
            if state.spent + state.reserved + usd > max {
                return None;
            }
        }
        state.reserved += usd;
        Some(self.reservation(usd))
    }

    /// Set aside `usd` for a request about to be sent, like [`Budget::reserve`], but when it does
    /// not fit only because of what other requests in flight have set aside, wait for them to
    /// settle (they usually cost a fraction of their reservation) and try again.
    /// [`Reserved::OverBudget`] only when what is already spent plus `usd` alone would pass the
    /// cap. Checks `cancel` while it waits.
    ///
    /// A caller must not wait while holding a reservation of its own from this budget: only
    /// reservations held by other threads can settle while it waits.
    pub fn reserve_or_wait(&self, usd: f64, cancel: &AtomicBool) -> Reserved<'_> {
        let usd = usd.max(0.0);
        let mut state = self.lock();
        let mut waited = false;
        loop {
            let fits = |state: &Spending, others: f64| {
                self.max_usd
                    .is_none_or(|max| state.spent + others + usd <= max)
            };
            if !fits(&state, 0.0) {
                state.refused = Some(usd);
                return Reserved::OverBudget;
            }
            if fits(&state, state.reserved) {
                state.reserved += usd;
                return Reserved::Yes(self.reservation(usd));
            }
            if cancel.load(Ordering::Relaxed) {
                return Reserved::Cancelled;
            }
            if !waited {
                waited = true;
                state.waits += 1;
            }
            state = self
                .changed
                .wait_timeout(state, CANCEL_CHECK)
                .map_or_else(|e| e.into_inner().0, |(state, _)| state);
        }
    }

    /// How many times [`Budget::reserve_or_wait`] waited for other requests to settle.
    #[cfg(test)]
    pub(crate) fn waits(&self) -> usize {
        self.lock().waits
    }

    fn reservation(&self, usd: f64) -> Reservation<'_> {
        Reservation {
            budget: self,
            usd,
            settled: false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Spending> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Money set aside by [`Budget::reserve`] for one request.
#[derive(Debug)]
pub struct Reservation<'a> {
    budget: &'a Budget,
    usd: f64,
    settled: bool,
}

impl Reservation<'_> {
    /// What was set aside, in US dollars.
    pub fn usd(&self) -> f64 {
        self.usd
    }

    /// The request cost `actual_usd`: count that as spent instead of the reservation.
    pub fn settle(mut self, actual_usd: f64) {
        let mut state = self.budget.lock();
        state.reserved = (state.reserved - self.usd).max(0.0);
        state.spent += actual_usd.max(0.0);
        self.settled = true;
        drop(state);
        self.budget.changed.notify_all();
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.settled {
            let mut state = self.budget.lock();
            state.reserved = (state.reserved - self.usd).max(0.0);
            drop(state);
            self.budget.changed.notify_all();
        }
    }
}

/// The most `request` can cost with `model`, in US dollars, for [`Budget::reserve`]: every image
/// at its real size (see [`image_tokens_bound`]; a JPEG whose size cannot be read counts as a
/// [`FRAME_LONG_SIDE`] square), the text at 3.5 bytes a token, the provider's own additions, 10 %
/// on top, and the answer at the request's `max_tokens` — the most it can be billed for.
pub fn request_cost_bound(model: Model, request: &AiRequest) -> f64 {
    let content: f64 = request
        .content
        .iter()
        .map(|block| match block {
            AiContent::Text(text) => text.len() as f64 / BYTES_PER_TOKEN,
            AiContent::Jpeg(jpeg) => {
                let (w, h) = jpeg_size(jpeg).unwrap_or((FRAME_LONG_SIDE, FRAME_LONG_SIDE));
                image_tokens_bound(model.provider, w, h) as f64
            }
        })
        .sum();
    let schema = request.schema.to_string().len() as f64 / BYTES_PER_TOKEN;
    let input = (content + schema + REQUEST_OVERHEAD_TOKENS as f64) * INPUT_MARGIN;
    model.cost_usd(AiUsage {
        input_tokens: input.ceil() as u64,
        output_tokens: u64::from(request.max_tokens),
    })
}

/// Input tokens a `width`×`height` image can be billed for by `provider`. Anthropic: one per 28×28
/// tile ([`frame_tokens`]). OpenAI bills images differently depending on the model, so the larger
/// of its two published schemes: 85 plus 170 per 512×512 tile (GPT-4.1), or one per 32×32 patch
/// times 1.62 (GPT-4.1 mini) — a 512×288 frame is 255 tokens there, 209 with Anthropic.
fn image_tokens_bound(provider: Provider, width: u32, height: u32) -> u64 {
    match provider {
        Provider::Anthropic => frame_tokens(width, height),
        Provider::OpenAi => {
            let tiles = u64::from(width.div_ceil(512)) * u64::from(height.div_ceil(512));
            let patches = u64::from(width.div_ceil(32)) * u64::from(height.div_ceil(32));
            (85 + 170 * tiles).max((patches as f64 * 1.62).ceil() as u64)
        }
    }
}

/// Width and height of a JPEG from its frame header (SOF marker), without decoding it.
fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut i = 2;
    loop {
        if *jpeg.get(i)? != 0xFF {
            return None;
        }
        let marker = *jpeg.get(i + 1)?;
        match marker {
            // Padding before a marker.
            0xFF => i += 1,
            // Markers without a length.
            0x01 | 0xD0..=0xD7 => i += 2,
            // Start of frame (baseline, progressive, …; not DHT/JPG/DAC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let height = u16::from_be_bytes([*jpeg.get(i + 5)?, *jpeg.get(i + 6)?]);
                let width = u16::from_be_bytes([*jpeg.get(i + 7)?, *jpeg.get(i + 8)?]);
                return Some((u32::from(width), u32::from(height)));
            }
            _ => {
                let length = u16::from_be_bytes([*jpeg.get(i + 2)?, *jpeg.get(i + 3)?]);
                i += 2 + usize::from(length);
            }
        }
    }
}

/// How a folder run goes, besides the [`crate::Options`] each clip is described with.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOptions {
    /// Clips in flight at once (at least 1); [`DEFAULT_JOBS`] by default.
    pub jobs: usize,
    /// Describe every clip again even when the cache has it (the new results still go to the
    /// cache).
    pub force: bool,
    /// Send the `.srt` next to each video, if there is one.
    pub subtitles: bool,
    /// Suggest tags from this vocabulary in the same request (see [`crate::describe_with_tags`]).
    pub vocabulary: Option<Vec<Tag>>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            jobs: DEFAULT_JOBS,
            force: false,
            subtitles: true,
            vocabulary: None,
        }
    }
}

/// What became of one clip of a folder run.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipOutcome {
    /// Described now (and written to the cache, when there is one).
    Described(crate::ClipRecord),
    /// Found in the cache: nothing was sent.
    Cached(crate::ClipRecord),
    /// Not described; [`crate::Error::Cancelled`] when the run was cancelled while it was in work.
    Failed(crate::Error),
    /// Its frames were read, but sending the request could have passed the budget even with no
    /// other request in flight: nothing was sent, and the run stopped describing new clips.
    OverBudget,
    /// The run stopped before reaching it.
    NotStarted,
}

impl ClipOutcome {
    /// The described clip, fresh or from the cache.
    pub fn record(&self) -> Option<&crate::ClipRecord> {
        match self {
            Self::Described(record) | Self::Cached(record) => Some(record),
            _ => None,
        }
    }
}

/// Why a folder run stopped before every clip was done.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    /// `cancel` was set.
    Cancelled,
    /// The next request could have passed the budget, even with no other request in flight.
    OverBudget,
    /// An error that would fail every clip the same way (see [`crate::AiError::stops_job`]); its
    /// summary line.
    Job(String),
}

/// What a folder run did.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderRun {
    /// One per video, in the order given.
    pub clips: Vec<ClipOutcome>,
    /// What this run's requests were billed for (cached clips cost nothing), bad answers
    /// included.
    pub usage: AiUsage,
    /// `None` when every clip was tried.
    pub stopped: Option<Stop>,
}

/// Progress of a folder run, for a display. Sent from the worker threads, several clips at once.
#[derive(Debug)]
pub enum FolderEvent<'a> {
    /// The clip at `index` of `videos` is taken up.
    Started { index: usize, video: &'a Path },
    /// Where describing it is.
    Stage {
        index: usize,
        video: &'a Path,
        stage: crate::Stage,
    },
    /// Something went wrong that does not fail the clip (subtitles not read, the cache not
    /// written).
    Warning {
        index: usize,
        video: &'a Path,
        message: String,
    },
    /// Done with it; the same outcome [`FolderRun::clips`] will hold. Also sent, without a
    /// `Started` before it, for each clip left [`ClipOutcome::NotStarted`] after the run stopped
    /// for the budget or an error (not after a cancel), so a display showing clips in order can
    /// move past it to the clips served from the cache after it.
    Finished {
        index: usize,
        video: &'a Path,
        outcome: &'a ClipOutcome,
    },
}

/// One line on what a stopped run left undone, for a summary.
impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Cancelled."),
            Self::OverBudget => f.write_str(
                "Stopped: the next video could take the run past its budget. Raise the budget to \
                 describe the rest.",
            ),
            Self::Job(summary) => f.write_str(summary),
        }
    }
}

/// What `cache` already has for `video` described with `options` and `run` (its vocabulary and
/// subtitles setting): the same file with the same settings. Always `None` with `run.force`, and
/// when the video cannot be read to tell its identity. Reads at most 192 KiB of the video (see
/// [`crate::FileIdentity`]); nothing is sent. [`describe_folder`] serves these even after it has
/// stopped for the budget or a rejected key, since they cost nothing.
pub fn cached_clip(
    video: &Path,
    cache: &crate::Cache,
    run: &RunOptions,
    options: &crate::Options,
) -> Option<crate::ClipRecord> {
    if run.force {
        return None;
    }
    let key =
        crate::CacheKey::new(video, options, run.vocabulary.as_deref(), run.subtitles).ok()?;
    cache.get(&key)
}

/// What becomes of the clips a run did not take up because it stopped for `stop`: `videos` are
/// those clips, each with its index for the events. After [`Stop::OverBudget`] or [`Stop::Job`],
/// each is looked up in `cache` ([`cached_clip`]: nothing is sent) and served
/// ([`ClipOutcome::Cached`]) when it is there, else left [`ClipOutcome::NotStarted`]; `on_event`
/// gets `Started` for a cached one and `Finished` for each, in the order given, so a display
/// showing clips in order can move past them. After [`Stop::Cancelled`] every one is
/// `NotStarted`, with no lookup and no event: Ctrl+C means stop now.
///
/// [`describe_folder`] does this for its own clips; a program running several folders one after
/// another (each with its own cache, one [`Budget`]) calls it for the folders after the one that
/// stopped.
pub fn serve_after_stop<'v>(
    videos: impl IntoIterator<Item = (usize, &'v Path)>,
    cache: Option<&crate::Cache>,
    run: &RunOptions,
    options: &crate::Options,
    stop: &Stop,
    on_event: &dyn Fn(FolderEvent<'_>),
) -> Vec<ClipOutcome> {
    videos
        .into_iter()
        .map(|(index, video)| {
            if *stop == Stop::Cancelled {
                return ClipOutcome::NotStarted;
            }
            let outcome = cache
                .and_then(|cache| cached_clip(video, cache, run, options))
                .map_or(ClipOutcome::NotStarted, ClipOutcome::Cached);
            if matches!(outcome, ClipOutcome::Cached(_)) {
                on_event(FolderEvent::Started { index, video });
            }
            on_event(FolderEvent::Finished {
                index,
                video,
                outcome: &outcome,
            });
            outcome
        })
        .collect()
}

#[cfg(feature = "frames")]
pub use run::{describe_clip, describe_folder};

#[cfg(feature = "frames")]
mod run {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        request_cost_bound, serve_after_stop, Budget, ClipOutcome, DescribedClip, FolderEvent,
        FolderRun, FrameFingerprint, Reserved, RunOptions, Stop,
    };
    use crate::cache::{Cache, CacheKey, ClipRecord};
    use crate::describe::{build_request, parse_answer, FrameSampling};
    use crate::frames::{self, Sampled};
    use crate::provider::{AiError, AiProvider, AiRequest, AiUsage, RateGate};
    use crate::tags::{build_combined_request, parse_combined_answer, Tag};
    use crate::{srt, Cue, Error, Options, Stage, MAX_DURATION_S};

    /// Describe the clip at `video` like [`crate::describe`] (or [`crate::describe_with_tags`]
    /// when `vocabulary` is given), keeping its frames' fingerprints for [`crate::group_clips`],
    /// within `budget`: once its frames are read, the request's upper bound
    /// ([`super::request_cost_bound`]) is reserved first ([`Budget::reserve_or_wait`]: waiting
    /// for other requests in flight on the same budget to settle when only they are in the way),
    /// and `Ok(None)` means it could not fit even then, so nothing was sent. A timeout, and an
    /// answer that came back but could not be read at all ([`crate::AiError::BadAnswer`]), count
    /// the whole bound as spent (either may have been billed, for an amount the error does not
    /// carry); an answer read but unusable ([`Error::BadAnswer`]) counts its real usage; any other
    /// failure spends nothing.
    ///
    /// [`crate::describe`] and [`crate::describe_with_tags`] are this with a budget without a
    /// cap: one clip is always asked for the same way.
    pub fn describe_clip(
        video: &Path,
        subtitles: &[Cue],
        vocabulary: Option<&[Tag]>,
        options: &Options,
        budget: &Budget,
        cancel: &AtomicBool,
        on_stage: impl FnMut(Stage),
    ) -> Result<Option<DescribedClip>, Error> {
        let provider = crate::provider_for(options)?;
        let worker = Worker {
            provider: provider.as_ref(),
            options,
            budget,
            cancel,
        };
        worker.describe(video, subtitles, vocabulary, on_stage)
    }

    /// A clip's frames, read, and the request made from them: everything before a request is sent.
    pub(crate) struct Prepared {
        /// The clip's length, in seconds.
        pub(crate) duration_s: f64,
        /// One per frame in the request, in time order, for grouping.
        pub(crate) fingerprints: Vec<FrameFingerprint>,
        pub(crate) request: AiRequest,
    }

    /// Read `video`'s frames and build the request describing it (with tag suggestions from
    /// `vocabulary`, when given): the one place a whole-clip request is made.
    pub(crate) fn prepare(
        video: &Path,
        subtitles: &[Cue],
        vocabulary: Option<&[Tag]>,
        options: &Options,
        cancel: &AtomicBool,
        on_stage: &mut impl FnMut(Stage),
    ) -> Result<Prepared, Error> {
        let (
            duration_s,
            Sampled {
                frames,
                fingerprints,
            },
        ) = read_frames(video, options.frame_sampling, cancel, on_stage)?;
        let request = match vocabulary {
            Some(vocabulary) => build_combined_request(
                options.model,
                &frames,
                subtitles,
                vocabulary,
                duration_s,
                options.language,
                options.moments,
            ),
            None => build_request(
                options.model,
                &frames,
                subtitles,
                duration_s,
                options.language,
                options.moments,
            ),
        };
        Ok(Prepared {
            duration_s,
            fingerprints,
            request,
        })
    }

    /// The length of the clip at `video`, in seconds, and the frames a whole-clip description is
    /// made from, each with its fingerprint.
    fn read_frames(
        video: &Path,
        sampling: FrameSampling,
        cancel: &AtomicBool,
        on_stage: &mut impl FnMut(Stage),
    ) -> Result<(f64, Sampled), Error> {
        let clip = frames::Clip::open(video, frames::OPEN_TIMEOUT).map_err(Error::Unreadable)?;
        let duration_s = clip
            .duration_s()
            .ok_or_else(|| Error::Unreadable("no duration".to_string()))?;
        if duration_s > MAX_DURATION_S {
            return Err(Error::TooLong(duration_s));
        }
        match clip.sample_with_fingerprints(duration_s, sampling, cancel, |done, total| {
            on_stage(Stage::Frame { done, total })
        }) {
            Ok(Some(sampled)) if !sampled.frames.is_empty() => Ok((duration_s, sampled)),
            Ok(Some(_)) => Err(Error::Unreadable("no frames".to_string())),
            Ok(None) => Err(Error::Cancelled),
            Err(e) => Err(Error::Unreadable(e)),
        }
    }

    /// One clip's request, with a given client.
    struct Worker<'a> {
        provider: &'a dyn AiProvider,
        options: &'a Options,
        budget: &'a Budget,
        cancel: &'a AtomicBool,
    }

    impl Worker<'_> {
        fn describe(
            &self,
            video: &Path,
            subtitles: &[Cue],
            vocabulary: Option<&[Tag]>,
            mut on_stage: impl FnMut(Stage),
        ) -> Result<Option<DescribedClip>, Error> {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let options = self.options;
            let Prepared {
                duration_s,
                fingerprints,
                request,
            } = prepare(
                video,
                subtitles,
                vocabulary,
                options,
                self.cancel,
                &mut on_stage,
            )?;
            let bound = request_cost_bound(options.model, &request);
            let reservation = match self.budget.reserve_or_wait(bound, self.cancel) {
                Reserved::Yes(reservation) => reservation,
                Reserved::OverBudget => return Ok(None),
                Reserved::Cancelled => return Err(Error::Cancelled),
            };
            // The last moment a Ctrl+C can still save the money: the reservation is released.
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            on_stage(Stage::Asking);
            let response = match self.provider.complete(&request, self.cancel) {
                Ok(response) => response,
                // Both may have been billed without saying how much: a timeout, and an answer
                // that came back but could not be read (its usage is not carried by the error).
                // The whole bound counts as spent, a conservative estimate.
                Err(e @ (AiError::Timeout | AiError::BadAnswer(_))) => {
                    reservation.settle(bound);
                    return Err(Error::Ai(e));
                }
                Err(AiError::Cancelled) => return Err(Error::Cancelled),
                Err(e) => return Err(Error::Ai(e)),
            };
            reservation.settle(options.model.cost_usd(response.usage));
            let answer = match vocabulary {
                Some(vocabulary) => {
                    parse_combined_answer(&response, duration_s, vocabulary, options.moments)
                        .map(|(description, tags)| (description, Some(tags)))
                }
                None => parse_answer(&response, duration_s, options.moments)
                    .map(|description| (description, None)),
            };
            let (description, tags) = answer.map_err(|reason| Error::BadAnswer {
                reason,
                usage: response.usage,
            })?;
            Ok(Some(DescribedClip {
                description,
                tags,
                usage: response.usage,
                duration_s,
                frames: fingerprints,
            }))
        }
    }

    /// Describe `videos` with up to `run.jobs` clips in flight, skipping those `cache` already
    /// has (unless `run.force`) and adding every newly described one to it, within `budget`.
    ///
    /// Each clip is looked up by its [`CacheKey`] (the file's identity and how it is described).
    /// A new result is written to the cache as soon as it is in, so a crash or a cancel loses at
    /// most the clips in flight. Every worker's client shares one pause: a 429 on one holds the
    /// others back too. A worker whose request does not fit `budget` only because of the others
    /// in flight waits for them to settle. The run stops describing new clips when a request
    /// could pass `budget` even with nothing else in flight ([`ClipOutcome::OverBudget`]) or when
    /// an error would fail every clip the same way (a rejected key, no credit); clips already in
    /// flight finish, and the clips left are still served from the cache when it has them
    /// (nothing is sent for those), the others left [`ClipOutcome::NotStarted`]. `cancel` stops
    /// at once: nothing more is taken up. `on_event` follows along from the worker threads.
    /// Blocks until done.
    pub fn describe_folder(
        videos: &[PathBuf],
        cache: Option<&Cache>,
        run: &RunOptions,
        options: &Options,
        budget: &Budget,
        cancel: &AtomicBool,
        on_event: impl Fn(FolderEvent<'_>) + Sync,
    ) -> FolderRun {
        let gate = Arc::new(RateGate::default());
        let make_provider = || -> Result<Box<dyn AiProvider>, Error> {
            crate::gated_provider_for(options, gate.clone())
        };
        Runner {
            cache,
            run,
            options,
            budget,
            cancel,
        }
        .go(videos, &make_provider, &on_event)
    }

    /// The parts of a folder run every worker shares.
    pub(crate) struct Runner<'a> {
        pub(crate) cache: Option<&'a Cache>,
        pub(crate) run: &'a RunOptions,
        pub(crate) options: &'a Options,
        pub(crate) budget: &'a Budget,
        pub(crate) cancel: &'a AtomicBool,
    }

    impl Runner<'_> {
        /// [`describe_folder`], with the clients made by `make_provider` (one per worker).
        pub(crate) fn go(
            &self,
            videos: &[PathBuf],
            make_provider: &(dyn Fn() -> Result<Box<dyn AiProvider>, Error> + Sync),
            on_event: &(dyn Fn(FolderEvent<'_>) + Sync),
        ) -> FolderRun {
            // `None`: not taken up (the run stopped first).
            let outcomes: Vec<Mutex<Option<ClipOutcome>>> =
                videos.iter().map(|_| Mutex::new(None)).collect();
            let next = AtomicUsize::new(0);
            let halt = AtomicBool::new(false);
            let stopped: Mutex<Option<Stop>> = Mutex::new(None);
            let usage = Mutex::new(AiUsage::default());
            let stop = |why: Stop| {
                halt.store(true, Ordering::Relaxed);
                let mut stopped = stopped.lock().unwrap_or_else(|e| e.into_inner());
                stopped.get_or_insert(why);
            };
            let jobs = self.run.jobs.clamp(1, videos.len().max(1));
            std::thread::scope(|scope| {
                for _ in 0..jobs {
                    scope.spawn(|| {
                        let provider = make_provider();
                        loop {
                            if self.cancel.load(Ordering::Relaxed) {
                                break;
                            }
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(video) = videos.get(index) else {
                                break;
                            };
                            if halt.load(Ordering::Relaxed) {
                                // Left for `serve_after_stop`, once the clips in flight are done.
                                break;
                            }
                            on_event(FolderEvent::Started { index, video });
                            let outcome = match &provider {
                                Ok(provider) => self.one(index, video, provider.as_ref(), on_event),
                                Err(e) => ClipOutcome::Failed(e.clone()),
                            };
                            match &outcome {
                                ClipOutcome::Described(record) => {
                                    *usage.lock().unwrap_or_else(|e| e.into_inner()) +=
                                        record.clip.usage;
                                }
                                ClipOutcome::Failed(Error::BadAnswer { usage: billed, .. }) => {
                                    *usage.lock().unwrap_or_else(|e| e.into_inner()) += *billed;
                                }
                                ClipOutcome::Failed(Error::Cancelled) => stop(Stop::Cancelled),
                                ClipOutcome::Failed(Error::Ai(e)) => {
                                    if let Some(summary) = e.stops_job() {
                                        stop(Stop::Job(summary));
                                    }
                                }
                                ClipOutcome::OverBudget => stop(Stop::OverBudget),
                                _ => {}
                            }
                            on_event(FolderEvent::Finished {
                                index,
                                video,
                                outcome: &outcome,
                            });
                            *outcomes[index].lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(outcome);
                        }
                    });
                }
            });
            if self.cancel.load(Ordering::Relaxed) {
                stop(Stop::Cancelled);
            }
            let stopped = stopped.into_inner().unwrap_or_else(|e| e.into_inner());
            let mut clips: Vec<Option<ClipOutcome>> = outcomes
                .into_iter()
                .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
                .collect();
            let left: Vec<usize> = (0..clips.len()).filter(|&i| clips[i].is_none()).collect();
            if let Some(why) = stopped.as_ref().filter(|_| !left.is_empty()) {
                let served = serve_after_stop(
                    left.iter().map(|&i| (i, videos[i].as_path())),
                    self.cache,
                    self.run,
                    self.options,
                    why,
                    on_event,
                );
                for (i, outcome) in left.into_iter().zip(served) {
                    clips[i] = Some(outcome);
                }
            }
            let clips: Vec<ClipOutcome> = clips
                .into_iter()
                .map(|c| c.unwrap_or(ClipOutcome::NotStarted))
                .collect();
            // A run that was stopped only after its last clip started still did everything.
            let stopped = stopped.filter(|_| {
                clips
                    .iter()
                    .any(|c| !matches!(c, ClipOutcome::Described(_) | ClipOutcome::Cached(_)))
            });
            FolderRun {
                clips,
                usage: usage.into_inner().unwrap_or_else(|e| e.into_inner()),
                stopped,
            }
        }

        /// The clip at `index`: from the cache, or described (and cached).
        fn one(
            &self,
            index: usize,
            video: &Path,
            provider: &dyn AiProvider,
            on_event: &(dyn Fn(FolderEvent<'_>) + Sync),
        ) -> ClipOutcome {
            let vocabulary = self.run.vocabulary.as_deref();
            let key = match CacheKey::new(video, self.options, vocabulary, self.run.subtitles) {
                Ok(key) => key,
                Err(e) => return ClipOutcome::Failed(Error::Unreadable(e.to_string())),
            };
            if let Some(cache) = self.cache.filter(|_| !self.run.force) {
                if let Some(record) = cache.get(&key) {
                    return ClipOutcome::Cached(record);
                }
            }
            let subtitles = if self.run.subtitles {
                srt::load_for(video).unwrap_or_else(|e| {
                    on_event(FolderEvent::Warning {
                        index,
                        video,
                        message: format!("subtitles not read: {e}"),
                    });
                    Vec::new()
                })
            } else {
                Vec::new()
            };
            let worker = Worker {
                provider,
                options: self.options,
                budget: self.budget,
                cancel: self.cancel,
            };
            let described = worker.describe(video, &subtitles, vocabulary, |stage| {
                on_event(FolderEvent::Stage {
                    index,
                    video,
                    stage,
                })
            });
            match described {
                Ok(Some(clip)) => {
                    let record = ClipRecord {
                        file: video.to_path_buf(),
                        key,
                        model: self.options.model.id.to_string(),
                        clip,
                    };
                    if let Some(cache) = self.cache {
                        if let Err(e) = cache.put(&record) {
                            on_event(FolderEvent::Warning {
                                index,
                                video,
                                message: format!(
                                    "not written to the cache {}: {e}",
                                    cache.path().display()
                                ),
                            });
                        }
                    }
                    ClipOutcome::Described(record)
                }
                Ok(None) => ClipOutcome::OverBudget,
                Err(e) => ClipOutcome::Failed(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::{estimate_usage, Frame, MomentsMode, SummaryLanguage, MODELS};

    #[test]
    fn a_folder_means_its_videos_sorted_and_a_missing_input_is_an_error() {
        let dir = std::env::temp_dir().join(format!("clipscribe-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("inner")).expect("dir");
        for name in ["b.MP4", "a.mov", "notes.txt", "a.srt", "inner/c.mp4"] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        let found = find_videos(std::slice::from_ref(&dir)).expect("found");
        assert_eq!(found, vec![dir.join("a.mov"), dir.join("b.MP4")]);
        let single = find_videos(&[dir.join("notes.txt")]).expect("a file as given");
        assert_eq!(single, vec![dir.join("notes.txt")]);
        assert!(find_videos(&[dir.join("missing.mp4")]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_budget_refuses_what_would_pass_the_cap_counting_what_is_in_flight() {
        let budget = Budget::new(Some(1.0));
        let first = budget.reserve(0.6).expect("fits");
        assert!(budget.reserve(0.5).is_none(), "0.6 in flight + 0.5 > 1.0");
        let second = budget.reserve(0.4).expect("0.6 + 0.4 fits exactly");
        first.settle(0.1);
        assert!((budget.spent_usd() - 0.1).abs() < 1e-12);
        drop(second); // failed: nothing billed, released
        assert!((budget.spent_usd() - 0.1).abs() < 1e-12);
        let third = budget.reserve(0.9).expect("0.1 spent + 0.9 fits");
        assert_eq!(third.usd(), 0.9);
        third.settle(0.9);
        assert!(budget.reserve(0.01).is_none(), "the cap is reached");
        assert!(Budget::new(None).reserve(1e9).is_some(), "no cap");
    }

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        #[cfg(feature = "frames")]
        {
            let mut bytes = Vec::new();
            image::codecs::jpeg::JpegEncoder::new(std::io::Cursor::new(&mut bytes))
                .encode_image(&image::RgbImage::new(width, height))
                .expect("jpeg");
            bytes
        }
        // A minimal header by hand without the `image` crate: SOI, an APP0 segment, SOF0.
        #[cfg(not(feature = "frames"))]
        {
            let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
            bytes.extend([0xFF, 0xC0, 0x00, 0x11, 0x08]);
            bytes.extend((height as u16).to_be_bytes());
            bytes.extend((width as u16).to_be_bytes());
            bytes
        }
    }

    #[test]
    fn a_jpeg_size_is_read_from_its_header() {
        assert_eq!(jpeg_size(&jpeg(512, 288)), Some((512, 288)));
        assert_eq!(jpeg_size(&jpeg(270, 480)), Some((270, 480)));
        assert_eq!(jpeg_size(b"not a jpeg"), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8, 0xFF]), None, "cut short");
    }

    /// The bound is what the budget reserves: it must never be below what the request can really
    /// cost, so it is above the typical estimate, and the answer is counted at `max_tokens`.
    #[test]
    fn the_cost_bound_covers_the_frames_the_text_and_the_longest_answer() {
        let frames: Vec<Frame> = (0..15)
            .map(|i| Frame {
                time_s: f64::from(i) * 2.0,
                jpeg: jpeg(512, 288),
            })
            .collect();
        let request = crate::describe::build_request(
            MODELS[0],
            &frames,
            &[],
            30.0,
            SummaryLanguage::English,
            MomentsMode::Important,
        );
        let bound = request_cost_bound(MODELS[0], &request);
        let typical = MODELS[0].cost_usd(estimate_usage(MODELS[0], 30.0, 0));
        assert!(bound > typical, "{bound} vs {typical}");
        let answer_alone = MODELS[0].cost_usd(AiUsage {
            input_tokens: 0,
            output_tokens: u64::from(MODELS[0].max_answer_tokens),
        });
        let images = MODELS[0].cost_usd(AiUsage {
            input_tokens: 15 * frame_tokens(512, 288),
            output_tokens: 0,
        });
        assert!(bound > answer_alone + images, "{bound}");
        assert!(bound < 0.05, "still about one Haiku clip: {bound}");
    }

    #[test]
    fn every_clip_is_counted_as_done_only_when_described_or_cached() {
        assert!(ClipOutcome::OverBudget.record().is_none());
        assert!(ClipOutcome::NotStarted.record().is_none());
        assert!(
            Stop::OverBudget.to_string().contains("Raise the budget"),
            "says what to do"
        );
    }

    /// After a budget stop every clip left is finished (not started, with no cache to serve it
    /// from), in order, so a display can move past them; after a cancel nothing more happens.
    #[test]
    fn after_a_stop_the_clips_left_are_finished_in_order_unless_cancelled() {
        let videos = [PathBuf::from("a.mp4"), PathBuf::from("b.mp4")];
        let options = crate::Options {
            api_key: String::new(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: crate::FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
        };
        let finished = Mutex::new(Vec::new());
        let on_event = |event: FolderEvent<'_>| {
            if let FolderEvent::Finished { index, .. } = event {
                finished.lock().expect("lock").push(index);
            }
        };
        let left = || videos.iter().enumerate().map(|(i, v)| (i + 3, v.as_path()));
        let run = RunOptions::default();
        let served = serve_after_stop(left(), None, &run, &options, &Stop::OverBudget, &on_event);
        assert_eq!(served, [ClipOutcome::NotStarted, ClipOutcome::NotStarted]);
        assert_eq!(*finished.lock().expect("lock"), [3, 4]);
        let served = serve_after_stop(left(), None, &run, &options, &Stop::Cancelled, &on_event);
        assert_eq!(served.len(), 2);
        assert_eq!(
            finished.lock().expect("lock").len(),
            2,
            "no event after a cancel"
        );
    }

    /// A request that does not fit only because of another one in flight waits for it to settle
    /// (for much less than it set aside, as requests do) and then goes ahead; one that could not
    /// fit even alone is over budget at once.
    #[test]
    fn a_reservation_waits_for_the_others_in_flight_instead_of_giving_up() {
        let budget = Budget::new(Some(1.0));
        let cancel = AtomicBool::new(false);
        let first = budget.reserve(0.6).expect("fits");
        assert!(
            matches!(budget.reserve_or_wait(1.5, &cancel), Reserved::OverBudget),
            "too much even alone: no wait"
        );
        assert_eq!(
            budget.refused_usd(),
            Some(1.5),
            "remembered for the message"
        );
        let (sent, got) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let reserved = budget.reserve_or_wait(0.6, &cancel);
                let ok = matches!(&reserved, Reserved::Yes(r) if r.usd() == 0.6);
                sent.send(ok).expect("send");
                if let Reserved::Yes(r) = reserved {
                    r.settle(0.1);
                }
            });
            // Wait until the other thread is really waiting, not a fixed time.
            while budget.waits() == 0 {
                std::thread::yield_now();
            }
            assert!(
                got.try_recv().is_err(),
                "still waiting while 0.6 is in flight"
            );
            first.settle(0.1);
            assert_eq!(got.recv(), Ok(true), "went ahead once the first settled");
        });
        assert!((budget.spent_usd() - 0.2).abs() < 1e-12);

        // The others in flight settle for so much that this no longer fits even alone.
        let big = budget.reserve(0.7).expect("0.2 + 0.7 fits");
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| budget.reserve_or_wait(0.5, &cancel));
            while budget.waits() < 2 {
                std::thread::yield_now();
            }
            big.settle(0.7);
            assert!(matches!(
                waiting.join().expect("join"),
                Reserved::OverBudget
            ));
        });

        // Ctrl+C while waiting.
        let cancel_me = AtomicBool::new(false);
        let held = budget.reserve(0.05).expect("fits");
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| budget.reserve_or_wait(0.08, &cancel_me));
            while budget.waits() < 3 {
                std::thread::yield_now();
            }
            cancel_me.store(true, Ordering::Relaxed);
            assert!(matches!(waiting.join().expect("join"), Reserved::Cancelled));
        });
        drop(held);
        assert!(matches!(
            Budget::new(None).reserve_or_wait(1e9, &cancel),
            Reserved::Yes(_)
        ));
    }

    /// OpenAI bills a frame for more tokens than Anthropic does; the bound follows the provider.
    #[test]
    fn the_image_bound_follows_each_providers_billing() {
        assert_eq!(image_tokens_bound(Provider::Anthropic, 512, 288), 209);
        assert_eq!(image_tokens_bound(Provider::OpenAi, 512, 288), 255);
        assert_eq!(image_tokens_bound(Provider::OpenAi, 288, 512), 255);
        // Two 512 tiles (425) against 16×32 patches × 1.62 (830): the larger.
        assert_eq!(image_tokens_bound(Provider::OpenAi, 1024, 512), 830);
    }

    /// The folder run end to end on real clips, with a local mock server standing in for the API:
    /// resuming, forcing, the budget cap, several clips in flight, and grouping.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    mod runs {
        use super::super::run::{prepare, Runner};
        use super::super::*;
        use crate::anthropic::{Anthropic, RetryPolicy};
        use crate::cache::{cache_path, Cache};
        use crate::describe::{FrameSampling, MomentsMode, SummaryLanguage};
        use crate::provider::{AiProvider, RateGate};
        use crate::{group_clips, Error, Model, Options};
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        /// When a held answer may go.
        type Release = Arc<dyn Fn() -> bool + Send + Sync>;

        /// A local server answering every request with a description, on as many connections at
        /// once as come in, holding each answer up to `wait` for another request to overlap it:
        /// how many requests it got, and the most it had in work at the same time.
        struct Server {
            url: String,
            requests: Arc<AtomicUsize>,
            most_at_once: Arc<AtomicUsize>,
        }

        /// How long the test clients wait for an answer: far longer than any `wait` of
        /// [`server`], so a held answer comes back as an answer (and a run that never overlaps
        /// its requests fails on `most_at_once`, not on a timeout).
        const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);

        fn server(wait: Duration) -> Server {
            server_with(wait, None)
        }

        /// [`server`], holding each answer until `release` says so (or `wait` runs out) instead
        /// of until two requests overlapped.
        fn server_with(wait: Duration, release: Option<Release>) -> Server {
            assert!(
                wait * 2 <= ANSWER_TIMEOUT,
                "a held answer must not time out"
            );
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            let requests = Arc::new(AtomicUsize::new(0));
            let most_at_once = Arc::new(AtomicUsize::new(0));
            let in_work = Arc::new(AtomicUsize::new(0));
            let (count, most) = (requests.clone(), most_at_once.clone());
            std::thread::spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    let (count, most, in_work) = (count.clone(), most.clone(), in_work.clone());
                    let release = release.clone();
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream);
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                                break;
                            }
                            if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                                length = v.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        let _ = reader.read_exact(&mut body);
                        let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                        let now = in_work.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        // Hold the answer until two requests have been in work at the same time
                        // (or `wait` runs out), so a test of overlapping requests does not depend
                        // on how long each clip takes to decode. The condition is the high-water
                        // mark, not the current count: a request that arrives second is released
                        // at once, so polling the current count could miss the moment the two
                        // overlapped, and a request that arrives after the overlap was already
                        // seen (the last clip of a run) has nothing left to wait for.
                        let started = std::time::Instant::now();
                        let released = || match &release {
                            Some(release) => release(),
                            None => most.load(Ordering::SeqCst) >= 2,
                        };
                        while !released() && started.elapsed() < wait {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        in_work.fetch_sub(1, Ordering::SeqCst);
                        let text = format!(
                            r#"{{\"summary\":\"The Earth turns in space, answer {n}.\",\"segments\":[]}}"#
                        );
                        let answer = serde_json::json!({
                            "content": [{"type": "text", "text": text.replace("\\\"", "\"")}],
                            "stop_reason": "end_turn",
                            "usage": {"input_tokens": 3000, "output_tokens": 40},
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                            answer.len()
                        );
                        let _ = reader.get_mut().write_all(response.as_bytes());
                    });
                }
            });
            Server {
                url,
                requests,
                most_at_once,
            }
        }

        fn options() -> Options {
            Options {
                api_key: "k".to_string(),
                model: Model::default(),
                language: SummaryLanguage::English,
                frame_sampling: FrameSampling::KeyFrames,
                moments: MomentsMode::Important,
            }
        }

        /// A folder of copies of the named test clips, and their paths in the order named (not
        /// [`find_videos`]'s sorted order, which would put `file_example_MOV…` before
        /// `rotated-90.mp4`: tests that depend on which clip goes first say so by the order).
        fn folder(name: &str, clips: &[&str]) -> (PathBuf, Vec<PathBuf>) {
            let dir =
                std::env::temp_dir().join(format!("clipscribe-run-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/clips");
            for clip in clips {
                std::fs::copy(repo.join(clip), dir.join(clip)).expect("copy");
            }
            let videos = clips.iter().map(|clip| dir.join(clip)).collect();
            (dir, videos)
        }

        fn run_folder(
            server: &Server,
            videos: &[PathBuf],
            cache: Option<&Cache>,
            run: &RunOptions,
            budget: &Budget,
            cancel: &AtomicBool,
            on_event: &(dyn Fn(FolderEvent<'_>) + Sync),
        ) -> FolderRun {
            let options = options();
            let gate = Arc::new(RateGate::default());
            let url = server.url.clone();
            let make_provider = move || -> Result<Box<dyn AiProvider>, Error> {
                let fast = RetryPolicy {
                    delays: vec![Duration::from_millis(1); 3],
                    rate_limit_wait: Duration::from_millis(1),
                    step: Duration::from_millis(1),
                    max_rate_limit_waits: 20,
                    answer_timeout: ANSWER_TIMEOUT,
                };
                Ok(Box::new(
                    Anthropic::with_endpoint("k".to_string(), url.clone(), fast)
                        .map_err(Error::Ai)?
                        .with_rate_gate(gate.clone()),
                ))
            };
            Runner {
                cache,
                run,
                options: &options,
                budget,
                cancel,
            }
            .go(videos, &make_provider, on_event)
        }

        /// What became of each clip, in a word; a failed clip's error is printed, so an assertion
        /// on the words says why.
        fn kinds(run: &FolderRun) -> Vec<&'static str> {
            run.clips
                .iter()
                .map(|c| match c {
                    ClipOutcome::Described(_) => "described",
                    ClipOutcome::Cached(_) => "cached",
                    ClipOutcome::Failed(e) => {
                        eprintln!("failed: {e:?}");
                        "failed"
                    }
                    ClipOutcome::OverBudget => "over budget",
                    ClipOutcome::NotStarted => "not started",
                })
                .collect()
        }

        /// The issue's "Done when": a run stopped part way is resumed without redoing the clips it
        /// finished; a run with everything cached sends nothing; `force` redoes everything and
        /// keeps the cache fresh.
        #[test]
        fn a_stopped_run_resumes_without_redoing_finished_clips() {
            let (dir, videos) = folder(
                "resume",
                &[
                    "rotated-90.mp4",
                    "file_example_MOV_480_700kB.mov",
                    "file_example_MP4_480_1_5MG.mp4",
                ],
            );
            let server = server(Duration::ZERO);
            let path = cache_path(&dir, None);
            let one_at_a_time = RunOptions {
                jobs: 1,
                ..RunOptions::default()
            };

            // Ctrl+C once the first clip is done.
            let cancel = AtomicBool::new(false);
            let cache = Cache::open(&path).expect("cache");
            let first = run_folder(
                &server,
                &videos,
                Some(&cache),
                &one_at_a_time,
                &Budget::new(None),
                &cancel,
                &|event| {
                    if matches!(event, FolderEvent::Finished { index: 0, .. }) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                },
            );
            assert_eq!(kinds(&first), ["described", "not started", "not started"]);
            assert_eq!(first.stopped, Some(Stop::Cancelled));
            assert_eq!(server.requests.load(Ordering::SeqCst), 1);
            assert_eq!(first.usage.input_tokens, 3000);
            drop(cache);

            // A new process: the cache is read back from disk.
            let cache = Cache::open(&path).expect("reopen");
            assert_eq!(cache.len(), 1);
            let second = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions::default(),
                &Budget::new(None),
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&second), ["cached", "described", "described"]);
            assert_eq!(second.stopped, None);
            assert_eq!(
                server.requests.load(Ordering::SeqCst),
                3,
                "only the two left"
            );
            assert_eq!(
                second.usage.input_tokens, 6000,
                "the cached clip cost nothing"
            );
            let first_record = first.clips[0].record().expect("record");
            assert_eq!(
                second.clips[0].record(),
                Some(first_record),
                "the same description"
            );

            let third = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions::default(),
                &Budget::new(None),
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&third), ["cached", "cached", "cached"]);
            assert_eq!(server.requests.load(Ordering::SeqCst), 3, "nothing sent");

            let forced = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions {
                    force: true,
                    ..RunOptions::default()
                },
                &Budget::new(None),
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&forced), ["described", "described", "described"]);
            assert_eq!(server.requests.load(Ordering::SeqCst), 6);
            drop(cache);
            let cache = Cache::open(&path).expect("reopen");
            assert_eq!(cache.len(), 3, "replaced, not added");
            let newest = forced.clips[0].record().expect("record");
            assert_eq!(
                cache.get(&newest.key).as_ref(),
                Some(newest),
                "the fresh result"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// What the budget reserves for `video`: the bound of the very request the run sends.
        fn bound(video: &Path) -> f64 {
            let options = options();
            let prepared = prepare(
                video,
                &[],
                None,
                &options,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .expect("prepared");
            request_cost_bound(options.model, &prepared.request)
        }

        /// What the mock server's answer costs (its 3,000 in / 40 out tokens).
        fn mock_cost() -> f64 {
            options().model.cost_usd(AiUsage {
                input_tokens: 3000,
                output_tokens: 40,
            })
        }

        /// A cap below one clip's bound: its frames are read, but nothing is sent, nothing is
        /// cached, and no other clip is started.
        #[test]
        fn the_budget_cap_stops_before_sending_what_it_cannot_pay_for() {
            let (dir, videos) = folder(
                "budget",
                &["rotated-90.mp4", "file_example_MOV_480_700kB.mov"],
            );
            let bounds: Vec<f64> = videos.iter().map(|v| bound(v)).collect();
            let server = server(Duration::ZERO);
            let cache = Cache::open(&cache_path(&dir, None)).expect("cache");
            let budget = Budget::new(Some(bounds[0] * 0.9));
            let run = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions {
                    jobs: 1,
                    ..RunOptions::default()
                },
                &budget,
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&run), ["over budget", "not started"]);
            assert_eq!(run.stopped, Some(Stop::OverBudget));
            assert_eq!(server.requests.load(Ordering::SeqCst), 0);
            assert_eq!(budget.spent_usd(), 0.0);
            assert_eq!(budget.refused_usd(), Some(bounds[0]), "what did not fit");
            assert!(cache.is_empty());

            // Room for the first clip, and then not for the second's bound on top of what the
            // first really cost.
            let cap = mock_cost() + bounds[1] - 1e-6;
            assert!(bounds[0] <= cap, "the first fits: {bounds:?} in {cap}");
            let one_clip = Budget::new(Some(cap));
            let run = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions {
                    jobs: 1,
                    ..RunOptions::default()
                },
                &one_clip,
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&run), ["described", "over budget"]);
            assert_eq!(server.requests.load(Ordering::SeqCst), 1);
            assert!(one_clip.spent_usd() <= cap);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An answer that came back but could not be read may have been billed, and the error
        /// does not say for how much: like a timeout, its whole bound counts as spent, so a
        /// run of them cannot go past the cap uncounted.
        #[test]
        fn an_unreadable_answer_counts_its_bound_as_spent() {
            struct Unreadable;
            impl AiProvider for Unreadable {
                fn complete(
                    &self,
                    _: &crate::provider::AiRequest,
                    _: &AtomicBool,
                ) -> Result<crate::provider::AiResponse, crate::AiError> {
                    Err(crate::AiError::BadAnswer("not JSON: {".to_string()))
                }
            }
            let (dir, videos) = folder("bad-answer", &["rotated-90.mp4"]);
            let budget = Budget::new(None);
            let run = Runner {
                cache: None,
                run: &RunOptions::default(),
                options: &options(),
                budget: &budget,
                cancel: &AtomicBool::new(false),
            }
            .go(
                &videos,
                &|| -> Result<Box<dyn AiProvider>, Error> { Ok(Box::new(Unreadable)) },
                &|_| {},
            );
            assert_eq!(kinds(&run), ["failed"]);
            assert!(
                (budget.spent_usd() - bound(&videos[0])).abs() < 1e-12,
                "{} spent",
                budget.spent_usd()
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// With several clips in flight, a clip whose bound does not fit next to the others' open
        /// reservations waits for them to settle instead of stopping the run: a cap with room for
        /// every clip's real cost, but for only one bound at a time, describes them all, one after
        /// the other, and never passes the cap.
        #[test]
        fn concurrent_reservations_wait_instead_of_stopping_the_run() {
            let (_dir, videos) = folder(
                "budget-jobs",
                &[
                    "rotated-90.mp4",
                    "file_example_MOV_480_700kB.mov",
                    "Short.Travel.Health.Views.file_example_WEBM_480_900KB.webm",
                ],
            );
            let bounds: Vec<f64> = videos.iter().map(|v| bound(v)).collect();
            let largest = bounds.iter().copied().fold(0.0, f64::max);
            let smallest = bounds.iter().copied().fold(f64::INFINITY, f64::min);
            let cap = 2.0 * mock_cost() + largest + 1e-6;
            assert!(
                smallest + largest > cap,
                "two bounds at once must not fit, or nothing would wait: {bounds:?} in {cap}"
            );
            let budget = Arc::new(Budget::new(Some(cap)));
            // Hold the first answer until another clip is waiting for the budget, so the test
            // does not depend on how fast each clip decodes: the wait really happens.
            let waiting = budget.clone();
            let server = server_with(
                Duration::from_secs(60),
                Some(Arc::new(move || waiting.waits() > 0)),
            );
            let run = run_folder(
                &server,
                &videos,
                None,
                &RunOptions {
                    jobs: 3,
                    ..RunOptions::default()
                },
                &budget,
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&run), ["described", "described", "described"]);
            assert_eq!(run.stopped, None);
            assert!(budget.waits() > 0, "a clip waited for the budget");
            assert_eq!(
                server.most_at_once.load(Ordering::SeqCst),
                1,
                "one bound at a time"
            );
            assert!(budget.spent_usd() <= cap, "{} > {cap}", budget.spent_usd());
            assert!((budget.spent_usd() - 3.0 * mock_cost()).abs() < 1e-9);
        }

        /// Once a run stops (here for the budget), the clips it did not get to are still served
        /// from the cache when it has them — they cost nothing — and only the others are left not
        /// started.
        #[test]
        fn a_stopped_run_still_serves_what_the_cache_has() {
            let (dir, videos) = folder(
                "stopped-cache",
                &[
                    "rotated-90.mp4",
                    "file_example_MOV_480_700kB.mov",
                    "file_example_MP4_480_1_5MG.mp4",
                ],
            );
            let server = server(Duration::ZERO);
            let cache = Cache::open(&cache_path(&dir, None)).expect("cache");
            let earlier = run_folder(
                &server,
                &videos[2..],
                Some(&cache),
                &RunOptions::default(),
                &Budget::new(None),
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&earlier), ["described"]);

            let finished = std::sync::Mutex::new(Vec::new());
            let run = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions {
                    jobs: 1,
                    ..RunOptions::default()
                },
                &Budget::new(Some(bound(&videos[0]) * 0.9)),
                &AtomicBool::new(false),
                &|event| {
                    if let FolderEvent::Finished { index, .. } = event {
                        finished.lock().expect("lock").push(index);
                    }
                },
            );
            assert_eq!(kinds(&run), ["over budget", "not started", "cached"]);
            assert_eq!(run.stopped, Some(Stop::OverBudget));
            assert_eq!(
                server.requests.load(Ordering::SeqCst),
                1,
                "nothing new sent"
            );
            assert_eq!(run.clips[2].record(), earlier.clips[0].record());
            assert_eq!(
                *finished.lock().expect("lock"),
                [0, 1, 2],
                "every clip is finished, in order, so a display can move past the unstarted one"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Ctrl+C while an answer is on its way does not throw it away: it was billed, so it is
        /// cached and counted; nothing new is started.
        #[test]
        fn an_answer_in_flight_when_cancelled_is_still_kept() {
            let (dir, videos) = folder(
                "cancel-in-flight",
                &["rotated-90.mp4", "file_example_MOV_480_700kB.mov"],
            );
            let cancel = Arc::new(AtomicBool::new(false));
            // Hold the answer until the cancel is set.
            let cancelled = cancel.clone();
            let server = server_with(
                Duration::from_secs(60),
                Some(Arc::new(move || cancelled.load(Ordering::SeqCst))),
            );
            let requests = server.requests.clone();
            let flag = cancel.clone();
            std::thread::spawn(move || {
                while requests.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                flag.store(true, Ordering::SeqCst);
            });
            let cache = Cache::open(&cache_path(&dir, None)).expect("cache");
            let run = run_folder(
                &server,
                &videos,
                Some(&cache),
                &RunOptions {
                    jobs: 1,
                    ..RunOptions::default()
                },
                &Budget::new(None),
                &cancel,
                &|_| {},
            );
            assert_eq!(kinds(&run), ["described", "not started"]);
            assert_eq!(run.stopped, Some(Stop::Cancelled));
            assert_eq!(cache.len(), 1, "the billed answer is cached");
            assert_eq!(run.usage.input_tokens, 3000, "and counted");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn several_clips_are_in_flight_at_once() {
            let (dir, videos) = folder(
                "jobs",
                &[
                    "rotated-90.mp4",
                    "file_example_MOV_480_700kB.mov",
                    "Short.Travel.Health.Views.file_example_WEBM_480_900KB.webm",
                ],
            );
            // Generous: clips decode in parallel on a slow, shared CI machine before their
            // requests can overlap.
            let server = server(Duration::from_secs(60));
            let run = run_folder(
                &server,
                &videos,
                None,
                &RunOptions {
                    jobs: 3,
                    ..RunOptions::default()
                },
                &Budget::new(None),
                &AtomicBool::new(false),
                &|_| {},
            );
            assert_eq!(kinds(&run), ["described", "described", "described"]);
            assert!(
                server.most_at_once.load(Ordering::SeqCst) >= 2,
                "requests overlapped: {}",
                server.most_at_once.load(Ordering::SeqCst)
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A 6 s clip made in-process with GStreamer (no `gst-launch-1.0` needed): `source` is
        /// the start of a pipeline description giving raw video, scaled here to 320×180 and
        /// written as Motion-JPEG in Matroska to `dir/<name>.mkv`.
        fn render(dir: &Path, name: &str, source: &str) -> PathBuf {
            use gstreamer as gst;
            use gstreamer::prelude::*;
            gst::init().expect("GStreamer");
            let path = dir.join(format!("{name}.mkv"));
            let description = format!(
                "{source} ! videoconvert ! videoscale ! video/x-raw,width=320,height=180 ! \
                 jpegenc ! matroskamux ! filesink location=\"{}\"",
                path.display()
            );
            let pipeline = gst::parse::launch(&description).expect("pipeline");
            pipeline.set_state(gst::State::Playing).expect("playing");
            let bus = pipeline.bus().expect("bus");
            let mut done = false;
            for message in bus.iter_timed(gst::ClockTime::from_seconds(120)) {
                match message.view() {
                    gst::MessageView::Eos(_) => {
                        done = true;
                        break;
                    }
                    gst::MessageView::Error(e) => panic!("{name}: {} ({:?})", e.error(), e.debug()),
                    _ => {}
                }
            }
            let _ = pipeline.set_state(gst::State::Null);
            assert!(done, "{name}: not finished in time");
            path
        }

        /// A `videotestsrc` pattern, 6 s: a scene unlike the test clips.
        fn pattern(dir: &Path, pattern: &str) -> PathBuf {
            render(
                dir,
                &format!("pattern-{pattern}"),
                &format!(
                    "videotestsrc num-buffers=60 pattern={pattern} ! video/x-raw,framerate=10/1"
                ),
            )
        }

        /// The first 6 s of the MP4 test clip, cropped by `crop` (`left right top bottom`, in
        /// pixels of its 480×270 frame) and scaled back up: the same scene framed differently.
        fn reframed(dir: &Path, name: &str, [left, right, top, bottom]: [u32; 4]) -> PathBuf {
            let clip = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/clips/file_example_MP4_480_1_5MG.mp4");
            render(
                dir,
                name,
                &format!(
                    "filesrc location=\"{}\" ! decodebin ! videoconvert ! videorate ! \
                     video/x-raw,framerate=10/1 ! identity eos-after=60 ! \
                     videocrop left={left} right={right} top={top} bottom={bottom}",
                    clip.display()
                ),
            )
        }

        /// What the model might write for each clip of the grouping tests: written for the tests
        /// (the mock server answers every clip with the same sentence, and no live request is
        /// made), in its usual style — the same footage described in different words each time,
        /// the way two requests describe it.
        fn description_of(name: &str) -> &'static str {
            match name {
                "MP4" => "The Earth turns slowly in space at night, city lights glowing across the continents.",
                "MOV" => "A night view of the Earth from space, its cities glowing as the planet rotates.",
                "WebM" => "The planet rotates in the dark, city lights glowing across the Earth seen from space.",
                "rotated-90" => "The Earth, seen from space at night, rotates slowly with its city lights glowing.",
                "zoom 1.25x" => "Close view of the night side of the Earth from space, city lights glowing as it turns.",
                "pan 20%" => "Part of the Earth at night seen from space, with glowing city lights, turning slowly.",
                "pan 40%" => "The edge of the Earth from space at night, lights of cities glowing as it slowly rotates.",
                "smpte" => "Vertical colour bars of a television test pattern fill the screen.",
                "ball" => "A white ball bounces around on a black background.",
                "gradient" => "A smooth grey gradient runs from dark to light across the picture.",
                "pinwheel" => "Black and white pinwheel blades spin around the centre.",
                "circular" => "Concentric black and white rings spread out from the centre.",
                _ => panic!("no description for {name}"),
            }
        }

        /// `clip` with `summary` as its description instead of the mock server's.
        fn described_as(clip: &DescribedClip, summary: &str) -> DescribedClip {
            DescribedClip {
                description: crate::Description {
                    summary: summary.to_string(),
                    segments: Vec::new(),
                },
                ..clip.clone()
            }
        }

        /// Grouping on the real test clips: all four are the same footage (the MP4, the MOV and
        /// the WebM one video in three containers, `rotated-90.mp4` its first 6 s stored sideways),
        /// so one group; generated test patterns are other scenes and get groups of their own.
        /// Both halves always run (the patterns are made in-process).
        #[test]
        fn the_test_clips_are_one_group_and_other_scenes_are_not() {
            let names = [
                "MP4",
                "MOV",
                "WebM",
                "rotated-90",
                "smpte",
                "ball",
                "gradient",
            ];
            let (dir, mut videos) = folder(
                "groups",
                &[
                    "file_example_MP4_480_1_5MG.mp4",
                    "file_example_MOV_480_700kB.mov",
                    "Short.Travel.Health.Views.file_example_WEBM_480_900KB.webm",
                    "rotated-90.mp4",
                ],
            );
            let patterns: Vec<PathBuf> = names[4..].iter().map(|p| pattern(&dir, p)).collect();
            videos.extend(patterns.iter().cloned());
            let server = server(Duration::ZERO);
            let run = run_folder(
                &server,
                &videos,
                None,
                &RunOptions::default(),
                &Budget::new(None),
                &AtomicBool::new(false),
                &|_| {},
            );
            let clips: Vec<DescribedClip> = run
                .clips
                .iter()
                .zip(names)
                .map(|(c, name)| {
                    described_as(&c.record().expect("described").clip, description_of(name))
                })
                .collect();
            let grouping = group_clips(&clips.iter().collect::<Vec<_>>());
            for (video, groups) in videos.iter().zip(&grouping.clips) {
                eprintln!(
                    "{}: group {} {:?}",
                    video.file_name().unwrap_or_default().to_string_lossy(),
                    groups.group,
                    groups
                        .stretches
                        .iter()
                        .map(|s| (s.start_s, s.end_s, s.group))
                        .collect::<Vec<_>>()
                );
            }
            let ids: Vec<usize> = grouping.clips.iter().map(|c| c.group).collect();
            assert_eq!(&ids[..4], &[1, 1, 1, 1], "the same footage: {ids:?}");
            for clip in &grouping.clips[..4] {
                assert!(clip.stretches.iter().all(|s| s.group == 1), "{clip:?}");
            }
            let mut pattern_ids = ids[4..].to_vec();
            pattern_ids.sort_unstable();
            pattern_ids.dedup();
            assert_eq!(pattern_ids.len(), patterns.len(), "a group each: {ids:?}");
            assert!(ids[4..].iter().all(|&id| id != 1), "{ids:?}");
            assert_eq!(grouping.groups[0].stretches, 4);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The measurement behind the grouping thresholds (`docs/design/whole-folders.md`,
        /// "Measured"): both distances `group_clips` joins clips by — the pictures' (`1 − r`)
        /// and the descriptions' words (Jaccard) — between the test clips, the test clip
        /// re-framed (cropped and scaled, as a camera moved or zoomed would), and generated
        /// patterns, each described as [`description_of`] says. Prints the table (`cargo test
        /// --lib grouping_distances -- --nocapture`) and checks what the design relies on.
        #[test]
        fn grouping_distances_on_the_test_clips() {
            use crate::groups::{text_similarity, SAME_SHOT, SAME_TEXT};
            let dir = std::env::temp_dir()
                .join(format!("clipscribe-run-distances-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/clips");
            let mut named: Vec<(&str, PathBuf)> = vec![
                ("MP4", repo.join("file_example_MP4_480_1_5MG.mp4")),
                ("MOV", repo.join("file_example_MOV_480_700kB.mov")),
                (
                    "WebM",
                    repo.join("Short.Travel.Health.Views.file_example_WEBM_480_900KB.webm"),
                ),
                ("rotated-90", repo.join("rotated-90.mp4")),
                ("zoom 1.25x", reframed(&dir, "zoom", [48, 48, 27, 27])),
                ("pan 20%", reframed(&dir, "pan", [96, 0, 0, 0])),
                ("pan 40%", reframed(&dir, "pan-far", [192, 0, 0, 0])),
            ];
            for p in ["smpte", "ball", "gradient", "pinwheel", "circular"] {
                named.push((p, pattern(&dir, p)));
            }
            let options = options();
            let clips: Vec<DescribedClip> = named
                .iter()
                .map(|(name, path)| {
                    let prepared = prepare(
                        path,
                        &[],
                        None,
                        &options,
                        &AtomicBool::new(false),
                        &mut |_| {},
                    )
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                    DescribedClip {
                        description: crate::Description {
                            summary: description_of(name).to_string(),
                            segments: Vec::new(),
                        },
                        tags: None,
                        usage: AiUsage::default(),
                        duration_s: prepared.duration_s,
                        frames: prepared.fingerprints,
                    }
                })
                .collect();
            let picture = |a: usize, b: usize| {
                crate::groups::clip_distance(&clips[a], &clips[b]).unwrap_or(f64::NAN)
            };
            let text = |a: usize, b: usize| {
                text_similarity(&clips[a].description.summary, &clips[b].description.summary)
            };
            let by = |a: usize, b: usize| match (picture(a, b) < SAME_SHOT, text(a, b) >= SAME_TEXT)
            {
                (true, true) => "both",
                (true, false) => "picture",
                (false, true) => "words",
                (false, false) => "no",
            };
            eprintln!(
                "from the MP4: picture (1 - r, best rotation, closest stretches), words \
                 (Jaccard), grouped by (picture < {SAME_SHOT}, words >= {SAME_TEXT}):"
            );
            for (i, (name, _)) in named.iter().enumerate().skip(1) {
                eprintln!(
                    "  {name:<12} {:.3}  {:.2}  {}",
                    picture(0, i),
                    text(0, i),
                    by(0, i)
                );
            }
            let patterns = 7..named.len();
            eprintln!("between patterns:");
            let mut closest_picture = f64::INFINITY;
            for i in patterns.clone() {
                for j in patterns.clone().filter(|&j| j > i) {
                    closest_picture = closest_picture.min(picture(i, j));
                    eprintln!(
                        "  {:<9} {:<9} {:.3}  {:.2}  {}",
                        named[i].0,
                        named[j].0,
                        picture(i, j),
                        text(i, j),
                        by(i, j)
                    );
                }
            }
            let mock = |n: usize| format!("The Earth turns in space, answer {n}.");
            eprintln!(
                "the mock server's own answers, one clip to another: words {:.2}",
                text_similarity(&mock(1), &mock(2))
            );
            for (i, (name, _)) in named.iter().enumerate().take(4).skip(1) {
                assert!(picture(0, i) < SAME_SHOT, "{name}: {}", picture(0, i));
            }
            for i in patterns.clone() {
                assert!(
                    picture(0, i) > SAME_SHOT,
                    "{}: {}",
                    named[i].0,
                    picture(0, i)
                );
                assert!(text(0, i) < SAME_TEXT, "{}: {}", named[i].0, text(0, i));
            }
            assert!(closest_picture > SAME_SHOT, "{closest_picture}");
            // The hybrid end to end: the re-framed footage joins the originals by its words, and
            // no pattern joins them. (Between patterns the words can join two different ones —
            // the pinwheel and the rings are both "black and white" around "the centre": the
            // word signal's false positive, printed above and in the design notes.)
            let grouping = group_clips(&clips.iter().collect::<Vec<_>>());
            let ids: Vec<usize> = grouping.clips.iter().map(|c| c.group).collect();
            eprintln!("groups: {ids:?}");
            assert!(ids[..7].iter().all(|&id| id == 1), "{ids:?}");
            assert!(ids[7..].iter().all(|&id| id != 1), "{ids:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
