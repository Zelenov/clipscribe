//! Command line: describes each input video with [`clipscribe::describe`] and prints its
//! summary and key moments, or only what it would cost.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, ValueEnum};
use clipscribe::{
    describe, describe_with_tags, estimate_tags_usage, estimate_usage, format_time, frames,
    parse_vocabulary, srt, AiUsage, Described, DescribedWithTags, Error, FrameSampling, Model,
    MomentsMode, Options, Provider, Stage, SummaryLanguage, Tag, MAX_DURATION_S, MODELS,
};
use serde_json::json;

/// Describe what happens in video clips, and when, with Claude.
///
/// Sends frames (key frames by default, at most 60) and the `.srt` next to each video, if there
/// is one, and prints a one-sentence summary and time-ranged key moments.
#[derive(Parser, Debug)]
#[command(name = "clipscribe", version)]
struct Cli {
    /// Video files or folders (a folder means the videos in it).
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Which AI service to use.
    #[arg(long, value_enum, default_value_t = ProviderArg::Anthropic)]
    provider: ProviderArg,

    /// The API key for --provider, or ANTHROPIC_API_KEY / OPENAI_API_KEY.
    #[arg(long)]
    api_key: Option<String>,

    /// The model: with anthropic (default), haiku is the cheapest and fine for most clips,
    /// sonnet and opus notice more; with openai, gpt-4.1-mini is the cheapest, gpt-4.1 notices
    /// more. Defaults to the chosen provider's cheapest model.
    #[arg(long, value_enum)]
    model: Option<ModelArg>,

    /// The language of the descriptions: subtitles (the subtitles' language, English if
    /// none), en, ru, uk, de, es or fr.
    #[arg(long, default_value = "subtitles", value_parser = parse_language)]
    language: SummaryLanguage,

    /// How frames are chosen: keyframes (where the picture changes the most, at most 60) or
    /// interval (one every 2 s, at most 60, spread evenly over a longer clip).
    #[arg(long, value_enum, default_value_t = FramesArg::Keyframes)]
    frames: FramesArg,

    /// Suggest tags for each video from this vocabulary file (one per line, `name — hint`), in
    /// the same request as the description.
    #[arg(long)]
    tags: Option<PathBuf>,

    /// How many moments (segments) a description gets: important (only what stands out, possibly
    /// none) or full (the whole clip in consecutive stretches, today's old behaviour).
    #[arg(long, value_enum, default_value_t = MomentsArg::Important)]
    moments: MomentsArg,

    /// Do not send the `.srt` next to each video.
    #[arg(long)]
    no_subtitles: bool,

    /// Print JSON (an array with one object per video) instead of text.
    #[arg(long)]
    json: bool,

    /// Only print what describing the videos would cost; nothing is sent.
    #[arg(long)]
    estimate: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum ProviderArg {
    Anthropic,
    #[value(name = "openai")]
    OpenAi,
}

impl ProviderArg {
    fn provider(self) -> Provider {
        match self {
            Self::Anthropic => Provider::Anthropic,
            Self::OpenAi => Provider::OpenAi,
        }
    }

    /// The environment variable `--api-key` falls back to, when this provider is chosen.
    fn env_var(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }

    /// This provider's cheapest model, used when `--model` is not given.
    fn default_model(self) -> ModelArg {
        match self {
            Self::Anthropic => ModelArg::Haiku,
            Self::OpenAi => ModelArg::Gpt41Mini,
        }
    }

    /// The exact string `--provider` accepts for this value, straight from clap's own
    /// `ValueEnum` (the same source `--help`'s `[possible values: ...]` reads), so it can never
    /// drift from the `#[value(name = ...)]` override above.
    fn cli_value(self) -> String {
        self.to_possible_value()
            .expect("every ProviderArg variant has a value")
            .get_name()
            .to_string()
    }
}

impl std::fmt::Display for ProviderArg {
    /// The provider's proper name (`Anthropic`, `OpenAI`), for prose; see [`Self::cli_value`]
    /// for the flag's own value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.provider().label())
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModelArg {
    Haiku,
    Sonnet,
    Opus,
    #[value(name = "gpt-4.1-mini")]
    Gpt41Mini,
    #[value(name = "gpt-4.1")]
    Gpt41,
}

impl ModelArg {
    fn id(self) -> &'static str {
        match self {
            Self::Haiku => "claude-haiku-4-5",
            Self::Sonnet => "claude-sonnet-5",
            Self::Opus => "claude-opus-5",
            Self::Gpt41Mini => "gpt-4.1-mini",
            Self::Gpt41 => "gpt-4.1",
        }
    }

    fn model(self) -> Model {
        let id = self.id();
        MODELS.into_iter().find(|m| m.id == id).unwrap_or_default()
    }

    /// The exact string `--model` accepts for this value, straight from clap's own `ValueEnum`
    /// (see [`ProviderArg::cli_value`]).
    fn cli_value(self) -> String {
        self.to_possible_value()
            .expect("every ModelArg variant has a value")
            .get_name()
            .to_string()
    }
}

impl std::fmt::Display for ModelArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.cli_value())
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FramesArg {
    Keyframes,
    Interval,
}

impl FramesArg {
    fn sampling(self) -> FrameSampling {
        match self {
            Self::Keyframes => FrameSampling::KeyFrames,
            Self::Interval => FrameSampling::Interval,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MomentsArg {
    Important,
    Full,
}

impl MomentsArg {
    fn mode(self) -> MomentsMode {
        match self {
            Self::Important => MomentsMode::Important,
            Self::Full => MomentsMode::Full,
        }
    }
}

/// `model_arg`, or `provider_arg`'s own default model when not given; an error when `model_arg`
/// belongs to a different provider than `provider_arg`.
fn resolve_model(model_arg: Option<ModelArg>, provider_arg: ProviderArg) -> Result<Model, String> {
    let model_arg = model_arg.unwrap_or_else(|| provider_arg.default_model());
    let model = model_arg.model();
    if model.provider != provider_arg.provider() {
        return Err(format!(
            "--model {model_arg} is an {} model, not {provider_arg}: drop --model or pick one \
             for --provider {}",
            model.provider.label(),
            provider_arg.cli_value(),
        ));
    }
    Ok(model)
}

fn parse_language(name: &str) -> Result<SummaryLanguage, String> {
    SummaryLanguage::ALL
        .into_iter()
        .find(|l| l.as_str() == name)
        .ok_or_else(|| {
            format!("unknown language {name:?}: use subtitles, en, ru, uk, de, es or fr")
        })
}

/// Extensions of the files a folder input contributes.
const VIDEO_EXTENSIONS: [&str; 12] = [
    "mp4", "mov", "m4v", "mkv", "webm", "avi", "mts", "m2ts", "wmv", "mpg", "mpeg", "3gp",
];

/// Set by Ctrl+C: the video in work stops at its next frame or while waiting for the answer.
static CANCEL: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    let cli = Cli::parse();
    let videos = match videos(&cli.inputs) {
        Ok(videos) if !videos.is_empty() => videos,
        Ok(_) => {
            eprintln!("error: no videos in the inputs");
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let model = match resolve_model(cli.model, cli.provider) {
        Ok(model) => model,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let vocabulary = match &cli.tags {
        Some(path) => match load_vocabulary(path) {
            Ok(vocabulary) => Some(vocabulary),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    if cli.estimate {
        return estimate(&videos, model, cli.no_subtitles, vocabulary.as_deref());
    }
    let api_key = cli
        .api_key
        .clone()
        .or_else(|| std::env::var(cli.provider.env_var()).ok())
        .filter(|k| !k.trim().is_empty());
    let Some(api_key) = api_key else {
        eprintln!(
            "error: no API key: set {} or pass --api-key",
            cli.provider.env_var()
        );
        return ExitCode::from(2);
    };
    let _ = ctrlc::set_handler(|| CANCEL.store(true, Ordering::Relaxed));
    let options = Options {
        api_key: api_key.trim().to_string(),
        model,
        language: cli.language,
        frame_sampling: cli.frames.sampling(),
        moments: cli.moments.mode(),
    };

    let mut results = Vec::new();
    let mut total = AiUsage::default();
    let mut failed = false;
    for video in &videos {
        if CANCEL.load(Ordering::Relaxed) {
            break;
        }
        let subtitles = if cli.no_subtitles {
            Vec::new()
        } else {
            srt::load_for(video).unwrap_or_else(|e| {
                eprintln!("warning: {}: subtitles not read: {e}", video.display());
                Vec::new()
            })
        };
        let progress = Progress::new(video);
        let error = if let Some(vocabulary) = &vocabulary {
            let result =
                describe_with_tags(video, &subtitles, vocabulary, &options, &CANCEL, |stage| {
                    progress.show(stage)
                });
            progress.clear();
            match result {
                Ok(described) => {
                    total += described.usage;
                    if cli.json {
                        results.push(to_json_with_tags(video, &described, model));
                    } else {
                        print_text_with_tags(video, &described, model);
                    }
                    None
                }
                Err(e) => Some(e),
            }
        } else {
            let result = describe(video, &subtitles, &options, &CANCEL, |stage| {
                progress.show(stage)
            });
            progress.clear();
            match result {
                Ok(described) => {
                    total += described.usage;
                    if cli.json {
                        results.push(to_json(video, &described, model));
                    } else {
                        print_text(video, &described, model);
                    }
                    None
                }
                Err(e) => Some(e),
            }
        };
        match error {
            None => {}
            Some(Error::Cancelled) => {
                eprintln!("cancelled: {}", video.display());
                break;
            }
            Some(e) => {
                failed = true;
                if let Error::BadAnswer { usage, .. } = &e {
                    total += *usage;
                }
                eprintln!("error: {}: {e}", video.display());
                // A rejected key or an empty balance would fail every video left the same way.
                if let Error::Ai(ai) = &e {
                    if let Some(stop) = ai.stops_job() {
                        eprintln!("{stop}");
                        break;
                    }
                }
            }
        }
    }
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).unwrap_or_default()
        );
    }
    if total != AiUsage::default() {
        eprintln!(
            "{} in / {} out tokens, about ${:.4} with {}",
            total.input_tokens,
            total.output_tokens,
            model.cost_usd(total),
            model.label
        );
    }
    if failed || CANCEL.load(Ordering::Relaxed) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The videos of `inputs`: files as given, folders as the videos in them, sorted.
fn videos(inputs: &[PathBuf]) -> std::io::Result<Vec<PathBuf>> {
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

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Read a `--tags` vocabulary file: one tag per line, `name — hint`.
fn load_vocabulary(path: &Path) -> Result<Vec<Tag>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let vocabulary = parse_vocabulary(&text);
    if vocabulary.is_empty() {
        return Err(format!("{}: no tags found", path.display()));
    }
    Ok(vocabulary)
}

/// Print what describing `videos` with `model` would cost, reading only their lengths.
/// `vocabulary`, when given, adds the cost of suggesting tags from it.
fn estimate(
    videos: &[PathBuf],
    model: Model,
    no_subtitles: bool,
    vocabulary: Option<&[Tag]>,
) -> ExitCode {
    let mut total = AiUsage::default();
    let mut unreadable = 0;
    for video in videos {
        let Some(duration_s) = frames::clip_duration_s(video) else {
            println!("{}: could not be read", video.display());
            unreadable += 1;
            continue;
        };
        if duration_s > MAX_DURATION_S {
            println!(
                "{}: {}, over the {} limit",
                video.display(),
                format_time(duration_s),
                format_time(MAX_DURATION_S)
            );
            continue;
        }
        let subtitle_bytes = if no_subtitles {
            0
        } else {
            std::fs::metadata(srt::subtitle_path(video)).map_or(0, |m| m.len() as usize)
        };
        let usage = match vocabulary {
            Some(vocabulary) => {
                estimate_tags_usage(model, duration_s, subtitle_bytes, vocabulary, None)
            }
            None => estimate_usage(model, duration_s, subtitle_bytes),
        };
        total += usage;
        println!(
            "{}: {}, about ${:.4}",
            video.display(),
            format_time(duration_s),
            model.cost_usd(usage)
        );
    }
    println!(
        "total: about ${:.4} with {} ({} in / {} out tokens)",
        model.cost_usd(total),
        model.label,
        total.input_tokens,
        total.output_tokens
    );
    if unreadable > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_text(video: &Path, described: &Described, model: Model) {
    println!(
        "{}  {} · {} frames · ${:.4}",
        video.display(),
        format_time(described.duration_s),
        described.frames,
        model.cost_usd(described.usage)
    );
    println!("  {}", described.description.summary);
    for moment in &described.description.segments {
        println!(
            "  {}–{}  {}",
            format_time(moment.start_s),
            format_time(moment.end_s),
            moment.description
        );
    }
    println!();
}

fn to_json(video: &Path, described: &Described, model: Model) -> serde_json::Value {
    json!({
        "file": video.display().to_string(),
        "duration_s": described.duration_s,
        "frames": described.frames,
        "summary": described.description.summary,
        "moments": described.description.segments.iter().map(|m| json!({
            "start_s": m.start_s,
            "end_s": m.end_s,
            "description": m.description,
        })).collect::<Vec<_>>(),
        "model": model.id,
        "usage": {
            "input_tokens": described.usage.input_tokens,
            "output_tokens": described.usage.output_tokens,
        },
        "cost_usd": model.cost_usd(described.usage),
    })
}

fn print_text_with_tags(video: &Path, described: &DescribedWithTags, model: Model) {
    println!(
        "{}  {} · {} frames · ${:.4}",
        video.display(),
        format_time(described.duration_s),
        described.frames,
        model.cost_usd(described.usage)
    );
    println!("  {}", described.description.summary);
    for moment in &described.description.segments {
        println!(
            "  {}–{}  {}",
            format_time(moment.start_s),
            format_time(moment.end_s),
            moment.description
        );
    }
    if !described.tags.tags.is_empty() {
        println!("  Tags:");
        for tag in &described.tags.tags {
            let ranges: Vec<String> = tag
                .ranges
                .iter()
                .map(|r| format!("{}\u{2013}{}", format_time(r.start_s), format_time(r.end_s)))
                .collect();
            let where_ = if ranges.is_empty() {
                String::new()
            } else {
                format!(" ({})", ranges.join(", "))
            };
            println!("    {} {:.0}%{where_}", tag.name, tag.confidence * 100.0);
        }
    }
    if !described.tags.new_tag_ideas.is_empty() {
        println!(
            "  New tag ideas: {}",
            described.tags.new_tag_ideas.join(", ")
        );
    }
    println!();
}

fn to_json_with_tags(
    video: &Path,
    described: &DescribedWithTags,
    model: Model,
) -> serde_json::Value {
    json!({
        "file": video.display().to_string(),
        "duration_s": described.duration_s,
        "frames": described.frames,
        "summary": described.description.summary,
        "moments": described.description.segments.iter().map(|m| json!({
            "start_s": m.start_s,
            "end_s": m.end_s,
            "description": m.description,
        })).collect::<Vec<_>>(),
        "tags": described.tags.tags.iter().map(|t| json!({
            "name": t.name,
            "confidence": t.confidence,
            "ranges": t.ranges.iter().map(|r| json!({
                "start_s": r.start_s,
                "end_s": r.end_s,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "new_tag_ideas": described.tags.new_tag_ideas,
        "model": model.id,
        "usage": {
            "input_tokens": described.usage.input_tokens,
            "output_tokens": described.usage.output_tokens,
        },
        "cost_usd": model.cost_usd(described.usage),
    })
}

/// One status line on stderr for the video in work, when stderr is a terminal.
struct Progress {
    name: String,
    shown: bool,
}

impl Progress {
    fn new(video: &Path) -> Self {
        Self {
            name: video.file_name().map_or_else(
                || video.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            shown: std::io::stderr().is_terminal(),
        }
    }

    fn show(&self, stage: Stage) {
        if !self.shown {
            return;
        }
        let status = match stage {
            Stage::Frame { done, total } => format!("frame {} of {total}", done + 1),
            Stage::Asking => "waiting for Claude".to_string(),
        };
        eprint!("\r{}: {status}\x1b[K", self.name);
    }

    fn clear(&self) {
        if self.shown {
            eprint!("\r\x1b[K");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipscribe::{Description, Segment, TagRange, TagSuggestion, TagSuggestions};

    fn sample() -> DescribedWithTags {
        DescribedWithTags {
            description: Description {
                summary: "Two hikers reach a viewpoint over a valley with goats grazing below."
                    .to_string(),
                segments: vec![Segment {
                    start_s: 12.0,
                    end_s: 20.0,
                    description: "A herd of goats crosses the path in front of the hikers."
                        .to_string(),
                }],
            },
            tags: TagSuggestions {
                tags: vec![
                    TagSuggestion {
                        name: "Goat".to_string(),
                        confidence: 0.95,
                        ranges: vec![TagRange {
                            start_s: 12.0,
                            end_s: 20.0,
                        }],
                    },
                    TagSuggestion {
                        name: "Outdoor".to_string(),
                        confidence: 0.8,
                        ranges: vec![],
                    },
                ],
                new_tag_ideas: vec!["Hiking trail".to_string()],
            },
            usage: AiUsage {
                input_tokens: 3965,
                output_tokens: 210,
            },
            duration_s: 30.0,
            frames: 16,
        }
    }

    /// Printed with `cargo test print_text_with_tags -- --nocapture`, for the PR's sample output:
    /// no live API key is needed since this exercises the CLI's own formatting, not a real answer.
    #[test]
    fn print_text_with_tags_shows_the_summary_segments_tags_and_ideas() {
        print_text_with_tags(Path::new("hike.mp4"), &sample(), MODELS[0]);
    }

    #[test]
    fn to_json_with_tags_carries_the_tags_and_new_tag_ideas_fields() {
        let json = to_json_with_tags(Path::new("hike.mp4"), &sample(), MODELS[0]);
        assert_eq!(json["tags"][0]["name"], "Goat");
        assert_eq!(json["tags"][0]["ranges"][0]["start_s"], 12.0);
        assert_eq!(json["tags"][1]["name"], "Outdoor");
        assert!(json["tags"][1]["ranges"].as_array().unwrap().is_empty());
        assert_eq!(json["new_tag_ideas"], json!(["Hiking trail"]));
        // Unchanged from plain `to_json`, so existing `--json` consumers without `--tags` see no
        // difference: only videos run with `--tags` get these extra fields at all.
        assert!(json.get("summary").is_some());
        assert!(json.get("moments").is_some());
    }

    #[test]
    fn resolve_model_picks_each_providers_own_default_when_none_is_given() {
        let anthropic = resolve_model(None, ProviderArg::Anthropic).expect("default");
        assert_eq!(anthropic.id, "claude-haiku-4-5");
        let openai = resolve_model(None, ProviderArg::OpenAi).expect("default");
        assert_eq!(openai.id, "gpt-4.1-mini");
    }

    #[test]
    fn resolve_model_accepts_a_model_matching_its_provider() {
        let model = resolve_model(Some(ModelArg::Gpt41), ProviderArg::OpenAi).expect("matches");
        assert_eq!(model.id, "gpt-4.1");
    }

    #[test]
    fn resolve_model_rejects_a_model_from_the_other_provider() {
        let error =
            resolve_model(Some(ModelArg::Haiku), ProviderArg::OpenAi).expect_err("mismatch");
        assert!(error.contains("--model haiku"), "{error}");
        assert!(error.contains("Anthropic"), "{error}");
        assert!(error.contains("--provider openai"), "{error}");
    }
}
