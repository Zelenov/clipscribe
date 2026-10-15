//! Describe what happens in a video clip, and when, with Claude.
//!
//! In: a video file, its subtitles if it has any, and [`Options`] (API key, model, language,
//! frame sampling, moments mode). Out: a [`Description`] (a one-sentence summary and, by default,
//! only the moments worth an editor's attention — see [`MomentsMode`]) and what the request cost.
//! Frames are key frames by default (at most 60, 512 px, JPEG, in memory only), one per
//! equal-sized window of the clip where the picture changes the most (see [`FrameSampling`]), and
//! sent with the subtitles in one request; see [`build_request`] for the prompt.
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use clipscribe::{describe, FrameSampling, MomentsMode, Options, SummaryLanguage, MODELS};
//!
//! let options = Options {
//!     api_key: std::env::var("ANTHROPIC_API_KEY").unwrap(),
//!     model: MODELS[0],
//!     language: SummaryLanguage::English,
//!     frame_sampling: FrameSampling::KeyFrames,
//!     moments: MomentsMode::Important,
//!     instructions: None,
//! };
//! let described = describe("clip.mp4".as_ref(), &[], &options, &AtomicBool::new(false), |_| {})
//!     .expect("described");
//! println!("{}", described.description.summary);
//! for moment in &described.description.segments {
//!     println!("{:.0}–{:.0} s: {}", moment.start_s, moment.end_s, moment.description);
//! }
//! ```
//!
//! Everything blocks: call it from a worker thread. Without the default `frames` feature the
//! crate has no GStreamer dependency and no [`describe`]; the models, the request, the answer,
//! the estimate and the Anthropic client remain.
//!
//! Whole folders: [`describe_folder`] describes many clips with several in flight, skipping those
//! a [`Cache`] already has and stopping within a [`Budget`]; [`group_clips`] groups similar
//! footage. See `docs/design/whole-folders.md`.

pub mod anthropic;
pub mod ass;
mod cache;
mod debug_frames;
mod describe;
mod folder;
#[cfg(feature = "frames")]
pub mod frames;
mod groups;
mod moment;
pub mod openai;
pub mod provider;
pub mod shots;
pub mod srt;
mod tags;

use std::time::Duration;

pub use cache::{cache_path, Cache, CacheKey, ClipRecord, FileIdentity, CACHE_FILE_NAME};
pub use debug_frames::{debug_frames_dir, dump_frames, set_debug_frames_dir, DEBUG_FRAMES_ENV};
pub use describe::*;
pub use folder::{
    cached_clip, find_videos, request_cost_bound, serve_after_stop, Budget, ClipOutcome,
    DescribedClip, FolderEvent, FolderRun, FrameFingerprint, Reservation, Reserved, RunOptions,
    Stop, DEFAULT_JOBS, VIDEO_EXTENSIONS,
};
#[cfg(feature = "frames")]
pub use folder::{describe_clip, describe_folder};
#[cfg(feature = "frames")]
pub use frames::detect_shots;
pub use groups::{group_clips, ClipGroups, Group, GroupOptions, Grouping, Stretch};
pub use moment::*;
pub use provider::{AiError, AiProvider, AiUsage, Provider, RetryReason};
pub use tags::*;

/// One subtitle cue, sent with the frames so the description knows what is said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start: Duration,
    pub end: Duration,
    pub text: String,
}

/// How a clip is described.
#[derive(Clone, PartialEq)]
pub struct Options {
    /// The API key for `model`'s provider (see [`Model::provider`]): an Anthropic key for an
    /// Anthropic model, an OpenAI key for a GPT model.
    pub api_key: String,
    pub model: Model,
    pub language: SummaryLanguage,
    /// How the clip's frames are chosen; see [`FrameSampling`].
    pub frame_sampling: FrameSampling,
    /// How many moments (segments) a description gets; see [`MomentsMode`].
    pub moments: MomentsMode,
    /// The user's own notes about the footage, added to the request of [`describe`],
    /// [`describe_with_tags`], [`suggest_tags`] and [`describe_moment`]: names to use, things
    /// to mark as moments, tags to suggest. `None` (or only whitespace) leaves every request
    /// exactly as it is without them. At most [`MAX_INSTRUCTIONS_CHARS`] characters, otherwise
    /// those functions return [`Error::InstructionsTooLong`] before anything is sent. They count
    /// in the price of a request: add their length to `subtitle_bytes` of [`estimate_usage`].
    pub instructions: Option<String>,
}

impl Options {
    /// `Ok` when [`Options::instructions`] fits [`MAX_INSTRUCTIONS_CHARS`].
    pub fn check_instructions(&self) -> Result<(), Error> {
        match self
            .instructions
            .as_deref()
            .map(|notes| notes.trim().chars().count())
        {
            Some(chars) if chars > MAX_INSTRUCTIONS_CHARS => Err(Error::InstructionsTooLong {
                chars,
                max: MAX_INSTRUCTIONS_CHARS,
            }),
            _ => Ok(()),
        }
    }
}

impl std::fmt::Debug for Options {
    /// The key is left out: options end up in logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("api_key", &"***")
            .field("model", &self.model.id)
            .field("language", &self.language)
            .field("frame_sampling", &self.frame_sampling)
            .field("moments", &self.moments)
            // The notes are the user's own: only their length goes to a log.
            .field(
                "instructions_chars",
                &self
                    .instructions
                    .as_ref()
                    .map(|notes| notes.chars().count()),
            )
            .finish()
    }
}

/// Where [`describe`] is, for a progress display. New stages may be added: match with a wildcard
/// arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// Only [`describe_moment_notifying`]: the frames are read and the video file is closed, so
    /// it can be moved or deleted while the request is in flight. Comes before
    /// [`Stage::Asking`]; not reported when reading the video failed or `cancel` was set.
    Released,
    /// Reading frame `done + 1` of `total`.
    Frame { done: usize, total: usize },
    /// The frames are sent to `provider`; waiting for the answer.
    Asking { provider: Provider },
    /// The request is waiting `after` before it is sent again, for `reason`.
    Retrying {
        after: std::time::Duration,
        reason: RetryReason,
    },
}

/// A described clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Described {
    pub description: Description,
    /// What the request was billed for; see [`Model::cost_usd`].
    pub usage: AiUsage,
    pub duration_s: f64,
    /// Frames sent.
    pub frames: usize,
}

/// Why a clip was not described.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// `cancel` was set.
    Cancelled,
    /// The video could not be opened or gave no frames; the reason is for the log.
    Unreadable(String),
    /// Longer than [`MAX_DURATION_S`]; the clip's length in seconds.
    TooLong(f64),
    /// The request failed; nothing usable was billed unless it timed out.
    Ai(AiError),
    /// An answer came (and was billed) but could not be used.
    BadAnswer { reason: String, usage: AiUsage },
    /// [`Options::instructions`] is longer than `max` characters; nothing was sent.
    InstructionsTooLong { chars: usize, max: usize },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Unreadable(reason) => write!(f, "the video could not be read ({reason})"),
            Self::TooLong(duration_s) => write!(
                f,
                "the clip is {}, longer than the {} limit",
                format_time(*duration_s),
                format_time(MAX_DURATION_S)
            ),
            Self::Ai(error) => f.write_str(&error.reason()),
            Self::BadAnswer { reason, .. } => f.write_str(reason),
            Self::InstructionsTooLong { chars, max } => write!(
                f,
                "the instructions are {chars} characters, longer than the {max} limit; shorten them"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// The client for `options.model`'s provider; with a `gate`, pausing together with every other
/// client sharing it (the workers of one folder run).
fn provider_for(
    options: &Options,
    gate: Option<std::sync::Arc<provider::RateGate>>,
) -> Result<Box<dyn provider::AiProvider>, Error> {
    options.check_instructions()?;
    let key = options.api_key.clone();
    match options.model.provider {
        Provider::Anthropic => Ok(Box::new(
            anthropic::Anthropic::new(key)
                .map_err(Error::Ai)?
                .with_rate_gate(gate),
        )),
        Provider::OpenAi => Ok(Box::new(
            openai::OpenAi::new(key)
                .map_err(Error::Ai)?
                .with_rate_gate(gate),
        )),
    }
}

/// How `sampling` picked the frames of a clip `duration_s` long, for `frames.json`: key frames
/// fall back to an interval when the budget is one frame or none.
#[cfg(feature = "frames")]
fn picked(duration_s: f64, sampling: FrameSampling) -> &'static str {
    if sampling == FrameSampling::Shots {
        "shot"
    } else if sampling == FrameSampling::KeyFrames && frame_count(duration_s) > 1 {
        "key_frame"
    } else {
        "interval"
    }
}

/// Sample the frames [`describe`] would send for `video` and write them as
/// [`set_debug_frames_dir`] does, into `dir`, without sending anything: for looking at the
/// frames without paying for a request. Returns how many were written (a write error is only
/// logged).
#[cfg(feature = "frames")]
pub fn dump_clip_frames(
    video: &std::path::Path,
    dir: &std::path::Path,
    sampling: FrameSampling,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize, Error> {
    let clip = frames::Clip::open(video, frames::OPEN_TIMEOUT).map_err(Error::Unreadable)?;
    let duration_s = clip
        .duration_s()
        .ok_or_else(|| Error::Unreadable("no duration".to_string()))?;
    if duration_s > MAX_DURATION_S {
        return Err(Error::TooLong(duration_s));
    }
    let sampled = frames::sample_clip(video, &clip, duration_s, sampling, cancel, |_, _| {});
    let frames = match sampled.map(|s| s.map(|s| s.frames)) {
        Ok(Some(frames)) if !frames.is_empty() => frames,
        Ok(Some(_)) => return Err(Error::Unreadable("no frames".to_string())),
        Ok(None) => return Err(Error::Cancelled),
        Err(e) => return Err(Error::Unreadable(e)),
    };
    debug_frames::dump_frames(
        dir,
        &debug_frames::name_of(video),
        &frames,
        picked(duration_s, sampling),
    );
    Ok(frames.len())
}

/// Describe the video at `video`, with its `subtitles` (empty when it has none). `cancel` is
/// checked between frames and while waiting for the answer; `on_stage` follows along.
#[cfg(feature = "frames")]
pub fn describe(
    video: &std::path::Path,
    subtitles: &[Cue],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
    on_stage: impl FnMut(Stage),
) -> Result<Described, Error> {
    let clip = uncapped(describe_clip(
        video,
        subtitles,
        None,
        options,
        &Budget::new(None),
        cancel,
        on_stage,
    )?);
    Ok(Described {
        frames: clip.frames.len(),
        description: clip.description,
        usage: clip.usage,
        duration_s: clip.duration_s,
    })
}

/// What [`describe_clip`] gives with a budget without a cap, which always has room.
#[cfg(feature = "frames")]
fn uncapped(clip: Option<DescribedClip>) -> DescribedClip {
    match clip {
        Some(clip) => clip,
        None => unreachable!("a budget without a cap never refuses a request"),
    }
}

/// A clip described together with tag suggestions from a vocabulary, from one request: see
/// [`describe_with_tags`].
#[derive(Debug, Clone, PartialEq)]
pub struct DescribedWithTags {
    pub description: Description,
    pub tags: TagSuggestions,
    pub usage: AiUsage,
    pub duration_s: f64,
    pub frames: usize,
}

/// Describe the clip at `video` and suggest tags from `vocabulary` in one request: cheaper than
/// [`describe`] followed by [`suggest_tags`], since the frames and the API call are shared. See
/// [`suggest_tags`] to tag an already-described clip without reading it again.
#[cfg(feature = "frames")]
pub fn describe_with_tags(
    video: &std::path::Path,
    subtitles: &[Cue],
    vocabulary: &[Tag],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
    on_stage: impl FnMut(Stage),
) -> Result<DescribedWithTags, Error> {
    let clip = uncapped(describe_clip(
        video,
        subtitles,
        Some(vocabulary),
        options,
        &Budget::new(None),
        cancel,
        on_stage,
    )?);
    Ok(DescribedWithTags {
        frames: clip.frames.len(),
        description: clip.description,
        // Always there when a vocabulary was given.
        tags: clip.tags.unwrap_or_default(),
        usage: clip.usage,
        duration_s: clip.duration_s,
    })
}

/// Suggest tags from `vocabulary` for a clip already described (by [`describe`] or
/// [`describe_with_tags`]): no video is read, so this works without the `frames` feature, from
/// `description` and `duration_s` alone. Cheaper than [`describe_with_tags`], but blind to
/// anything `description`'s summary and segments left out. `cancel` is checked while waiting for
/// the answer. Like `describe`, a bad answer is still billed: [`Error::BadAnswer`] carries the
/// usage so the caller can still account for it.
pub fn suggest_tags(
    description: &Description,
    duration_s: f64,
    subtitles: &[Cue],
    vocabulary: &[Tag],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<TagSuggestions, Error> {
    let provider = provider_for(options, None)?;
    let mut request = build_tags_only_request(
        options.model,
        description,
        subtitles,
        vocabulary,
        duration_s,
    );
    add_instructions(&mut request, options.instructions.as_deref(), true);
    let response = provider.complete(&request, cancel).map_err(|e| match e {
        AiError::Cancelled => Error::Cancelled,
        e => Error::Ai(e),
    })?;
    parse_tags_only_answer(&response, vocabulary, duration_s).map_err(|reason| Error::BadAnswer {
        reason,
        usage: response.usage,
    })
}

/// A named, described moment of a clip: see [`describe_moment`].
#[derive(Debug, Clone, PartialEq)]
pub struct DescribedMoment {
    pub moment: Moment,
    /// What the request was billed for; see [`Model::cost_usd`].
    pub usage: AiUsage,
}

/// Name and describe the frame at `at_s` in `video`, for a marker there, instead of describing
/// the whole clip ([`describe`]): reads the frame plus `window_s` on each side of it
/// ([`frames::Clip::sample_moment`]) and any subtitle lines that overlap that same window, and
/// asks for a short name and a one-to-two sentence description in one small, fast request.
/// [`MOMENT_WINDOW_S`] is a reasonable default for `window_s`. `cancel` is checked between
/// frames and while waiting for the answer. See [`describe_moment_notifying`] to learn when the
/// video file is released.
#[cfg(feature = "frames")]
pub fn describe_moment(
    video: &std::path::Path,
    at_s: f64,
    window_s: f64,
    subtitles: &[Cue],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<DescribedMoment, Error> {
    describe_moment_notifying(video, at_s, window_s, subtitles, options, cancel, |_| {})
}

/// [`describe_moment`], calling `on_stage` as it goes: [`Stage::Released`] once the frames are
/// read and `video` is closed (before the request, which can take up to two minutes, so the file
/// can be moved or deleted meanwhile), then [`Stage::Asking`], and [`Stage::Retrying`] before
/// each wait between attempts. `on_stage` runs on the calling thread.
#[cfg(feature = "frames")]
pub fn describe_moment_notifying(
    video: &std::path::Path,
    at_s: f64,
    window_s: f64,
    subtitles: &[Cue],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
    on_stage: impl FnMut(Stage),
) -> Result<DescribedMoment, Error> {
    describe_moment_with(
        video,
        at_s,
        window_s,
        subtitles,
        options,
        cancel,
        on_stage,
        || provider_for(options, None),
    )
}

/// [`describe_moment_notifying`] with the provider made by `provider`, which tests replace.
#[cfg(feature = "frames")]
#[allow(clippy::too_many_arguments)]
fn describe_moment_with<'p>(
    video: &std::path::Path,
    at_s: f64,
    window_s: f64,
    subtitles: &[Cue],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
    mut on_stage: impl FnMut(Stage),
    provider: impl FnOnce() -> Result<Box<dyn provider::AiProvider + 'p>, Error>,
) -> Result<DescribedMoment, Error> {
    options.check_instructions()?;
    let clip = frames::Clip::open(video, frames::OPEN_TIMEOUT).map_err(Error::Unreadable)?;
    let frames = match clip.sample_moment(at_s, window_s, cancel) {
        Ok(Some(frames)) if !frames.is_empty() => frames,
        Ok(Some(_)) => return Err(Error::Unreadable("no frames".to_string())),
        Ok(None) => return Err(Error::Cancelled),
        Err(e) => return Err(Error::Unreadable(e)),
    };
    drop(clip);
    on_stage(Stage::Released);
    debug_frames::dump_if_on(
        &format!("{}-at-{at_s:.2}s", debug_frames::name_of(video)),
        &frames,
        "moment",
    );
    let mut request = build_moment_request(
        options.model,
        &frames,
        subtitles,
        at_s,
        window_s,
        options.language,
    );
    add_instructions(&mut request, options.instructions.as_deref(), false);
    let provider = provider()?;
    on_stage(Stage::Asking {
        provider: options.model.provider,
    });
    let response = provider
        .complete_notifying(&request, cancel, &mut |after, reason| {
            on_stage(Stage::Retrying { after, reason })
        })
        .map_err(|e| match e {
            AiError::Cancelled => Error::Cancelled,
            e => Error::Ai(e),
        })?;
    let moment = parse_moment_answer(&response).map_err(|reason| Error::BadAnswer {
        reason,
        usage: response.usage,
    })?;
    Ok(DescribedMoment {
        moment,
        usage: response.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[cfg(feature = "frames")]
    #[test]
    fn a_file_that_is_not_a_video_is_unreadable() {
        let dir = std::env::temp_dir().join(format!("clipscribe-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let fake = dir.join("fake.mp4");
        std::fs::write(&fake, b"not a movie").expect("write");
        let options = Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        };
        let result = describe(&fake, &[], &options, &AtomicBool::new(false), |_| {});
        assert!(matches!(result, Err(Error::Unreadable(_))), "{result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "frames")]
    #[test]
    fn a_video_file_with_tags_that_is_not_a_video_is_unreadable() {
        let dir = std::env::temp_dir().join(format!("clipscribe-lib-tags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let fake = dir.join("fake.mp4");
        std::fs::write(&fake, b"not a movie").expect("write");
        let options = Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        };
        let vocabulary = vec![Tag {
            name: "Goat".to_string(),
            hint: None,
        }];
        let result = describe_with_tags(
            &fake,
            &[],
            &vocabulary,
            &options,
            &AtomicBool::new(false),
            |_| {},
        );
        assert!(matches!(result, Err(Error::Unreadable(_))), "{result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether this process holds `path` open (Linux: its open file descriptors).
    #[cfg(all(feature = "frames", target_os = "linux"))]
    fn is_open(path: &std::path::Path) -> bool {
        let path = path.canonicalize().expect("path");
        std::fs::read_dir("/proc/self/fd")
            .expect("fds")
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == path))
    }

    /// Answers like the model would, noting what the notifications said when the request came.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    struct Stub<'a> {
        seen: &'a std::cell::RefCell<Vec<String>>,
        clip_open: &'a std::cell::Cell<Option<bool>>,
        clip: std::path::PathBuf,
    }

    #[cfg(all(feature = "frames", target_os = "linux"))]
    impl provider::AiProvider for Stub<'_> {
        fn complete(
            &self,
            _: &provider::AiRequest,
            _: &AtomicBool,
        ) -> Result<provider::AiResponse, AiError> {
            self.seen.borrow_mut().push("request".to_string());
            self.clip_open.set(Some(is_open(&self.clip)));
            Ok(provider::AiResponse {
                json: serde_json::json!({"name": "Goat", "description": "A goat walks."}),
                stop_reason: "end_turn".to_string(),
                usage: AiUsage::default(),
            })
        }
    }

    #[cfg(all(feature = "frames", target_os = "linux"))]
    fn moment_options() -> Options {
        Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        }
    }

    /// A private copy of a test clip, so no other test holds the same file open.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    fn private_clip(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("clipscribe-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let copy = dir.join("clip.mov");
        std::fs::copy("tests/clips/file_example_MOV_480_700kB.mov", &copy).expect("copy");
        (dir, copy)
    }

    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn describe_moment_reports_the_clip_released_before_the_request() {
        let (dir, video) = private_clip("moment-released");
        let seen = std::cell::RefCell::new(Vec::new());
        let clip_open = std::cell::Cell::new(None);
        let stub = Stub {
            seen: &seen,
            clip_open: &clip_open,
            clip: video.clone(),
        };
        let result = describe_moment_with(
            &video,
            1.0,
            MOMENT_WINDOW_S,
            &[],
            &moment_options(),
            &AtomicBool::new(false),
            |stage| {
                let name = match stage {
                    Stage::Released => {
                        assert!(!is_open(&video), "released while the file is still open");
                        "released"
                    }
                    Stage::Asking { .. } => "asking",
                    _ => "other",
                };
                seen.borrow_mut().push(name.to_string());
            },
            || Ok(Box::new(stub)),
        );
        assert_eq!(result.expect("described").moment.name, "Goat");
        assert_eq!(*seen.borrow(), ["released", "asking", "request"]);
        assert_eq!(clip_open.get(), Some(false), "clip open during the request");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn instructions_up_to_the_cap_are_fine_and_longer_ones_are_an_error() {
        let options = |chars: usize| Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: Some("é".repeat(chars)),
        };
        assert_eq!(options(MAX_INSTRUCTIONS_CHARS).check_instructions(), Ok(()));
        let too_long = options(MAX_INSTRUCTIONS_CHARS + 1);
        assert_eq!(
            too_long.check_instructions(),
            Err(Error::InstructionsTooLong {
                chars: MAX_INSTRUCTIONS_CHARS + 1,
                max: MAX_INSTRUCTIONS_CHARS
            })
        );
        // Before any provider is made, so before any request.
        assert!(matches!(
            provider_for(&too_long, None),
            Err(Error::InstructionsTooLong { .. })
        ));
        let message = Error::InstructionsTooLong {
            chars: 2001,
            max: 2000,
        }
        .to_string();
        assert!(
            message.contains("2001") && message.contains("2000"),
            "{message}"
        );
    }

    #[test]
    fn the_debug_output_keeps_the_key_and_the_notes_out() {
        let options = Options {
            api_key: "sk-secret".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: Some("The man is Eugene.".to_string()),
        };
        let shown = format!("{options:?}");
        assert!(
            !shown.contains("sk-secret") && !shown.contains("Eugene"),
            "{shown}"
        );
    }

    /// Keeps the first text of the request it is asked, answers a moment.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    struct Capture<'a>(&'a std::cell::RefCell<String>);

    #[cfg(all(feature = "frames", target_os = "linux"))]
    impl provider::AiProvider for Capture<'_> {
        fn complete(
            &self,
            request: &provider::AiRequest,
            _: &AtomicBool,
        ) -> Result<provider::AiResponse, AiError> {
            if let provider::AiContent::Text(text) = &request.content[0] {
                *self.0.borrow_mut() = text.clone();
            }
            Ok(provider::AiResponse {
                json: serde_json::json!({"name": "Moon", "description": "The moon."}),
                stop_reason: "end_turn".to_string(),
                usage: AiUsage::default(),
            })
        }
    }

    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn describe_moment_sends_the_notes() {
        let (dir, video) = private_clip("moment-notes");
        let text = std::cell::RefCell::new(String::new());
        let options = Options {
            instructions: Some("Mark the moon separately.".to_string()),
            ..moment_options()
        };
        describe_moment_with(
            &video,
            1.0,
            MOMENT_WINDOW_S,
            &[],
            &options,
            &AtomicBool::new(false),
            |_| {},
            || Ok(Box::new(Capture(&text))),
        )
        .expect("described");
        assert!(text
            .borrow()
            .contains("<notes>\nMark the moon separately.\n</notes>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggest_tags_refuses_notes_over_the_cap_before_any_request() {
        let options = Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: Some("x".repeat(MAX_INSTRUCTIONS_CHARS + 1)),
        };
        let description = Description {
            summary: "s".to_string(),
            segments: vec![],
            main: None,
        };
        let result = suggest_tags(
            &description,
            10.0,
            &[],
            &[],
            &options,
            &AtomicBool::new(false),
        );
        assert!(
            matches!(result, Err(Error::InstructionsTooLong { .. })),
            "{result:?}"
        );
    }

    /// A clip that cannot be read never reaches the request, so it is never "released".
    #[cfg(feature = "frames")]
    #[test]
    fn describe_moment_does_not_report_a_release_for_an_unreadable_clip() {
        let dir =
            std::env::temp_dir().join(format!("clipscribe-moment-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let fake = dir.join("fake.mp4");
        std::fs::write(&fake, b"not a movie").expect("write");
        let options = Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        };
        let mut stages = Vec::new();
        let result = describe_moment_notifying(
            &fake,
            1.0,
            MOMENT_WINDOW_S,
            &[],
            &options,
            &AtomicBool::new(false),
            |stage| stages.push(stage),
        );
        assert!(matches!(result, Err(Error::Unreadable(_))), "{result:?}");
        assert!(stages.is_empty(), "{stages:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn a_test_clip_dumps_its_frames_and_an_index() {
        let dir = std::env::temp_dir().join(format!("clipscribe-lib-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let video = std::path::Path::new("tests/clips/file_example_MOV_480_700kB.mov");
        let count = dump_clip_frames(
            video,
            &dir,
            FrameSampling::KeyFrames,
            &AtomicBool::new(false),
        )
        .expect("frames");
        let folder = dir.join("file_example_MOV_480_700kB");
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.join("frames.json")).expect("index"))
                .expect("json");
        let entries = index.as_array().expect("array");
        assert_eq!(entries.len(), count);
        assert!(count > 1);
        for entry in entries {
            let jpeg = std::fs::read(folder.join(entry["file"].as_str().expect("file")))
                .expect("frame file");
            assert_eq!(jpeg.len() as u64, entry["bytes"].as_u64().expect("bytes"));
            assert_eq!(&jpeg[..2], &[0xff, 0xd8], "a JPEG");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn options_never_print_the_key() {
        let options = Options {
            api_key: "sk-ant-secret".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        };
        let debug = format!("{options:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("KeyFrames"), "{debug}");
    }

    /// A real request, only when `CLIPSCRIBE_LIVE_API_KEY` is set (never in CI without the
    /// secret): one test clip, Claude Haiku 4.5, a summary and moments inside the clip. Prints
    /// the real token usage for the estimate to be checked against.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn live_description_of_a_test_clip() {
        let Some(api_key) = std::env::var("CLIPSCRIBE_LIVE_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        else {
            eprintln!("CLIPSCRIBE_LIVE_API_KEY not set: live test skipped");
            return;
        };
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/clips/file_example_MP4_480_1_5MG.mp4");
        let options = Options {
            api_key: api_key.trim().to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        };
        let described = match describe(&path, &[], &options, &AtomicBool::new(false), |_| {}) {
            Ok(described) => described,
            // The key works but its account cannot pay: nothing about the code to test.
            Err(Error::Ai(e @ (AiError::OutOfCredit(_) | AiError::LimitReached(_)))) => {
                eprintln!("live test skipped: {}", e.reason());
                return;
            }
            Err(e) => panic!("answer: {e:?}"),
        };
        eprintln!(
            "live: {} frames, usage {:?} (estimated {:?}), cost ${:.4}\n{:#?}",
            described.frames,
            described.usage,
            estimate_usage(Model::default(), described.duration_s, 0),
            Model::default().cost_usd(described.usage),
            described.description
        );
        let d = &described.description;
        assert!(!d.summary.is_empty());
        // `Important` mode may legitimately return no segments at all for this clip (a single
        // continuous shot with nothing standing out) — that is the feature, not a failure.
        assert!(d
            .segments
            .iter()
            .all(|s| s.start_s >= 0.0 && s.end_s <= described.duration_s));
    }

    /// [`live_description_of_a_test_clip`] with a note, only when `CLIPSCRIBE_LIVE_API_KEY` is set:
    /// the model is asked to call what it sees by a name only the note gives, and the name must
    /// come back in the description.
    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn live_description_follows_the_users_notes() {
        let Some(api_key) = std::env::var("CLIPSCRIBE_LIVE_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        else {
            eprintln!("CLIPSCRIBE_LIVE_API_KEY not set: live test skipped");
            return;
        };
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/clips/file_example_MP4_480_1_5MG.mp4");
        let options = Options {
            api_key: api_key.trim().to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: Some(
                "Call the main animal or object of this clip Zorblax, never anything else. \
                 Always use that name in the summary."
                    .to_string(),
            ),
        };
        let described = match describe(&path, &[], &options, &AtomicBool::new(false), |_| {}) {
            Ok(described) => described,
            Err(Error::Ai(e @ (AiError::OutOfCredit(_) | AiError::LimitReached(_)))) => {
                eprintln!("live test skipped: {}", e.reason());
                return;
            }
            Err(e) => panic!("answer: {e:?}"),
        };
        eprintln!("live: {:#?}", described.description);
        assert!(
            described.description.summary.contains("Zorblax"),
            "the note was not followed: {:?}",
            described.description.summary
        );
    }

    /// The same request as [`live_description_of_a_test_clip`], through OpenAI instead: only
    /// when `OPENAI_API_KEY` is set (never in CI without that secret, and not reachable at all
    /// from this crate's own development environment — `api.openai.com` is blocked there; see
    /// `docs/design/openai-provider.md`).
    #[cfg(all(feature = "frames", target_os = "linux"))]
    #[test]
    fn live_description_of_a_test_clip_with_openai() {
        let Some(api_key) = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
        else {
            eprintln!("OPENAI_API_KEY not set: live OpenAI test skipped");
            return;
        };
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/clips/file_example_MP4_480_1_5MG.mp4");
        let options = Options {
            api_key: api_key.trim().to_string(),
            model: Model::from_id("gpt-4.1-mini"),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
            instructions: None,
        };
        let described = match describe(&path, &[], &options, &AtomicBool::new(false), |_| {}) {
            Ok(described) => described,
            // The key works but its account cannot pay: nothing about the code to test.
            Err(Error::Ai(e @ (AiError::OutOfCredit(_) | AiError::LimitReached(_)))) => {
                eprintln!("live OpenAI test skipped: {}", e.reason());
                return;
            }
            Err(e) => panic!("answer: {e:?}"),
        };
        eprintln!(
            "live (openai): {} frames, usage {:?} (estimated {:?}), cost ${:.4}\n{:#?}",
            described.frames,
            described.usage,
            estimate_usage(options.model, described.duration_s, 0),
            options.model.cost_usd(described.usage),
            described.description
        );
        let d = &described.description;
        assert!(!d.summary.is_empty());
        assert!(d
            .segments
            .iter()
            .all(|s| s.start_s >= 0.0 && s.end_s <= described.duration_s));
    }
}
