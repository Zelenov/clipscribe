//! Describing a clip: which frames to sample, the request built from them and the subtitles,
//! reading the answer back, and what it costs.

use serde_json::{json, Value};

use super::provider::{AiContent, AiRequest, AiResponse, AiUsage, Provider};
use crate::Cue;

/// The models descriptions can be written with. Anthropic's three come first, cheapest to
/// strongest, and `MODELS[0]` (Claude Haiku 4.5) is the crate's default regardless of what
/// follows; the two OpenAI models after them are *not* cheaper-than-Haiku-implies-default, since
/// picking a provider (`Options.model.provider`, the CLI's `--provider`) is a separate, explicit
/// choice from picking a model. Sonnet and Opus think before answering: at low effort, since
/// describing frames needs little reasoning, with room for the thinking in their answer budget;
/// neither GPT-4.1 model has a reasoning setting, so their `effort` stays `None`, like Haiku's.
pub const MODELS: [Model; 5] = [
    Model {
        provider: Provider::Anthropic,
        id: "claude-haiku-4-5",
        label: "Claude Haiku 4.5",
        input_usd_per_mtok: 1.0,
        output_usd_per_mtok: 5.0,
        effort: None,
        max_answer_tokens: 4000,
        answer_tokens: 600,
    },
    Model {
        provider: Provider::Anthropic,
        id: "claude-sonnet-5",
        label: "Claude Sonnet 5",
        input_usd_per_mtok: 2.0,
        output_usd_per_mtok: 10.0,
        effort: Some("low"),
        max_answer_tokens: 16000,
        answer_tokens: 1500,
    },
    Model {
        provider: Provider::Anthropic,
        id: "claude-opus-5",
        label: "Claude Opus 5",
        input_usd_per_mtok: 5.0,
        output_usd_per_mtok: 25.0,
        effort: Some("low"),
        max_answer_tokens: 16000,
        answer_tokens: 1500,
    },
    Model {
        provider: Provider::OpenAi,
        id: "gpt-4.1-mini",
        label: "GPT-4.1 mini",
        input_usd_per_mtok: 0.40,
        output_usd_per_mtok: 1.60,
        effort: None,
        max_answer_tokens: 4000,
        answer_tokens: 600,
    },
    Model {
        provider: Provider::OpenAi,
        id: "gpt-4.1",
        label: "GPT-4.1",
        input_usd_per_mtok: 2.0,
        output_usd_per_mtok: 8.0,
        effort: None,
        max_answer_tokens: 4000,
        answer_tokens: 600,
    },
];

/// When the prices in [`MODELS`] were checked. The two OpenAI models were priced from training
/// data, not a live quote: `api.openai.com` and OpenAI's own docs are both blocked from the
/// environment this crate is developed in (see `docs/design/openai-provider.md`). Confirm them
/// against OpenAI's current pricing before relying on `--estimate` for `--provider openai`.
pub const PRICES_CHECKED: &str = "2026-09-26";

/// Clips longer than this are skipped.
pub const MAX_DURATION_S: f64 = 30.0 * 60.0;
/// One frame every this many seconds, up to [`MAX_FRAMES`].
const FRAME_INTERVAL_S: f64 = 2.0;
/// Frames sent per clip at most; a longer clip is sampled evenly.
pub const MAX_FRAMES: usize = 60;
/// How much denser `candidate_times` samples than [`sample_times`], to give
/// `select_key_frames` real choices inside each window it picks from. Key-frame selection only
/// runs where there are decoded candidates to choose from (the `frames` feature).
#[cfg(feature = "frames")]
const CANDIDATE_OVERSAMPLE: usize = 4;
/// The long side of a frame, in pixels.
pub const FRAME_LONG_SIDE: u32 = 512;
/// Instruction tokens per request, for the estimate.
const INSTRUCTION_TOKENS: u64 = 600;
/// Characters of subtitle text per token, for the estimate.
const CHARS_PER_TOKEN: f64 = 3.5;
/// In [`MomentsMode::Important`], a segment spanning at least this fraction of the clip is
/// dropped: that is the summary's job, not a segment's.
const WHOLE_CLIP_FRACTION: f64 = 0.9;

/// A model, its prices and how it is asked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Model {
    /// Which AI service this model belongs to; picks the client [`crate::describe`] and friends
    /// build from [`Options::api_key`](crate::Options::api_key).
    pub provider: Provider,
    pub id: &'static str,
    /// The name the panel and the settings show.
    pub label: &'static str,
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    /// `output_config.effort`; `None` for a model without it (Haiku 4.5 rejects it).
    pub effort: Option<&'static str>,
    /// Output tokens a request may use, thinking included.
    pub max_answer_tokens: u32,
    /// Output tokens of a typical answer, thinking included, for the estimate.
    pub answer_tokens: u64,
}

impl Model {
    /// The model with the stored id; an unknown id falls back to the default.
    pub fn from_id(id: &str) -> Self {
        MODELS.into_iter().find(|m| m.id == id).unwrap_or(MODELS[0])
    }

    /// What `usage` costs, in US dollars.
    pub fn cost_usd(&self, usage: AiUsage) -> f64 {
        (usage.input_tokens as f64 * self.input_usd_per_mtok
            + usage.output_tokens as f64 * self.output_usd_per_mtok)
            / 1_000_000.0
    }
}

/// How a clip's frames are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameSampling {
    /// Candidates spaced closely, keeping the one in each window of the clip where the picture
    /// changes the most (`select_key_frames`, `frames` feature only); the same frame budget as
    /// `Interval`, spent on where the clip actually changes instead of a blind timestamp.
    #[default]
    KeyFrames,
    /// One frame every 2 s, at most [`MAX_FRAMES`] spread evenly over a longer clip — today's
    /// behaviour before key frames, kept for anyone who wants the old, predictable spacing.
    Interval,
}

/// How many moments (segments) a description gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MomentsMode {
    /// Only moments that stand out: none at all for a static or uniform clip (the summary is
    /// enough then), one per clearly different part of the clip otherwise. `max_segments` stays
    /// an upper bound, never a target to fill.
    #[default]
    Important,
    /// Today's behaviour before this mode existed: the model is asked to cover the whole clip in
    /// consecutive stretches, and the extra `Important`-only validation is skipped.
    Full,
}

/// The language descriptions are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SummaryLanguage {
    /// The subtitles' language; English when a clip has none.
    #[default]
    SameAsSubtitles,
    English,
    Russian,
    Ukrainian,
    German,
    Spanish,
    French,
}

impl SummaryLanguage {
    /// Every choice, in the order the dropdown lists them.
    pub const ALL: [SummaryLanguage; 7] = [
        Self::SameAsSubtitles,
        Self::English,
        Self::Russian,
        Self::Ukrainian,
        Self::German,
        Self::Spanish,
        Self::French,
    ];

    /// The name stored in the settings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameAsSubtitles => "subtitles",
            Self::English => "en",
            Self::Russian => "ru",
            Self::Ukrainian => "uk",
            Self::German => "de",
            Self::Spanish => "es",
            Self::French => "fr",
        }
    }

    /// Read a stored name; unknown names fall back to the default.
    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|l| l.as_str() == name)
            .unwrap_or_default()
    }

    /// The sentence of the instructions that names the language.
    pub(crate) fn instruction(self, has_subtitles: bool) -> &'static str {
        match self {
            Self::SameAsSubtitles if has_subtitles => "Write in the language of the subtitles.",
            Self::SameAsSubtitles | Self::English => "Write in English.",
            Self::Russian => "Write in Russian.",
            Self::Ukrainian => "Write in Ukrainian.",
            Self::German => "Write in German.",
            Self::Spanish => "Write in Spanish.",
            Self::French => "Write in French.",
        }
    }
}

impl Default for Model {
    fn default() -> Self {
        MODELS[0]
    }
}

impl std::fmt::Display for Model {
    /// `Claude Sonnet 5 ($2 / $10 per M tokens)`, as the settings list it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (${} / ${} per M tokens)",
            self.label, self.input_usd_per_mtok, self.output_usd_per_mtok
        )
    }
}

impl std::fmt::Display for SummaryLanguage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SameAsSubtitles => "Same as the subtitles (English if none)",
            Self::English => "English",
            Self::Russian => "Russian",
            Self::Ukrainian => "Ukrainian",
            Self::German => "German",
            Self::Spanish => "Spanish",
            Self::French => "French",
        })
    }
}

/// A stretch of the clip and what happens in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start_s: f64,
    pub end_s: f64,
    pub description: String,
}

/// What a clip shows: a one-line summary and its segments in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub summary: String,
    pub segments: Vec<Segment>,
}

/// Where to take frames in a clip `duration_s` long: one every 2 s, at most 60 (a longer clip
/// is sampled evenly), one in the middle of a clip shorter than 2 s.
pub fn sample_times(duration_s: f64) -> Vec<f64> {
    sample_times_with(duration_s, FRAME_INTERVAL_S, MAX_FRAMES)
}

/// How many frames [`sample_times`] takes from a clip `duration_s` long, without listing them.
pub fn frame_count(duration_s: f64) -> usize {
    count_with(duration_s, FRAME_INTERVAL_S, MAX_FRAMES)
}

/// Candidates for `select_key_frames` to choose from: `CANDIDATE_OVERSAMPLE` times denser than
/// [`sample_times`] and capped at that many times more of them, so every window
/// `select_key_frames` picks one frame from has real choices in it. Below two frames' worth of
/// budget ([`frame_count`] returns 0 or 1) there is no window to choose within, so callers use
/// [`sample_times`] directly instead of this.
#[cfg(feature = "frames")]
pub(crate) fn candidate_times(duration_s: f64) -> Vec<f64> {
    sample_times_with(
        duration_s,
        FRAME_INTERVAL_S / CANDIDATE_OVERSAMPLE as f64,
        MAX_FRAMES * CANDIDATE_OVERSAMPLE,
    )
}

/// Shared shape of [`sample_times`] and [`candidate_times`]: one sample every `interval_s`, at
/// most `max_count` (a longer clip is sampled evenly), one in the middle of a clip shorter than
/// `interval_s`.
fn sample_times_with(duration_s: f64, interval_s: f64, max_count: usize) -> Vec<f64> {
    if duration_s < interval_s {
        return (duration_s > 0.0)
            .then_some(duration_s / 2.0)
            .into_iter()
            .collect();
    }
    let interval = interval_s.max(duration_s / max_count as f64);
    (0..count_with(duration_s, interval_s, max_count))
        .map(|i| i as f64 * interval)
        .collect()
}

/// How many samples [`sample_times_with`] takes, without listing them.
fn count_with(duration_s: f64, interval_s: f64, max_count: usize) -> usize {
    if duration_s <= 0.0 {
        return 0;
    }
    if duration_s < interval_s {
        return 1;
    }
    let interval = interval_s.max(duration_s / max_count as f64);
    ((duration_s / interval).ceil() as usize).min(max_count)
}

/// How a clip's frames are chosen: see [`FrameSampling`].
///
/// From `candidates` (each candidate's time and its fingerprint — same-length byte vectors, a
/// downsampled grayscale grid of the frame; see `frames::fingerprint`), keep at most
/// `max_frames`: split the candidates into `max_frames` equal-sized windows and, in each, the one
/// whose fingerprint differs most from the *previous* candidate's — the moment inside that
/// window where the picture changes the most. A window with no real change keeps its first
/// candidate, the same timestamp fixed-interval sampling would have picked there.
///
/// Returns indices into `candidates`, strictly increasing (each window's pick is drawn from its
/// own non-overlapping range), so the result needs no separate de-duplication pass.
#[cfg(feature = "frames")]
pub(crate) fn select_key_frames(candidates: &[(f64, Vec<u8>)], max_frames: usize) -> Vec<usize> {
    let total = candidates.len();
    if total == 0 || max_frames == 0 {
        return Vec::new();
    }
    if total <= max_frames {
        return (0..total).collect();
    }
    let scores = change_scores(candidates);
    (0..max_frames)
        .map(|window| {
            let from = window * total / max_frames;
            let to = (window + 1) * total / max_frames;
            (from..to).fold(
                from,
                |best, i| if scores[i] > scores[best] { i } else { best },
            )
        })
        .collect()
}

/// How much each candidate's fingerprint differs from the one before it: `0.0` for the first
/// candidate (nothing to compare it with).
#[cfg(feature = "frames")]
fn change_scores(candidates: &[(f64, Vec<u8>)]) -> Vec<f64> {
    std::iter::once(0.0)
        .chain(
            candidates
                .windows(2)
                .map(|pair| fingerprint_diff(&pair[0].1, &pair[1].1)),
        )
        .collect()
}

/// Mean absolute difference between two same-length byte fingerprints, normalised to 0.0–1.0.
/// `0.0` when they differ in length or are empty (nothing to compare). Also how much the level
/// of light changes between two frames for [`crate::group_clips`]'s cuts.
pub(crate) fn fingerprint_diff(a: &[u8], b: &[u8]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let total: u64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum();
    total as f64 / (a.len() as f64 * 255.0)
}

/// At most this many segments, so the description stays short: one per 30 s, from 3 to 12.
pub fn max_segments(duration_s: f64) -> usize {
    ((duration_s / 30.0) as usize).clamp(3, 12)
}

/// The size of a frame scaled to [`FRAME_LONG_SIDE`] on its long side, aspect kept.
pub fn frame_size(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height).max(1);
    if long <= FRAME_LONG_SIDE {
        return (width, height);
    }
    let scale = |side: u32| {
        ((side as u64 * FRAME_LONG_SIDE as u64 + long as u64 / 2) / long as u64).max(1) as u32
    };
    (scale(width), scale(height))
}

/// Input tokens of one frame: one per 28×28 tile (Anthropic's vision docs, image cost).
pub fn frame_tokens(width: u32, height: u32) -> u64 {
    u64::from(width.div_ceil(28)) * u64::from(height.div_ceil(28))
}

/// Estimated tokens of describing one clip with `model`, before its frames are known: 16:9
/// frames, the subtitles (`subtitle_bytes`, the `.srt` file's size, an upper bound on its
/// text), the instructions, and a typical answer.
pub fn estimate_usage(model: Model, duration_s: f64, subtitle_bytes: usize) -> AiUsage {
    let (w, h) = frame_size(1920, 1080);
    let frames = frame_count(duration_s) as u64;
    AiUsage {
        input_tokens: frames * frame_tokens(w, h)
            + (subtitle_bytes as f64 / CHARS_PER_TOKEN) as u64
            + INSTRUCTION_TOKENS,
        output_tokens: model.answer_tokens,
    }
}

/// One sampled frame: where it is in the clip and its JPEG bytes.
#[derive(Debug, Clone)]
pub struct Frame {
    pub time_s: f64,
    pub jpeg: Vec<u8>,
}

/// The JSON schema of an answer.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "start_s": {"type": "number"},
                        "end_s": {"type": "number"},
                        "description": {"type": "string"}
                    },
                    "required": ["start_s", "end_s", "description"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summary", "segments"],
        "additionalProperties": false
    })
}

/// The request to `model` describing a clip `duration_s` long from its frames and subtitles.
/// `moments` picks the instructions for how many segments to return: see [`MomentsMode`].
pub fn build_request(
    model: Model,
    frames: &[Frame],
    subtitles: &[Cue],
    duration_s: f64,
    language: SummaryLanguage,
    moments: MomentsMode,
) -> AiRequest {
    let has_subtitles = !subtitles.is_empty();
    let moments_instruction = match moments {
        MomentsMode::Important => format!(
            "Answer with a one-sentence summary of the whole clip. Then, at most {} segments \
             (start_s and end_s in seconds, inside 0–{:.0}) for whatever is worth a video \
             editor's attention on its own, in one short sentence each with the key details \
             (place, action, camera, people, objects, on-screen text). An empty list is the \
             right answer when nothing stands out: a static shot, someone talking to camera, or \
             walking while talking, with nothing changing. A small or vague change is not a \
             segment. Add a segment for a stretch that clearly stands out from the rest of the \
             clip (an event, a different activity, something unexpected), and for each part \
             when the clip is made of clearly different parts (a different place, look, shot \
             type or activity) — the editor needs the cut points. Never a segment that covers \
             the whole clip: that is what the summary is for.",
            max_segments(duration_s),
            duration_s,
        ),
        // Byte-for-byte the original wording (before this mode existed): every existing caller
        // that asks for `Full` gets exactly the same request it always did.
        MomentsMode::Full => format!(
            "Answer with a one-sentence summary of the whole clip, then at most {} segments: \
             consecutive stretches of the clip (start_s and end_s in seconds, inside 0–{:.0}) \
             with what happens in each, in one short sentence with the key details (place, \
             action, camera, people, objects, on-screen text). Merge stretches where nothing \
             changes.",
            max_segments(duration_s),
            duration_s,
        ),
    };
    let mut instructions = format!(
        "You describe a video clip for a video editor who has not watched it. It is {} long. \
         You get frames sampled from it, each preceded by its time as t=m:ss{}.\n\
         {moments_instruction}\n\
         Stay factual: describe only what is seen and said. Do not guess who people are.\n{}",
        format_time(duration_s),
        if has_subtitles {
            ", and the clip's subtitles, which may be inaccurate"
        } else {
            ""
        },
        language.instruction(has_subtitles),
    );
    if has_subtitles {
        instructions.push_str("\n\nSubtitles:\n");
        for cue in subtitles {
            instructions.push_str(&format!(
                "[{}–{}] {}\n",
                format_time(cue.start.as_secs_f64()),
                format_time(cue.end.as_secs_f64()),
                cue.text.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
        }
    }
    let mut content = vec![AiContent::Text(instructions)];
    for frame in frames {
        content.push(AiContent::Text(format!("t={}", format_time(frame.time_s))));
        content.push(AiContent::Jpeg(frame.jpeg.clone()));
    }
    AiRequest {
        model: model.id.to_string(),
        content,
        schema: schema(),
        max_tokens: model.max_answer_tokens,
        effort: model.effort,
    }
}

/// Read an answer about a clip `duration_s` long. Segments outside the clip, empty or
/// backwards are dropped, the rest sorted; an answer the model did not finish or with an
/// empty summary is an error with the reason for the failed list. In [`MomentsMode::Important`],
/// a segment covering almost the whole clip is dropped (that is the summary's job) and adjacent
/// segments whose descriptions say the same thing are merged — signs the model tiled the
/// timeline instead of picking out what matters; [`MomentsMode::Full`] skips both.
pub fn parse_answer(
    response: &AiResponse,
    duration_s: f64,
    moments: MomentsMode,
) -> Result<Description, String> {
    match response.stop_reason.as_str() {
        "end_turn" => {}
        "max_tokens" => return Err("The answer was too long".to_string()),
        "refusal" => return Err("The model declined to describe it".to_string()),
        other => return Err(format!("The model stopped early ({other})")),
    }
    let summary = response.json["summary"].as_str().unwrap_or_default().trim();
    if summary.is_empty() {
        return Err("The answer had no summary".to_string());
    }
    // Times a little past the end (the last frame's time rounded up) are the end.
    let end = duration_s + 1.0;
    let mut segments: Vec<Segment> = response.json["segments"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let start_s = item["start_s"].as_f64()?;
                    let end_s = item["end_s"].as_f64()?.min(duration_s);
                    let description = item["description"].as_str()?.trim().to_string();
                    let valid = start_s >= 0.0
                        && end_s > start_s
                        && start_s < end
                        && !description.is_empty();
                    valid.then_some(Segment {
                        start_s,
                        end_s,
                        description,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    segments.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
    if moments == MomentsMode::Important {
        // Merge first: two tiles that individually pass the whole-clip check can merge into one
        // that would not, so the check must see the merged result, never the other way round.
        segments = merge_same_description(segments);
        segments.retain(|s| (s.end_s - s.start_s) < WHOLE_CLIP_FRACTION * duration_s);
    }
    segments.truncate(max_segments(duration_s));
    Ok(Description {
        summary: summary.to_string(),
        segments,
    })
}

/// Merge adjacent segments (already sorted by `start_s`) whose descriptions are the same once
/// trimmed and lower-cased, into one spanning both and keeping the first description — a sign
/// the model tiled the timeline with near-identical sentences instead of picking out what
/// matters. One pass: a tiling model repeats itself between neighbouring stretches, not across
/// the whole answer.
fn merge_same_description(segments: Vec<Segment>) -> Vec<Segment> {
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        let same_as_last = merged.last().is_some_and(|last: &Segment| {
            last.description.trim().to_lowercase() == segment.description.trim().to_lowercase()
        });
        if same_as_last {
            let last = merged.last_mut().expect("checked above");
            last.end_s = last.end_s.max(segment.end_s);
        } else {
            merged.push(segment);
        }
    }
    merged
}

/// `m:ss` below an hour, `h:mm:ss` from an hour on. Seconds are rounded down.
pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(json: Value, stop_reason: &str) -> AiResponse {
        AiResponse {
            json,
            stop_reason: stop_reason.to_string(),
            usage: AiUsage::default(),
        }
    }

    #[test]
    fn frames_every_two_seconds_up_to_sixty() {
        assert_eq!(sample_times(1.0), vec![0.5]);
        assert_eq!(sample_times(10.0), vec![0.0, 2.0, 4.0, 6.0, 8.0]);
        assert_eq!(sample_times(38.0).len(), 19);
        let long = sample_times(600.0);
        assert_eq!(long.len(), 60);
        assert!((long[1] - 10.0).abs() < 1e-9, "evenly spread");
        assert!(sample_times(0.0).is_empty());
        for d in [0.0, 1.0, 10.0, 38.0, 600.0, 1800.0] {
            assert_eq!(frame_count(d), sample_times(d).len(), "{d}");
        }
    }

    #[cfg(feature = "frames")]
    #[test]
    fn candidates_are_four_times_as_dense_and_capped_four_times_as_high() {
        assert_eq!(candidate_times(10.0).len(), 4 * sample_times(10.0).len());
        assert_eq!(candidate_times(0.1), vec![0.05]);
        assert!(candidate_times(0.0).is_empty());
        // A long clip's candidates are still capped, at four times today's frame cap.
        assert_eq!(
            candidate_times(10_000.0).len(),
            MAX_FRAMES * CANDIDATE_OVERSAMPLE
        );
    }

    #[cfg(feature = "frames")]
    #[test]
    fn key_frames_pick_the_biggest_change_in_each_window_and_stay_in_order() {
        // 8 candidates, one point of real change (index 5) inside the second half; two windows
        // of 4 candidates each is the whole budget.
        let flat = vec![0u8; 4];
        let changed = vec![255u8; 4];
        let candidates: Vec<(f64, Vec<u8>)> = (0..8)
            .map(|i| {
                (
                    i as f64,
                    if i == 5 {
                        changed.clone()
                    } else {
                        flat.clone()
                    },
                )
            })
            .collect();
        let chosen = select_key_frames(&candidates, 2);
        assert_eq!(
            chosen,
            vec![0, 5],
            "first window's start, second window's change"
        );
        assert!(
            chosen.windows(2).all(|w| w[0] < w[1]),
            "strictly increasing"
        );
    }

    #[cfg(feature = "frames")]
    #[test]
    fn key_frames_fall_back_to_the_window_start_with_no_change() {
        let flat = vec![0u8; 4];
        let candidates: Vec<(f64, Vec<u8>)> = (0..9).map(|i| (i as f64, flat.clone())).collect();
        // 9 candidates into 3 windows of 3: with nothing to prefer, each window's first index.
        assert_eq!(select_key_frames(&candidates, 3), vec![0, 3, 6]);
    }

    #[cfg(feature = "frames")]
    #[test]
    fn key_frames_never_exceed_the_budget_or_the_candidates() {
        let one = vec![(0.0, vec![1u8, 2, 3])];
        assert_eq!(select_key_frames(&one, 5), vec![0]);
        assert!(select_key_frames(&[], 5).is_empty());
        let ten: Vec<(f64, Vec<u8>)> = (0..10).map(|i| (i as f64, vec![i as u8])).collect();
        assert!(select_key_frames(&ten, 0).is_empty());
        assert_eq!(select_key_frames(&ten, 100), (0..10).collect::<Vec<_>>());
        assert_eq!(select_key_frames(&ten, 4).len(), 4);
    }

    /// However the fingerprints score, no two chosen candidates are further apart in index than
    /// two window widths (the worst case: the last candidate of one window and the first of the
    /// next both picked), the coverage bound the design doc claims.
    #[cfg(feature = "frames")]
    #[test]
    fn key_frames_stay_within_two_window_widths_of_each_other() {
        // Every candidate distinct, so any of them could be a window's pick.
        let candidates: Vec<(f64, Vec<u8>)> =
            (0..97).map(|i| (i as f64, vec![(i % 251) as u8])).collect();
        let max_frames = 11;
        let chosen = select_key_frames(&candidates, max_frames);
        let window = candidates.len().div_ceil(max_frames);
        assert!(
            chosen.windows(2).all(|w| w[1] - w[0] <= 2 * window),
            "{chosen:?} (window {window})"
        );
    }

    #[cfg(feature = "frames")]
    #[test]
    fn fingerprint_diff_is_normalised_and_handles_mismatched_input() {
        assert_eq!(fingerprint_diff(&[0, 0], &[255, 255]), 1.0);
        assert_eq!(fingerprint_diff(&[0, 0], &[0, 0]), 0.0);
        assert_eq!(fingerprint_diff(&[100], &[0]), 100.0 / 255.0);
        assert_eq!(fingerprint_diff(&[], &[]), 0.0, "nothing to compare");
        assert_eq!(fingerprint_diff(&[1, 2], &[1]), 0.0, "different lengths");
    }

    #[test]
    fn frames_keep_their_aspect() {
        assert_eq!(frame_size(1920, 1080), (512, 288));
        assert_eq!(frame_size(1080, 1920), (288, 512));
        assert_eq!(frame_size(320, 240), (320, 240));
        assert_eq!(frame_tokens(512, 288), 209);
    }

    #[test]
    fn segment_cap_is_one_per_half_minute_from_3_to_12() {
        assert_eq!(max_segments(10.0), 3);
        assert_eq!(max_segments(150.0), 5);
        assert_eq!(max_segments(1800.0), 12);
    }

    #[test]
    fn a_stored_model_id_is_read_back_and_an_unknown_one_is_the_default() {
        assert_eq!(Model::from_id("claude-sonnet-5").label, "Claude Sonnet 5");
        assert_eq!(Model::from_id("claude-gone-1"), MODELS[0]);
        assert_eq!(
            MODELS[0].to_string(),
            "Claude Haiku 4.5 ($1 / $5 per M tokens)"
        );
        assert!(MODELS[0].effort.is_none(), "Haiku 4.5 rejects effort");
    }

    #[test]
    fn openai_models_are_in_the_list_with_no_effort_setting() {
        let mini = Model::from_id("gpt-4.1-mini");
        assert_eq!(mini.provider, Provider::OpenAi);
        assert!(mini.effort.is_none(), "GPT-4.1 does not reason");
        let full = Model::from_id("gpt-4.1");
        assert_eq!(full.provider, Provider::OpenAi);
        assert!(
            full.input_usd_per_mtok > mini.input_usd_per_mtok,
            "the stronger one costs more"
        );
        assert_eq!(MODELS[0].provider, Provider::Anthropic, "still the default");
    }

    #[test]
    fn a_thousand_one_minute_clips_cost_about_ten_dollars() {
        let mut usage = AiUsage::default();
        for _ in 0..1000 {
            usage += estimate_usage(MODELS[0], 60.0, 1050);
        }
        let cost = MODELS[0].cost_usd(usage);
        assert!((8.0..12.0).contains(&cost), "{cost}");
    }

    #[test]
    fn the_request_has_instructions_subtitles_and_labelled_frames() {
        let subtitles = [Cue {
            start: std::time::Duration::from_secs(1),
            end: std::time::Duration::from_secs(3),
            text: "Привет,\nмир".to_string(),
        }];
        let frames = vec![
            Frame {
                time_s: 0.0,
                jpeg: vec![1],
            },
            Frame {
                time_s: 2.0,
                jpeg: vec![2],
            },
        ];
        let request = build_request(
            MODELS[0],
            &frames,
            &subtitles,
            4.0,
            SummaryLanguage::SameAsSubtitles,
            MomentsMode::Important,
        );
        assert_eq!(request.model, "claude-haiku-4-5");
        let AiContent::Text(instructions) = &request.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("[0:01–0:03] Привет, мир"));
        assert!(instructions.contains("language of the subtitles"));
        assert_eq!(request.content[3], AiContent::Text("t=0:02".to_string()));
        assert_eq!(request.content[4], AiContent::Jpeg(vec![2]));
        assert_eq!(request.schema["additionalProperties"], false);
        assert_eq!(
            request.schema["properties"]["segments"]["items"]["additionalProperties"],
            false
        );

        let silent = build_request(
            MODELS[0],
            &frames,
            &[],
            4.0,
            SummaryLanguage::SameAsSubtitles,
            MomentsMode::Important,
        );
        let AiContent::Text(instructions) = &silent.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("Write in English."));
        assert!(!instructions.contains("Subtitles:"));
    }

    #[test]
    fn important_mode_asks_for_only_what_stands_out_and_full_mode_asks_to_cover_the_clip() {
        let frames = vec![Frame {
            time_s: 0.0,
            jpeg: vec![1],
        }];
        let important = build_request(
            MODELS[0],
            &frames,
            &[],
            30.0,
            SummaryLanguage::English,
            MomentsMode::Important,
        );
        let AiContent::Text(instructions) = &important.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("An empty list is the right answer"));
        assert!(!instructions.contains("Merge stretches where nothing changes"));

        let full = build_request(
            MODELS[0],
            &frames,
            &[],
            30.0,
            SummaryLanguage::English,
            MomentsMode::Full,
        );
        let AiContent::Text(instructions) = &full.content[0] else {
            panic!("instructions first");
        };
        assert!(instructions.contains("consecutive stretches"));
        assert!(instructions.contains("Merge stretches where nothing changes"));
        assert!(!instructions.contains("An empty list is the right answer"));
    }

    /// `Full` exists so a caller can keep exactly today's request; pinned with an exact string
    /// comparison (not `.contains`) so a future edit to the shared preamble can't silently
    /// reword it, as an earlier draft of this mode did.
    #[test]
    fn full_mode_is_byte_for_byte_the_original_wording() {
        let request = build_request(
            MODELS[0],
            &[],
            &[],
            30.0,
            SummaryLanguage::English,
            MomentsMode::Full,
        );
        let AiContent::Text(instructions) = &request.content[0] else {
            panic!("instructions first");
        };
        assert_eq!(
            instructions,
            "You describe a video clip for a video editor who has not watched it. It is 0:30 \
             long. You get frames sampled from it, each preceded by its time as t=m:ss.\n\
             Answer with a one-sentence summary of the whole clip, then at most 3 segments: \
             consecutive stretches of the clip (start_s and end_s in seconds, inside 0–30) with \
             what happens in each, in one short sentence with the key details (place, action, \
             camera, people, objects, on-screen text). Merge stretches where nothing changes.\n\
             Stay factual: describe only what is seen and said. Do not guess who people are.\n\
             Write in English."
        );
    }

    #[test]
    fn invalid_segments_are_dropped_and_the_rest_sorted() {
        let answer = json!({"summary": " A walk. ", "segments": [
            {"start_s": 10.0, "end_s": 20.0, "description": "Second"},
            {"start_s": 0.0, "end_s": 10.0, "description": "First"},
            {"start_s": 5.0, "end_s": 5.0, "description": "Empty"},
            {"start_s": 50.0, "end_s": 60.0, "description": "Past the end"},
            {"start_s": 20.0, "end_s": 31.0, "description": "Clamped"},
            {"start_s": 1.0, "end_s": 2.0, "description": "  "}
        ]});
        let d = parse_answer(&response(answer, "end_turn"), 30.0, MomentsMode::Full)
            .expect("description");
        assert_eq!(d.summary, "A walk.");
        let names: Vec<&str> = d.segments.iter().map(|s| s.description.as_str()).collect();
        assert_eq!(names, ["First", "Second", "Clamped"]);
        assert_eq!(d.segments[2].end_s, 30.0);
    }

    #[test]
    fn an_empty_summary_or_an_early_stop_fails() {
        let empty = json!({"summary": "", "segments": []});
        assert!(parse_answer(&response(empty, "end_turn"), 10.0, MomentsMode::Important).is_err());
        let fine = json!({"summary": "x", "segments": []});
        assert_eq!(
            parse_answer(
                &response(fine.clone(), "max_tokens"),
                10.0,
                MomentsMode::Important
            ),
            Err("The answer was too long".to_string())
        );
        assert!(parse_answer(&response(fine, "refusal"), 10.0, MomentsMode::Important).is_err());
    }

    #[test]
    fn important_mode_accepts_an_empty_segment_list() {
        let answer = json!({"summary": "A static shot.", "segments": []});
        let d = parse_answer(&response(answer, "end_turn"), 10.0, MomentsMode::Important)
            .expect("description");
        assert!(d.segments.is_empty());
    }

    #[test]
    fn important_mode_drops_a_whole_clip_segment_but_full_mode_keeps_it() {
        let answer = json!({"summary": "A walk.", "segments": [
            {"start_s": 0.0, "end_s": 29.0, "description": "The whole clip"}
        ]});
        let d = parse_answer(
            &response(answer.clone(), "end_turn"),
            30.0,
            MomentsMode::Important,
        )
        .expect("description");
        assert!(d.segments.is_empty(), "{:?}", d.segments);
        let d = parse_answer(&response(answer, "end_turn"), 30.0, MomentsMode::Full)
            .expect("description");
        assert_eq!(d.segments.len(), 1, "full mode keeps it");
    }

    /// The strict `<` in the whole-clip check: a segment at exactly the threshold is dropped
    /// (the issue says "≥ 90%"), one just under it is kept.
    #[test]
    fn the_whole_clip_threshold_is_inclusive_at_exactly_90_percent() {
        let at_threshold = json!({"summary": "A walk.", "segments": [
            {"start_s": 0.0, "end_s": 27.0, "description": "Exactly 90% of 30s"}
        ]});
        let d = parse_answer(
            &response(at_threshold, "end_turn"),
            30.0,
            MomentsMode::Important,
        )
        .expect("description");
        assert!(d.segments.is_empty(), "{:?}", d.segments);

        let just_under = json!({"summary": "A walk.", "segments": [
            {"start_s": 0.0, "end_s": 26.9, "description": "Just under 90% of 30s"}
        ]});
        let d = parse_answer(
            &response(just_under, "end_turn"),
            30.0,
            MomentsMode::Important,
        )
        .expect("description");
        assert_eq!(d.segments.len(), 1, "{:?}", d.segments);
    }

    /// The bug round 1's correctness review found: two tiles that individually pass the
    /// whole-clip check can merge into one that would not — the check must see the merged
    /// result, not run before merging.
    #[test]
    fn important_mode_drops_a_whole_clip_segment_formed_by_merging_two_tiles() {
        let answer = json!({"summary": "A market street.", "segments": [
            {"start_s": 0.0, "end_s": 16.0, "description": "People walk through the market."},
            {"start_s": 16.0, "end_s": 29.0, "description": "people walk through the market."}
        ]});
        let d = parse_answer(&response(answer, "end_turn"), 30.0, MomentsMode::Important)
            .expect("description");
        assert!(d.segments.is_empty(), "got {:?}", d.segments);
    }

    #[test]
    fn important_mode_merges_three_or_more_adjacent_segments_with_the_same_description() {
        let answer = json!({"summary": "A walk.", "segments": [
            {"start_s": 0.0, "end_s": 3.0, "description": "A dog runs by."},
            {"start_s": 3.0, "end_s": 6.0, "description": "A dog runs by."},
            {"start_s": 6.0, "end_s": 9.0, "description": "A dog runs by."},
            {"start_s": 20.0, "end_s": 25.0, "description": "A cat sleeps."}
        ]});
        let d = parse_answer(&response(answer, "end_turn"), 30.0, MomentsMode::Important)
            .expect("description");
        assert_eq!(d.segments.len(), 2, "{:?}", d.segments);
        assert_eq!(d.segments[0].start_s, 0.0);
        assert_eq!(d.segments[0].end_s, 9.0, "all three merged into one");
    }

    #[test]
    fn important_mode_merges_adjacent_segments_with_the_same_description() {
        let answer = json!({"summary": "A walk.", "segments": [
            {"start_s": 0.0, "end_s": 5.0, "description": "A dog runs by."},
            {"start_s": 5.0, "end_s": 10.0, "description": " a dog runs by. "},
            {"start_s": 20.0, "end_s": 25.0, "description": "A cat sleeps."}
        ]});
        let d = parse_answer(&response(answer, "end_turn"), 30.0, MomentsMode::Important)
            .expect("description");
        assert_eq!(d.segments.len(), 2, "{:?}", d.segments);
        assert_eq!(d.segments[0].start_s, 0.0);
        assert_eq!(d.segments[0].end_s, 10.0, "spans both merged segments");
        assert_eq!(
            d.segments[0].description, "A dog runs by.",
            "keeps the first wording"
        );
        assert_eq!(d.segments[1].description, "A cat sleeps.");
    }

    /// Printed with `cargo test important_moments_sample -- --nocapture`, for the PR's sample
    /// output: no live API key is needed since this exercises the prompt text and the validation
    /// rules directly, not a real answer.
    #[test]
    fn important_moments_sample() {
        let frames = vec![Frame {
            time_s: 0.0,
            jpeg: vec![1],
        }];
        let important = build_request(
            MODELS[0],
            &frames,
            &[],
            30.0,
            SummaryLanguage::English,
            MomentsMode::Important,
        );
        let AiContent::Text(important_instructions) = &important.content[0] else {
            panic!("instructions first");
        };
        eprintln!("--- important prompt ---\n{important_instructions}");

        let full = build_request(
            MODELS[0],
            &frames,
            &[],
            30.0,
            SummaryLanguage::English,
            MomentsMode::Full,
        );
        let AiContent::Text(full_instructions) = &full.content[0] else {
            panic!("instructions first");
        };
        eprintln!("--- full prompt ---\n{full_instructions}");

        let static_shot = json!({"summary": "A goat stands in a field, unmoving.", "segments": [
            {"start_s": 0.0, "end_s": 29.0, "description": "A goat stands in a field."}
        ]});
        let d = parse_answer(
            &response(static_shot, "end_turn"),
            30.0,
            MomentsMode::Important,
        )
        .expect("description");
        eprintln!(
            "--- static clip, important mode: whole-clip segment dropped ---\nsummary: {}\nsegments: {:?}",
            d.summary, d.segments
        );

        let tiled = json!({"summary": "A market street.", "segments": [
            {"start_s": 0.0, "end_s": 10.0, "description": "People walk through the market."},
            {"start_s": 10.0, "end_s": 20.0, "description": "people walk through the market."},
            {"start_s": 20.0, "end_s": 25.0, "description": "A vendor weighs saffron for a customer."}
        ]});
        let d = parse_answer(&response(tiled, "end_turn"), 25.0, MomentsMode::Important)
            .expect("description");
        eprintln!(
            "--- tiled answer, important mode: repeated segments merged ---\nsegments: {:?}",
            d.segments
        );
    }

    #[test]
    fn languages_round_trip_through_their_stored_names() {
        for language in SummaryLanguage::ALL {
            assert_eq!(SummaryLanguage::from_name(language.as_str()), language);
        }
        assert_eq!(
            SummaryLanguage::from_name("??"),
            SummaryLanguage::SameAsSubtitles
        );
    }

    #[test]
    fn times_are_minutes_below_an_hour_and_hours_above() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(62.9), "1:02");
        assert_eq!(format_time(3599.0), "59:59");
        assert_eq!(format_time(3600.0), "1:00:00");
        assert_eq!(format_time(3725.0), "1:02:05");
    }
}
