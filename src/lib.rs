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

pub mod anthropic;
mod describe;
#[cfg(feature = "frames")]
pub mod frames;
mod moment;
pub mod openai;
pub mod provider;
pub mod srt;
mod tags;

use std::time::Duration;

pub use describe::*;
pub use moment::*;
pub use provider::{AiError, AiProvider, AiUsage, Provider};
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
            .finish()
    }
}

/// Where [`describe`] is, for a progress display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Reading frame `done + 1` of `total`.
    Frame { done: usize, total: usize },
    /// The frames are sent; waiting for the answer.
    Asking,
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
        }
    }
}

impl std::error::Error for Error {}

/// The client for `options.model`'s provider.
fn provider_for(options: &Options) -> Result<Box<dyn provider::AiProvider>, Error> {
    match options.model.provider {
        Provider::Anthropic => Ok(Box::new(
            anthropic::Anthropic::new(options.api_key.clone()).map_err(Error::Ai)?,
        )),
        Provider::OpenAi => Ok(Box::new(
            openai::OpenAi::new(options.api_key.clone()).map_err(Error::Ai)?,
        )),
    }
}

/// Describe the video at `video`, with its `subtitles` (empty when it has none). `cancel` is
/// checked between frames and while waiting for the answer; `on_stage` follows along.
#[cfg(feature = "frames")]
pub fn describe(
    video: &std::path::Path,
    subtitles: &[Cue],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
    mut on_stage: impl FnMut(Stage),
) -> Result<Described, Error> {
    let clip = frames::Clip::open(video, frames::OPEN_TIMEOUT).map_err(Error::Unreadable)?;
    let duration_s = clip
        .duration_s()
        .ok_or_else(|| Error::Unreadable("no duration".to_string()))?;
    if duration_s > MAX_DURATION_S {
        return Err(Error::TooLong(duration_s));
    }
    let frames = match clip.sample(duration_s, options.frame_sampling, cancel, |done, total| {
        on_stage(Stage::Frame { done, total })
    }) {
        Ok(Some(frames)) if !frames.is_empty() => frames,
        Ok(Some(_)) => return Err(Error::Unreadable("no frames".to_string())),
        Ok(None) => return Err(Error::Cancelled),
        Err(e) => return Err(Error::Unreadable(e)),
    };
    drop(clip);
    let request = build_request(
        options.model,
        &frames,
        subtitles,
        duration_s,
        options.language,
        options.moments,
    );
    on_stage(Stage::Asking);
    let provider = provider_for(options)?;
    let response = provider.complete(&request, cancel).map_err(|e| match e {
        AiError::Cancelled => Error::Cancelled,
        e => Error::Ai(e),
    })?;
    let description = parse_answer(&response, duration_s, options.moments).map_err(|reason| {
        Error::BadAnswer {
            reason,
            usage: response.usage,
        }
    })?;
    Ok(Described {
        description,
        usage: response.usage,
        duration_s,
        frames: frames.len(),
    })
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
    mut on_stage: impl FnMut(Stage),
) -> Result<DescribedWithTags, Error> {
    let clip = frames::Clip::open(video, frames::OPEN_TIMEOUT).map_err(Error::Unreadable)?;
    let duration_s = clip
        .duration_s()
        .ok_or_else(|| Error::Unreadable("no duration".to_string()))?;
    if duration_s > MAX_DURATION_S {
        return Err(Error::TooLong(duration_s));
    }
    let frames = match clip.sample(duration_s, options.frame_sampling, cancel, |done, total| {
        on_stage(Stage::Frame { done, total })
    }) {
        Ok(Some(frames)) if !frames.is_empty() => frames,
        Ok(Some(_)) => return Err(Error::Unreadable("no frames".to_string())),
        Ok(None) => return Err(Error::Cancelled),
        Err(e) => return Err(Error::Unreadable(e)),
    };
    drop(clip);
    let request = build_combined_request(
        options.model,
        &frames,
        subtitles,
        vocabulary,
        duration_s,
        options.language,
        options.moments,
    );
    on_stage(Stage::Asking);
    let provider = provider_for(options)?;
    let response = provider.complete(&request, cancel).map_err(|e| match e {
        AiError::Cancelled => Error::Cancelled,
        e => Error::Ai(e),
    })?;
    let (description, tags) =
        parse_combined_answer(&response, duration_s, vocabulary, options.moments).map_err(
            |reason| Error::BadAnswer {
                reason,
                usage: response.usage,
            },
        )?;
    Ok(DescribedWithTags {
        description,
        tags,
        usage: response.usage,
        duration_s,
        frames: frames.len(),
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
    let request = build_tags_only_request(
        options.model,
        description,
        subtitles,
        vocabulary,
        duration_s,
    );
    let provider = provider_for(options)?;
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
    pub usage: AiUsage,
}

/// Name and describe the frame at `at_s` in `video`, for a marker there, instead of describing
/// the whole clip ([`describe`]): reads the frame plus a few around it
/// ([`frames::Clip::sample_moment`], [`MOMENT_WINDOW_S`]) and any subtitle lines that overlap
/// that window, and asks for a short name and a one-to-two sentence description in one small,
/// fast request. `cancel` is checked between frames and while waiting for the answer.
#[cfg(feature = "frames")]
pub fn describe_moment(
    video: &std::path::Path,
    at_s: f64,
    subtitles: &[Cue],
    options: &Options,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<DescribedMoment, Error> {
    let clip = frames::Clip::open(video, frames::OPEN_TIMEOUT).map_err(Error::Unreadable)?;
    let frames = match clip.sample_moment(at_s, MOMENT_WINDOW_S, cancel) {
        Ok(Some(frames)) if !frames.is_empty() => frames,
        Ok(Some(_)) => return Err(Error::Unreadable("no frames".to_string())),
        Ok(None) => return Err(Error::Cancelled),
        Err(e) => return Err(Error::Unreadable(e)),
    };
    drop(clip);
    let request = build_moment_request(options.model, &frames, subtitles, at_s, options.language);
    let provider = provider_for(options)?;
    let response = provider.complete(&request, cancel).map_err(|e| match e {
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

    #[test]
    fn options_never_print_the_key() {
        let options = Options {
            api_key: "sk-ant-secret".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
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
