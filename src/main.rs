//! Command line: describes each input video with [`clipscribe::describe`] and prints its
//! summary and key moments, or only what it would cost.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::{Parser, ValueEnum};
use clipscribe::{
    describe, describe_moment, describe_with_tags, estimate_tags_usage, estimate_usage,
    format_time, frames, parse_vocabulary, srt, AiUsage, Described, DescribedMoment,
    DescribedWithTags, Error, FrameSampling, Model, MomentsMode, Options, Provider,
    SummaryLanguage, Tag, MAX_DURATION_S, MODELS, MOMENT_WINDOW_S,
};
use serde_json::json;

mod progress;
use progress::{Batch, Outcome, Plan, Totals};

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

    /// No progress bars, plan, summary or token line: only one line per video on stderr, plus
    /// warnings and why a run stopped (and, as ever, the descriptions on stdout). Bars are off
    /// anyway when stderr is not a terminal.
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Only print what describing the videos would cost; nothing is sent.
    #[arg(long)]
    estimate: bool,

    /// Debugging: write the frames sent to the model into this folder (one subfolder per video),
    /// as the same JPEG bytes, named by time, with a frames.json. With --estimate the frames are
    /// read and written but nothing is sent.
    #[arg(long, value_name = "DIR")]
    dump_frames: Option<PathBuf>,

    /// Name and describe the moment at this time (m:ss.f, h:mm:ss.f or plain seconds) in one
    /// video, instead of describing the whole clip. Fast and cheap: one small request.
    #[arg(long, value_parser = parse_at, conflicts_with_all = ["tags", "estimate", "frames", "moments"])]
    at: Option<f64>,

    /// How far around --at to read frames and nearby subtitles from, in seconds each way.
    /// Defaults to a window close enough that the moment is still recognisably the same action,
    /// far enough to show which way it is moving.
    #[arg(long, requires = "at")]
    window: Option<f64>,
}

/// A timestamp: `h:mm:ss.f`, `m:ss.f`, or plain seconds, all with the fraction optional.
fn parse_at(text: &str) -> Result<f64, String> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(format!(
            "unusable time {text:?}: use m:ss.f, h:mm:ss.f or seconds"
        ));
    }
    let mut seconds = 0.0;
    for part in &parts {
        let value: f64 = part
            .parse()
            .map_err(|_| format!("unusable time {text:?}: {part:?} is not a number"))?;
        if value < 0.0 {
            return Err(format!("unusable time {text:?}: negative"));
        }
        seconds = seconds * 60.0 + value;
    }
    Ok(seconds)
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
    if cli.at.is_some() && videos.len() != 1 {
        eprintln!("error: --at takes exactly one video, not {}", videos.len());
        return ExitCode::from(2);
    }
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
        let dump = cli
            .dump_frames
            .as_deref()
            .map(|dir| (dir, cli.frames.sampling()));
        return estimate(
            &videos,
            model,
            cli.no_subtitles,
            vocabulary.as_deref(),
            dump,
        );
    }
    if let Some(dir) = &cli.dump_frames {
        clipscribe::set_debug_frames_dir(Some(dir.clone()));
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

    if let Some(at_s) = cli.at {
        let window_s = cli.window.unwrap_or(MOMENT_WINDOW_S);
        return describe_one_moment(
            &videos[0],
            at_s,
            window_s,
            &options,
            cli.no_subtitles,
            cli.json,
            model,
        );
    }

    let run = plan_run(&videos, model, cli.no_subtitles, vocabulary.as_deref());
    let mut batch = Batch::new(model, videos.len(), run.plan.footage_s, cli.quiet);
    batch.plan(&run.plan);
    let names = display_names(&videos);
    let mut results = Vec::new();
    let mut total = AiUsage::default();
    let mut totals = Totals::default();
    let mut failed = false;
    let mut handled = 0;
    for ((video, clip), name) in videos.iter().zip(&run.clips).zip(&names) {
        if CANCEL.load(Ordering::Relaxed) {
            totals.cancelled = true;
            break;
        }
        if clip.too_long {
            failed = true;
            handled += 1;
            totals.skipped += 1;
            let why = format!("over the {} limit", format_time(MAX_DURATION_S));
            let line = batch.finish_video(name, 0.0, &Outcome::Skipped(why));
            totals.problems.push(line);
            continue;
        }
        let subtitles = if cli.no_subtitles {
            Vec::new()
        } else {
            srt::load_for(video).unwrap_or_else(|e| {
                batch.note(&format!(
                    "warning: {}: subtitles not read: {e}",
                    video.display()
                ));
                Vec::new()
            })
        };
        batch.begin(name);
        let (outcome, error) = if let Some(vocabulary) = &vocabulary {
            match describe_with_tags(video, &subtitles, vocabulary, &options, &CANCEL, |stage| {
                batch.stage(stage)
            }) {
                Ok(described) => {
                    total += described.usage;
                    let outcome =
                        Outcome::of(&described.description, model.cost_usd(described.usage));
                    if cli.json {
                        results.push(to_json_with_tags(video, &described, model));
                    } else {
                        batch.print(|| print_text_with_tags(video, &described, model));
                    }
                    (Some(outcome), None)
                }
                Err(e) => (None, Some(e)),
            }
        } else {
            match describe(video, &subtitles, &options, &CANCEL, |stage| {
                batch.stage(stage)
            }) {
                Ok(described) => {
                    total += described.usage;
                    let outcome =
                        Outcome::of(&described.description, model.cost_usd(described.usage));
                    if cli.json {
                        results.push(to_json(video, &described, model));
                    } else {
                        batch.print(|| print_text(video, &described, model));
                    }
                    (Some(outcome), None)
                }
                Err(e) => (None, Some(e)),
            }
        };
        match (outcome, error) {
            (Some(outcome), _) => {
                if let Outcome::Described { moments, .. } = &outcome {
                    totals.described += 1;
                    totals.moments += moments;
                }
                handled += 1;
                batch.finish_video(name, clip.duration_s, &outcome);
            }
            (None, Some(Error::Cancelled)) => {
                // The video in work was started: it is not "not tried".
                handled += 1;
                totals.cancelled = true;
                break;
            }
            (None, Some(e)) => {
                failed = true;
                handled += 1;
                if let Error::BadAnswer { usage, .. } = &e {
                    total += *usage;
                    batch.add_cost(model.cost_usd(*usage));
                }
                let line =
                    batch.finish_video(name, clip.duration_s, &Outcome::Failed(e.to_string()));
                totals.problems.push(line);
                // A rejected key or an empty balance would fail every video left the same way.
                if let Error::Ai(ai) = &e {
                    if let Some(stop) = ai.stops_job() {
                        batch.note(&stop);
                        totals.stopped = true;
                        break;
                    }
                }
            }
            (None, None) => {}
        }
    }
    totals.cost_usd = model.cost_usd(total);
    totals.cancelled |= CANCEL.load(Ordering::Relaxed);
    totals.not_tried = videos.len().saturating_sub(handled);
    let cancelled = totals.cancelled;
    batch.finish(totals);
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).unwrap_or_default()
        );
    }
    if total != AiUsage::default() && !cli.quiet {
        print_usage(total, model);
    }
    if failed || cancelled {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `--at`: name and describe the moment at `at_s` in `video`, instead of describing a whole clip.
fn describe_one_moment(
    video: &Path,
    at_s: f64,
    window_s: f64,
    options: &Options,
    no_subtitles: bool,
    json: bool,
    model: Model,
) -> ExitCode {
    let subtitles = if no_subtitles {
        Vec::new()
    } else {
        srt::load_for(video).unwrap_or_else(|e| {
            eprintln!("warning: {}: subtitles not read: {e}", video.display());
            Vec::new()
        })
    };
    match describe_moment(video, at_s, window_s, &subtitles, options, &CANCEL) {
        Ok(described) => {
            if json {
                // `--json` prints an array with one object per video (see its own --help text);
                // `--at` only ever runs on one video, but the shape stays an array so a script
                // built for the general case can treat --at output the same way.
                let value = vec![moment_to_json(video, at_s, &described, model)];
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
            } else {
                print_moment(video, at_s, &described, model);
            }
            print_usage(described.usage, model);
            ExitCode::SUCCESS
        }
        Err(Error::Cancelled) => {
            eprintln!("cancelled: {}", video.display());
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {}: {e}", video.display());
            // A bad answer is still billed: say so, like the batch loop does.
            if let Error::BadAnswer { usage, .. } = &e {
                print_usage(*usage, model);
            }
            ExitCode::FAILURE
        }
    }
}

/// The tokens and cost line printed on stderr after a run, for a batch's total or one moment.
fn print_usage(usage: AiUsage, model: Model) {
    eprintln!(
        "{} in / {} out tokens, about ${:.4} with {}",
        usage.input_tokens,
        usage.output_tokens,
        model.cost_usd(usage),
        model.label
    );
}

fn print_moment(video: &Path, at_s: f64, described: &DescribedMoment, model: Model) {
    println!(
        "{}  {} · ${:.4}",
        video.display(),
        format_time(at_s),
        model.cost_usd(described.usage)
    );
    println!("  {}", described.moment.name);
    println!("  {}", described.moment.description);
}

fn moment_to_json(
    video: &Path,
    at_s: f64,
    described: &DescribedMoment,
    model: Model,
) -> serde_json::Value {
    json!({
        "file": video.display().to_string(),
        "at_s": at_s,
        "name": described.moment.name,
        "description": described.moment.description,
        "model": model.id,
        "usage": {
            "input_tokens": described.usage.input_tokens,
            "output_tokens": described.usage.output_tokens,
        },
        "cost_usd": model.cost_usd(described.usage),
    })
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

/// The names result lines use: the file name, or the path as given when two inputs share a file
/// name (`a/clip.mp4` and `b/clip.mp4`), so their lines can be told apart.
fn display_names(videos: &[PathBuf]) -> Vec<String> {
    let file_name = |video: &PathBuf| {
        video.file_name().map_or_else(
            || video.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    };
    videos
        .iter()
        .map(|video| {
            let name = file_name(video);
            if videos
                .iter()
                .filter(|other| file_name(other) == name)
                .count()
                > 1
            {
                video.display().to_string()
            } else {
                name
            }
        })
        .collect()
}

/// What a clip is expected to cost, or why it will not be sent.
enum Estimated {
    Unreadable,
    TooLong(f64),
    Ready { duration_s: f64, usage: AiUsage },
}

/// Read the length of `video` and price describing it.
fn estimate_clip(
    video: &Path,
    model: Model,
    no_subtitles: bool,
    vocabulary: Option<&[Tag]>,
) -> Estimated {
    let Some(duration_s) = frames::clip_duration_s(video) else {
        return Estimated::Unreadable;
    };
    if duration_s > MAX_DURATION_S {
        return Estimated::TooLong(duration_s);
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
    Estimated::Ready { duration_s, usage }
}

/// One clip of a run: its length (0 when it cannot be read) and whether it is over the limit.
struct Clip {
    duration_s: f64,
    too_long: bool,
}

/// The line before a run and what the loop needs from the same look at each clip.
struct RunPlan {
    plan: Plan,
    clips: Vec<Clip>,
}

/// Look at every clip before the run: what to tell the user, and what the loop needs.
fn plan_run(
    videos: &[PathBuf],
    model: Model,
    no_subtitles: bool,
    vocabulary: Option<&[Tag]>,
) -> RunPlan {
    let mut plan = Plan {
        videos: videos.len(),
        footage_s: 0.0,
        cost_usd: 0.0,
        model: model.label,
        over_limit: 0,
        limit_s: MAX_DURATION_S,
        unreadable: 0,
    };
    let mut clips = Vec::new();
    for video in videos {
        clips.push(
            match estimate_clip(video, model, no_subtitles, vocabulary) {
                Estimated::Unreadable => {
                    plan.unreadable += 1;
                    Clip {
                        duration_s: 0.0,
                        too_long: false,
                    }
                }
                Estimated::TooLong(duration_s) => {
                    plan.over_limit += 1;
                    Clip {
                        duration_s,
                        too_long: true,
                    }
                }
                Estimated::Ready { duration_s, usage } => {
                    plan.footage_s += duration_s;
                    plan.cost_usd += model.cost_usd(usage);
                    Clip {
                        duration_s,
                        too_long: false,
                    }
                }
            },
        );
    }
    RunPlan { plan, clips }
}

/// Print what describing `videos` with `model` would cost, reading only their lengths.
/// `vocabulary`, when given, adds the cost of suggesting tags from it.
fn estimate(
    videos: &[PathBuf],
    model: Model,
    no_subtitles: bool,
    vocabulary: Option<&[Tag]>,
    dump: Option<(&Path, FrameSampling)>,
) -> ExitCode {
    let mut total = AiUsage::default();
    let mut unreadable = 0;
    for video in videos {
        let (duration_s, usage) = match estimate_clip(video, model, no_subtitles, vocabulary) {
            Estimated::Unreadable => {
                println!("{}: could not be read", video.display());
                unreadable += 1;
                continue;
            }
            Estimated::TooLong(duration_s) => {
                println!(
                    "{}: {}, over the {} limit",
                    video.display(),
                    format_time(duration_s),
                    format_time(MAX_DURATION_S)
                );
                continue;
            }
            Estimated::Ready { duration_s, usage } => (duration_s, usage),
        };
        total += usage;
        println!(
            "{}: {}, about ${:.4}",
            video.display(),
            format_time(duration_s),
            model.cost_usd(usage)
        );
        if let Some((dir, sampling)) = dump {
            match clipscribe::dump_clip_frames(video, dir, sampling, &CANCEL) {
                Ok(count) => println!("  {count} frames written to {}", dir.display()),
                Err(e) => eprintln!("error: {}: frames not written: {e}", video.display()),
            }
        }
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

/// The suggested In/Out, when the description has one.
fn print_main(description: &clipscribe::Description) {
    if let Some(main) = description.main {
        println!(
            "  Main: {}–{}",
            format_time(main.start_s),
            format_time(main.end_s)
        );
    }
}

fn main_json(description: &clipscribe::Description) -> serde_json::Value {
    match description.main {
        Some(main) => json!({"start_s": main.start_s, "end_s": main.end_s}),
        None => serde_json::Value::Null,
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
    print_main(&described.description);
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
        "main": main_json(&described.description),
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
    print_main(&described.description);
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
        "main": main_json(&described.description),
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
                main: None,
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

    fn sample_moment() -> DescribedMoment {
        DescribedMoment {
            moment: clipscribe::Moment {
                name: "Goat crosses path".to_string(),
                description: "A goat walks in front of the hikers on the trail.".to_string(),
            },
            usage: AiUsage {
                input_tokens: 620,
                output_tokens: 24,
            },
        }
    }

    /// Printed with `cargo test print_moment -- --nocapture`, for the PR's sample output: no live
    /// API key is needed since this exercises the CLI's own formatting, not a real answer.
    #[test]
    fn print_moment_shows_the_name_and_description() {
        print_moment(Path::new("hike.mp4"), 83.4, &sample_moment(), MODELS[0]);
    }

    #[test]
    fn moment_to_json_carries_the_name_description_and_usage() {
        let json = moment_to_json(Path::new("hike.mp4"), 83.4, &sample_moment(), MODELS[0]);
        assert_eq!(json["at_s"], 83.4);
        assert_eq!(json["name"], "Goat crosses path");
        assert_eq!(json["usage"]["input_tokens"], 620);
    }

    /// Printed with `cargo test moment_to_json_pretty -- --nocapture`, for the PR's `--at --json`
    /// sample output: no live API key is needed since this exercises the CLI's own formatting,
    /// not a real answer. `--at --json` wraps the one moment in a one-element array, like
    /// `--json` does for every video in the general case.
    #[test]
    fn moment_to_json_pretty_prints_like_the_cli_does() {
        let json = vec![moment_to_json(
            Path::new("hike.mp4"),
            83.4,
            &sample_moment(),
            MODELS[0],
        )];
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
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
    fn the_main_range_is_printed_and_in_the_json_and_null_when_absent() {
        let mut described = sample();
        assert_eq!(main_json(&described.description), serde_json::Value::Null);
        described.description.main = Some(clipscribe::MainRange {
            start_s: 7.0,
            end_s: 28.5,
        });
        let json = to_json_with_tags(Path::new("hike.mp4"), &described, MODELS[0]);
        assert_eq!(json["main"], json!({"start_s": 7.0, "end_s": 28.5}));
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

    #[test]
    fn parse_at_reads_seconds_mmss_and_hhmmss() {
        assert_eq!(parse_at("5"), Ok(5.0));
        assert_eq!(parse_at("5.5"), Ok(5.5));
        assert_eq!(parse_at("1:23.4"), Ok(83.4));
        assert_eq!(parse_at("1:02:03"), Ok(3723.0));
    }

    #[test]
    fn parse_at_rejects_nonsense() {
        assert!(parse_at("").is_err());
        assert!(parse_at("abc").is_err());
        assert!(parse_at("1:2:3:4").is_err());
        assert!(parse_at("1::3").is_err());
        assert!(parse_at("-5").is_err());
        assert!(parse_at("1:-5").is_err());
    }

    #[test]
    fn inputs_with_the_same_file_name_are_told_apart_by_their_path() {
        let videos = [
            PathBuf::from("a/clip.mp4"),
            PathBuf::from("b/clip.mp4"),
            PathBuf::from("c/other.mp4"),
        ];
        let names = display_names(&videos);
        assert_eq!(names[0], PathBuf::from("a/clip.mp4").display().to_string());
        assert_eq!(names[1], PathBuf::from("b/clip.mp4").display().to_string());
        assert_eq!(names[2], "other.mp4");
    }
}
