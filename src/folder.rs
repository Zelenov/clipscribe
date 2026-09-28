//! Whole folders: finding the videos, one clip described with what grouping needs
//! ([`describe_clip`]), a budget cap ([`Budget`]), and a run over many clips with several in flight
//! that skips what the cache already has ([`describe_folder`]). See the design notes in
//! `docs/design/whole-folders.md`.
//!
//! The building blocks are public on their own so a program with its own progress display
//! (frename) can drive them: [`describe_folder`] with its [`FolderEvent`]s, or its own loop over
//! [`crate::CacheKey`], [`crate::Cache`], [`describe_clip`] and [`Budget`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::describe::{frame_tokens, Description, Model, FRAME_LONG_SIDE};
use crate::provider::{AiContent, AiRequest, AiUsage};
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
pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The fingerprint of one frame sent to the model: an 8×8 grid of average luma (64 bytes, rows
/// packed) of the upright frame, what [`crate::group_clips`] compares.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameFingerprint {
    pub time_s: f64,
    pub fingerprint: Vec<u8>,
}

/// A clip described by [`describe_clip`]: what [`crate::describe`] (or
/// [`crate::describe_with_tags`]) gives, plus the fingerprints of the frames it was described
/// from, for grouping.
#[derive(Debug, Clone, PartialEq)]
pub struct DescribedClip {
    pub description: Description,
    /// Tag suggestions, when a vocabulary was given.
    pub tags: Option<TagSuggestions>,
    /// What the request was billed for; see [`Model::cost_usd`].
    pub usage: AiUsage,
    pub duration_s: f64,
    /// One per frame sent, in time order.
    pub frames: Vec<FrameFingerprint>,
}

/// A spending cap shared by the requests of a run: what has been spent, plus what the requests in
/// flight could still cost. See [`Budget::reserve`].
#[derive(Debug, Default)]
pub struct Budget {
    max_usd: Option<f64>,
    state: Mutex<Spending>,
}

#[derive(Debug, Default)]
struct Spending {
    spent: f64,
    reserved: f64,
}

impl Budget {
    /// A budget of at most `max_usd` US dollars; `None` is no cap (spending is still counted).
    pub fn new(max_usd: Option<f64>) -> Self {
        Self {
            max_usd,
            state: Mutex::default(),
        }
    }

    pub fn max_usd(&self) -> Option<f64> {
        self.max_usd
    }

    /// Spent so far, in US dollars (reservations not settled yet left out).
    pub fn spent_usd(&self) -> f64 {
        self.lock().spent
    }

    /// Set aside `usd` for a request about to be sent, or `None` when what is spent, what is set
    /// aside for other requests and `usd` together would pass the cap: then the request must not be
    /// sent. The reservation is released when dropped, or replaced by what the request really cost
    /// with [`Reservation::settle`].
    pub fn reserve(&self, usd: f64) -> Option<Reservation<'_>> {
        let usd = usd.max(0.0);
        let mut state = self.lock();
        if let Some(max) = self.max_usd {
            if state.spent + state.reserved + usd > max {
                return None;
            }
        }
        state.reserved += usd;
        Some(Reservation {
            budget: self,
            usd,
            settled: false,
        })
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
    /// What was set aside.
    pub fn usd(&self) -> f64 {
        self.usd
    }

    /// The request cost `actual_usd`: count that as spent instead of the reservation.
    pub fn settle(mut self, actual_usd: f64) {
        let mut state = self.budget.lock();
        state.reserved = (state.reserved - self.usd).max(0.0);
        state.spent += actual_usd.max(0.0);
        self.settled = true;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.settled {
            let mut state = self.budget.lock();
            state.reserved = (state.reserved - self.usd).max(0.0);
        }
    }
}

/// The most `request` can cost with `model`, in US dollars, for [`Budget::reserve`]: every image
/// at its real size ([`frame_tokens`]; a JPEG whose size cannot be read counts as a
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
                frame_tokens(w, h) as f64
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
    /// Its frames were read, but sending the request could have passed the budget: nothing was
    /// sent, and the run stopped starting new clips.
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
    /// The next request could have passed the budget.
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
    /// Done with it; the same outcome [`FolderRun::clips`] will hold.
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
            Self::OverBudget => f.write_str("Stopped: the next clip could pass the budget."),
            Self::Job(summary) => f.write_str(summary),
        }
    }
}

#[cfg(feature = "frames")]
pub use run::{describe_clip, describe_folder};

#[cfg(feature = "frames")]
mod run {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::{
        request_cost_bound, Budget, ClipOutcome, DescribedClip, FolderEvent, FolderRun,
        FrameFingerprint, RunOptions, Stop,
    };
    use crate::cache::{Cache, CacheKey, ClipRecord};
    use crate::describe::{build_request, parse_answer};
    use crate::provider::{AiError, AiProvider, AiUsage, RateGate};
    use crate::tags::{build_combined_request, parse_combined_answer, Tag};
    use crate::{srt, Cue, Error, Options, Stage};

    /// Describe the clip at `video` like [`crate::describe`] (or [`crate::describe_with_tags`]
    /// when `vocabulary` is given), keeping its frames' fingerprints for [`crate::group_clips`],
    /// within `budget`: once its frames are read, the request's upper bound
    /// ([`super::request_cost_bound`]) is reserved first, and `Ok(None)` means it did not fit, so
    /// nothing was sent. A timeout counts its whole bound as spent (it may have been billed); any
    /// other failure spends nothing but a bad answer's usage.
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
            let (duration_s, frames, fingerprints) =
                crate::read_frames(video, options.frame_sampling, self.cancel, &mut on_stage)?;
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
            let bound = request_cost_bound(options.model, &request);
            let Some(reservation) = self.budget.reserve(bound) else {
                return Ok(None);
            };
            on_stage(Stage::Asking);
            let response = match self.provider.complete(&request, self.cancel) {
                Ok(response) => response,
                Err(AiError::Timeout) => {
                    reservation.settle(bound);
                    return Err(Error::Ai(AiError::Timeout));
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
                frames: frames
                    .iter()
                    .zip(fingerprints)
                    .map(|(frame, fingerprint)| FrameFingerprint {
                        time_s: frame.time_s,
                        fingerprint,
                    })
                    .collect(),
            }))
        }
    }

    /// Describe `videos` with up to `run.jobs` clips in flight, skipping those `cache` already
    /// has (unless `run.force`) and adding every newly described one to it, within `budget`.
    ///
    /// Each clip is looked up by its [`CacheKey`] (the file's identity and how it is described).
    /// A new result is written to the cache as soon as it is in, so a crash or a cancel loses at
    /// most the clips in flight. Every worker's client shares one pause: a 429 on one holds the
    /// others back too. The run stops starting new clips when `cancel` is set, when the next
    /// request could pass `budget` ([`ClipOutcome::OverBudget`]) or when an error would fail
    /// every clip the same way (a rejected key, no credit); clips already in flight finish.
    /// `on_event` follows along from the worker threads. Blocks until done.
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
            let outcomes: Vec<Mutex<ClipOutcome>> = videos
                .iter()
                .map(|_| Mutex::new(ClipOutcome::NotStarted))
                .collect();
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
                            if self.cancel.load(Ordering::Relaxed) || halt.load(Ordering::Relaxed)
                            {
                                break;
                            }
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(video) = videos.get(index) else {
                                break;
                            };
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
                            *outcomes[index].lock().unwrap_or_else(|e| e.into_inner()) = outcome;
                        }
                    });
                }
            });
            if self.cancel.load(Ordering::Relaxed) {
                stop(Stop::Cancelled);
            }
            let clips: Vec<ClipOutcome> = outcomes
                .into_iter()
                .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
                .collect();
            let stopped = stopped.into_inner().unwrap_or_else(|e| e.into_inner());
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
        assert_eq!(
            Stop::OverBudget.to_string(),
            "Stopped: the next clip could pass the budget."
        );
    }

    /// The folder run end to end on real clips, with a local mock server standing in for the API:
    /// resuming, forcing, the budget cap, several clips in flight, and grouping.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    mod runs {
        use super::super::run::Runner;
        use super::super::*;
        use crate::anthropic::{Anthropic, RetryPolicy};
        use crate::cache::{cache_path, Cache};
        use crate::describe::{FrameSampling, MomentsMode, SummaryLanguage};
        use crate::provider::{AiProvider, RateGate};
        use crate::{group_clips, Error, Model, Options};
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        /// A local server answering every request with a description, on as many connections at
        /// once as come in, holding each answer up to `wait` for another request to overlap it:
        /// how many requests it got, and the most it had in work at the same time.
        struct Server {
            url: String,
            requests: Arc<AtomicUsize>,
            most_at_once: Arc<AtomicUsize>,
        }

        fn server(wait: Duration) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            let requests = Arc::new(AtomicUsize::new(0));
            let most_at_once = Arc::new(AtomicUsize::new(0));
            let in_work = Arc::new(AtomicUsize::new(0));
            let (count, most) = (requests.clone(), most_at_once.clone());
            std::thread::spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    let (count, most, in_work) = (count.clone(), most.clone(), in_work.clone());
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
                        // Hold the answer until a second request is in work too (or `wait`
                        // runs out), so a test of overlapping requests does not depend on how
                        // long each clip takes to decode.
                        let started = std::time::Instant::now();
                        while in_work.load(Ordering::SeqCst) < 2 && started.elapsed() < wait {
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

        /// A folder of copies of the named test clips.
        fn folder(name: &str, clips: &[&str]) -> (PathBuf, Vec<PathBuf>) {
            let dir =
                std::env::temp_dir().join(format!("clipscribe-run-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("dir");
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/clips");
            for clip in clips {
                std::fs::copy(repo.join(clip), dir.join(clip)).expect("copy");
            }
            let videos = find_videos(std::slice::from_ref(&dir)).expect("videos");
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
                    answer_timeout: Duration::from_secs(20),
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

        fn kinds(run: &FolderRun) -> Vec<&'static str> {
            run.clips
                .iter()
                .map(|c| match c {
                    ClipOutcome::Described(_) => "described",
                    ClipOutcome::Cached(_) => "cached",
                    ClipOutcome::Failed(_) => "failed",
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
                &["rotated-90.mp4", "file_example_MOV_480_700kB.mov", "file_example_MP4_480_1_5MG.mp4"],
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
            assert_eq!(server.requests.load(Ordering::SeqCst), 3, "only the two left");
            assert_eq!(second.usage.input_tokens, 6000, "the cached clip cost nothing");
            let first_record = first.clips[0].record().expect("record");
            assert_eq!(second.clips[0].record(), Some(first_record), "the same description");

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
            assert_eq!(cache.get(&newest.key).as_ref(), Some(newest), "the fresh result");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A cap below one clip's bound: its frames are read, but nothing is sent, nothing is
        /// cached, and no other clip is started.
        #[test]
        fn the_budget_cap_stops_before_sending_what_it_cannot_pay_for() {
            let (dir, videos) = folder(
                "budget",
                &["rotated-90.mp4", "file_example_MOV_480_700kB.mov"],
            );
            let server = server(Duration::ZERO);
            let cache = Cache::open(&cache_path(&dir, None)).expect("cache");
            let budget = Budget::new(Some(0.001));
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
            assert!(cache.is_empty());

            // Room for one clip: the first (rotated-90.mp4, 3 frames) reserves a bound of about
            // $0.022 and costs $0.0032 (the mock's 3,000 in / 40 out tokens); the second (the
            // MOV, 15 frames) would reserve about $0.024 more, past $0.025.
            let one_clip = Budget::new(Some(0.025));
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
            assert!(one_clip.spent_usd() <= 0.025);
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
            let server = server(Duration::from_secs(20));
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

        /// A clip made with `gst-launch-1.0 videotestsrc pattern=<pattern>`, or `None` when the
        /// tool is not installed.
        fn test_pattern(dir: &std::path::Path, pattern: &str) -> Option<PathBuf> {
            let path = dir.join(format!("pattern-{pattern}.mkv"));
            let made = std::process::Command::new("gst-launch-1.0")
                .args(["-q", "videotestsrc", "num-buffers=60"])
                .arg(format!("pattern={pattern}"))
                .args([
                    "!",
                    "video/x-raw,framerate=10/1,width=320,height=180",
                    "!",
                    "jpegenc",
                    "!",
                    "matroskamux",
                    "!",
                    "filesink",
                ])
                .arg(format!("location={}", path.display()))
                .status();
            made.is_ok_and(|s| s.success()).then_some(path)
        }

        /// Grouping on the real test clips: all four are the same footage (the MP4, the MOV and
        /// the WebM one video in three containers, `rotated-90.mp4` its first 6 s stored sideways),
        /// so one group; generated test patterns, when `gst-launch-1.0` is there to make them, are
        /// other scenes and get groups of their own.
        #[test]
        fn the_test_clips_are_one_group_and_other_scenes_are_not() {
            let (dir, mut videos) = folder(
                "groups",
                &[
                    "file_example_MP4_480_1_5MG.mp4",
                    "file_example_MOV_480_700kB.mov",
                    "Short.Travel.Health.Views.file_example_WEBM_480_900KB.webm",
                    "rotated-90.mp4",
                ],
            );
            let patterns: Vec<PathBuf> = ["smpte", "ball"]
                .iter()
                .filter_map(|p| test_pattern(&dir, p))
                .collect();
            if patterns.is_empty() {
                eprintln!("gst-launch-1.0 could not make the test patterns: only the clips checked");
            }
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
            let clips: Vec<&DescribedClip> = run
                .clips
                .iter()
                .map(|c| &c.record().expect("described").clip)
                .collect();
            let grouping = group_clips(&clips);
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
            pattern_ids.dedup();
            assert_eq!(pattern_ids.len(), patterns.len(), "a group each: {ids:?}");
            assert!(ids[4..].iter().all(|&id| id != 1), "{ids:?}");
            assert_eq!(grouping.groups[0].stretches, 4);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
