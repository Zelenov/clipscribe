//! Command line: describes each input video with [`clipscribe::describe`] and prints its
//! summary and key moments, or only what it would cost.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, ValueEnum};
use clipscribe::{
    describe, estimate_usage, format_time, frames, srt, AiUsage, Described, Error, FrameSampling,
    Model, Options, Stage, SummaryLanguage, MAX_DURATION_S, MODELS,
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

    /// Anthropic API key.
    #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    /// The model: haiku is the cheapest and fine for most clips; sonnet and opus notice more.
    #[arg(long, value_enum, default_value_t = ModelArg::Haiku)]
    model: ModelArg,

    /// The language of the descriptions: subtitles (the subtitles' language, English if
    /// none), en, ru, uk, de, es or fr.
    #[arg(long, default_value = "subtitles", value_parser = parse_language)]
    language: SummaryLanguage,

    /// How frames are chosen: keyframes (where the picture changes the most, at most 60) or
    /// interval (one every 2 s, at most 60, spread evenly over a longer clip).
    #[arg(long, value_enum, default_value_t = FramesArg::Keyframes)]
    frames: FramesArg,

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

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModelArg {
    Haiku,
    Sonnet,
    Opus,
}

impl ModelArg {
    fn model(self) -> Model {
        let id = match self {
            Self::Haiku => "claude-haiku-4-5",
            Self::Sonnet => "claude-sonnet-5",
            Self::Opus => "claude-opus-5",
        };
        MODELS.into_iter().find(|m| m.id == id).unwrap_or_default()
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
    let model = cli.model.model();
    if cli.estimate {
        return estimate(&videos, model, cli.no_subtitles);
    }
    let Some(api_key) = cli.api_key.clone().filter(|k| !k.trim().is_empty()) else {
        eprintln!("error: no API key: set ANTHROPIC_API_KEY or pass --api-key");
        return ExitCode::from(2);
    };
    let _ = ctrlc::set_handler(|| CANCEL.store(true, Ordering::Relaxed));
    let options = Options {
        api_key: api_key.trim().to_string(),
        model,
        language: cli.language,
        frame_sampling: cli.frames.sampling(),
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
            }
            Err(Error::Cancelled) => {
                eprintln!("cancelled: {}", video.display());
                break;
            }
            Err(e) => {
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

/// Print what describing `videos` with `model` would cost, reading only their lengths.
fn estimate(videos: &[PathBuf], model: Model, no_subtitles: bool) -> ExitCode {
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
        let usage = estimate_usage(model, duration_s, subtitle_bytes);
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
