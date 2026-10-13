//! Command line: describes the input videos with [`clipscribe::describe_folder`] (several at
//! once, optionally resuming from a cache and grouping similar footage) and prints each one's
//! summary and key moments, or only what it would cost.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use clap::{ArgGroup, Parser, ValueEnum};
use clipscribe::{
    cache_path, describe_folder, describe_moment, estimate_tags_usage, estimate_usage, find_videos,
    format_time, frames, group_clips, parse_vocabulary, serve_after_stop, srt, AiUsage, Budget,
    Cache, ClipGroups, ClipOutcome, ClipRecord, DescribedMoment, Error, FolderEvent, FrameSampling,
    GroupOptions, Grouping, Model, MomentsMode, Options, Provider, RunOptions, Stop,
    SummaryLanguage, Tag, DEFAULT_JOBS, MAX_DURATION_S, MODELS, MOMENT_WINDOW_S,
};
use serde_json::json;

mod progress;
use progress::{Batch, Outcome, Plan, Totals};

/// Describe what happens in video clips, and when, with Claude.
///
/// Sends frames (key frames by default, at most 60) and the `.srt` next to each video, if there
/// is one, and prints a one-sentence summary and time-ranged key moments. Several videos are
/// described at once (--jobs); results print in input order.
#[derive(Parser, Debug)]
#[command(name = "clipscribe", version)]
#[command(group(ArgGroup::new("cache").args(["resume", "force"])))]
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

    /// Print JSON (an array with one object per video) instead of text. A video from the cache
    /// (`"cached": true`) shows the `usage` and `cost_usd` of when it was described, not what
    /// this run spent: the usage line on stderr counts only this run.
    #[arg(long)]
    json: bool,

    /// No progress bars, plan or summary: only one line per video on stderr, plus warnings and
    /// why a run stopped (and, as ever, the descriptions on stdout). A stderr that is not a
    /// terminal (piped, CI) gets the same lines; `--quiet` also drops the token line, which a
    /// piped stderr keeps.
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Only print what describing the videos would cost; nothing is sent.
    #[arg(long, conflicts_with_all = FOLDER_RUN_ARGS)]
    estimate: bool,

    /// Skip videos already described with the same settings in the cache
    /// (`.clipscribe-cache.jsonl` next to them), and add each newly described one to it as soon
    /// as it is done, so a stopped run picks up where it left off.
    #[arg(long)]
    resume: bool,

    /// Describe every video again, even those in the cache, and write the new results to it.
    #[arg(long)]
    force: bool,

    /// Keep the cache (see --resume) in this directory instead of next to the videos.
    #[arg(long, requires = "cache")]
    cache_dir: Option<PathBuf>,

    /// Group similar footage: videos, and stretches within them, that look like the same shot
    /// (duplicates, re-exports, a clip stored sideways, a camera that did not move). Each group
    /// gets a short label; nothing extra is sent. `--groups=words` also joins videos whose
    /// descriptions share many words (the same subject, even after the camera moved): EXPERIMENTAL
    /// — its threshold was not tuned on real descriptions, it can join unrelated videos described
    /// in similar everyday words and miss the same subject reworded; check the groups.
    #[arg(
        long,
        value_enum,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "pictures",
        value_name = "BY"
    )]
    groups: Option<GroupsArg>,

    /// How many videos are in work at once.
    #[arg(long, default_value_t = DEFAULT_JOBS as u16, value_parser = clap::value_parser!(u16).range(1..))]
    jobs: u16,

    /// Stop before a request could take this run's spending past this many US dollars (videos
    /// found in the cache cost nothing). Each request sets aside several times its likely cost
    /// (its answer counted at full length) before it is sent, so a cap close to --estimate's
    /// total can stop several videos early.
    #[arg(long, value_parser = parse_cost)]
    max_cost: Option<f64>,
    /// Debugging: write the frames sent to the model into this folder (one subfolder per video),
    /// as the same JPEG bytes, named by time, with a frames.json. With --estimate the frames are
    /// read and written but nothing is sent.
    #[arg(long, value_name = "DIR")]
    dump_frames: Option<PathBuf>,

    /// Name and describe the moment at this time (m:ss.f, h:mm:ss.f or plain seconds) in one
    /// video, instead of describing the whole clip. Fast and cheap: one small request.
    #[arg(
        long,
        value_parser = parse_at,
        conflicts_with_all = [
            "tags", "estimate", "frames", "moments",
            "resume", "force", "cache_dir", "groups", "jobs", "max_cost",
        ],
    )]
    at: Option<f64>,

    /// How far around --at to read frames and nearby subtitles from, in seconds each way.
    /// Defaults to a window close enough that the moment is still recognisably the same action,
    /// far enough to show which way it is moving.
    #[arg(long, requires = "at")]
    window: Option<f64>,

    /// Find the shots of an edited video instead of describing it: where the picture cuts,
    /// dissolves or fades. Reads each video once on this computer; sends nothing, needs no API
    /// key, costs nothing. With --json, an array with `shots` and `transitions` per video. A video that cannot be read is named on
    /// stderr and the exit code is 1; the others are still listed.
    #[arg(long, conflicts_with_all = SHOTS_CONFLICTS)]
    shots: bool,
}

/// The options that describe or price a video, which `--shots` (local, nothing sent) does not take.
const SHOTS_CONFLICTS: [&str; 18] = [
    "provider",
    "api_key",
    "model",
    "language",
    "frames",
    "tags",
    "moments",
    "no_subtitles",
    "estimate",
    "resume",
    "force",
    "cache_dir",
    "groups",
    "jobs",
    "max_cost",
    "dump_frames",
    "at",
    "window",
];

/// The options of a run over whole clips, which neither --at nor --estimate takes.
const FOLDER_RUN_ARGS: [&str; 6] = ["resume", "force", "cache_dir", "groups", "jobs", "max_cost"];

/// A --max-cost amount: US dollars, more than zero.
fn parse_cost(text: &str) -> Result<f64, String> {
    match text.trim().trim_start_matches('$').parse::<f64>() {
        Ok(usd) if usd.is_finite() && usd > 0.0 => Ok(usd),
        _ => Err(format!(
            "unusable amount {text:?}: use US dollars above zero, e.g. 5 or 0.50"
        )),
    }
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

/// What `--groups` compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum GroupsArg {
    /// The pictures of the frames already sent.
    Pictures,
    /// The pictures, and the words of the descriptions (experimental).
    Words,
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

/// Set by Ctrl+C: the videos in work stop at their next frame or while waiting for the answer.
static CANCEL: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    let cli = Cli::parse();
    let videos = match find_videos(&cli.inputs) {
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
    if cli.shots {
        return run_shots(&videos, cli.json);
    }
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

    let caching = cli.resume || cli.force;
    let mut batches = Vec::new();
    for (path, range) in batches_by_cache(&videos, caching, cli.cache_dir.as_deref()) {
        let cache = match path.map(|path| Cache::open(&path).map_err(|e| (path, e))) {
            None => None,
            Some(Ok(cache)) => Some(cache),
            Some(Err((path, e))) => {
                eprintln!("error: cache {}: {e}", path.display());
                return ExitCode::from(2);
            }
        };
        batches.push((cache, range));
    }
    let run_plan = plan_run(&videos, model, cli.no_subtitles, vocabulary.as_deref());
    let batch = Batch::new(model, videos.len(), run_plan.plan.footage_s, cli.quiet);
    batch.plan(&run_plan.plan);
    let run = RunOptions {
        jobs: usize::from(cli.jobs),
        force: cli.force,
        subtitles: !cli.no_subtitles,
        vocabulary,
    };
    let budget = Budget::new(cli.max_cost);
    let status = Mutex::new(Status::new(
        batch,
        display_names(&videos),
        run_plan.clips.iter().map(|c| c.duration_s).collect(),
        model,
        !cli.json,
    ));

    let mut outcomes: Vec<ClipOutcome> = Vec::with_capacity(videos.len());
    let mut total = AiUsage::default();
    let mut stopped = None;
    for (cache, range) in &batches {
        let offset = range.start;
        if let Some(stop) = &stopped {
            // An earlier folder stopped the run: this one's cache is still served (not after a
            // cancel), the rest is not started.
            outcomes.extend(serve_after_stop(
                range
                    .clone()
                    .map(|index| (index - offset, videos[index].as_path())),
                cache.as_ref(),
                &run,
                &options,
                stop,
                &CANCEL,
                &|event| lock(&status).on_event(offset, event),
            ));
            continue;
        }
        let folder_run = describe_folder(
            &videos[range.clone()],
            cache.as_ref(),
            &run,
            &options,
            &budget,
            &CANCEL,
            |event| lock(&status).on_event(offset, event),
        );
        total += folder_run.usage;
        stopped = folder_run.stopped;
        outcomes.extend(folder_run.clips);
    }
    if CANCEL.load(Ordering::Relaxed) {
        // Ctrl+C overrides whatever stopped the run first, as in `describe_folder`.
        stopped = Some(Stop::Cancelled);
    }
    let mut status = status.into_inner().unwrap_or_else(|e| e.into_inner());

    let not_done = outcomes.iter().filter(|o| o.record().is_none()).count();
    match &stopped {
        Some(Stop::OverBudget) => {
            let described_now = outcomes
                .iter()
                .filter(|o| matches!(o, ClipOutcome::Described(_)))
                .count();
            status.batch.note(&budget_stop_message(
                budget.max_usd().unwrap_or_default(),
                budget.spent_usd(),
                budget.refused_usd(),
                not_done,
                videos.len(),
                described_now,
                caching,
            ));
            status.totals.stopped = true;
        }
        Some(stop @ Stop::Job(_)) => {
            status.batch.note(&stop.to_string());
            status.totals.stopped = true;
        }
        // Each cancelled video is said as it stops.
        Some(Stop::Cancelled) => status.totals.cancelled = true,
        None => {}
    }
    let Status {
        mut batch,
        mut totals,
        ..
    } = status;
    totals.cost_usd = model.cost_usd(total);
    totals.not_tried = outcomes
        .iter()
        .filter(|o| matches!(o, ClipOutcome::NotStarted | ClipOutcome::OverBudget))
        .count();
    batch.finish(totals);

    let described: Vec<(usize, &ClipRecord)> = outcomes
        .iter()
        .enumerate()
        .filter_map(|(i, outcome)| outcome.record().map(|record| (i, record)))
        .collect();
    let grouping = cli.groups.map(|by| {
        let clips: Vec<_> = described.iter().map(|(_, record)| &record.clip).collect();
        group_clips(
            &clips,
            &GroupOptions {
                words: by == GroupsArg::Words,
            },
        )
    });
    if cli.json {
        let results: Vec<serde_json::Value> = described
            .iter()
            .enumerate()
            .map(|(n, (i, record))| {
                let groups = grouping.as_ref().map(|g| (g, &g.clips[n]));
                clip_to_json(
                    &videos[*i],
                    record,
                    matches!(outcomes[*i], ClipOutcome::Cached(_)),
                    groups,
                )
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&results).unwrap_or_default()
        );
    } else if let Some(grouping) = grouping.as_ref().filter(|g| !g.groups.is_empty()) {
        let videos: Vec<&Path> = described
            .iter()
            .map(|(i, _)| videos[*i].as_path())
            .collect();
        print_groups(&videos, grouping);
    }

    if total != AiUsage::default() && !cli.quiet {
        print_usage(total, model);
    }
    let cached = outcomes
        .iter()
        .filter(|o| matches!(o, ClipOutcome::Cached(_)))
        .count();
    if cached > 0 {
        eprintln!("{cached} from the cache, nothing sent for them");
    }
    if not_done > 0 || CANCEL.load(Ordering::Relaxed) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `--shots`: the shots of each video, as text or as a JSON array. A video that cannot be read
/// is reported on stderr and the exit code is a failure, but the others are still done.
fn run_shots(videos: &[PathBuf], json: bool) -> ExitCode {
    let _ = ctrlc::set_handler(|| CANCEL.store(true, Ordering::Relaxed));
    let mut failed = false;
    let mut results = Vec::new();
    for video in videos {
        if CANCEL.load(Ordering::Relaxed) {
            failed = true;
            break;
        }
        match clipscribe::detect_shots(video, &CANCEL) {
            Ok(shots) if json => results.push(shots_to_json(video, &shots)),
            Ok(shots) => print_shots(video, &shots),
            Err(e) => {
                eprintln!("error: {}: {e}", video.display());
                failed = true;
            }
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&results).unwrap_or_default()
        );
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn print_shots(video: &Path, shots: &clipscribe::shots::Shots) {
    let name = video.file_name().unwrap_or(video.as_os_str());
    println!(
        "{}  {} · {}",
        name.to_string_lossy(),
        plural(shots.shots.len(), "shot"),
        plural(shots.transitions.len(), "transition"),
    );
    for (n, shot) in shots.shots.iter().enumerate() {
        let enters = shot.enters_by.map_or("start", |kind| kind.name());
        println!(
            "  {:>3}  {:>7.2} – {:>7.2} s  {enters}",
            n + 1,
            shot.start_s,
            shot.end_s
        );
    }
}

fn shots_to_json(video: &Path, shots: &clipscribe::shots::Shots) -> serde_json::Value {
    json!({
        "video": video.to_string_lossy(),
        "shots": shots.shots.iter().map(|s| json!({
            "start_s": s.start_s,
            "end_s": s.end_s,
            "enters_by": s.enters_by.map(|kind| kind.name()),
        })).collect::<Vec<_>>(),
        "transitions": shots.transitions.iter().map(|t| json!({
            "kind": t.kind.name(),
            "start_s": t.start_s,
            "end_s": t.end_s,
        })).collect::<Vec<_>>(),
    })
}

/// What to say when a run stopped at `--max-cost`, and what to do next: `spent_usd` is what this
/// run spent, `next_usd` the bound of the video that did not fit (see [`Budget::refused_usd`]),
/// `not_done` of `total` videos are left, `described_now` were described (and paid for) by this
/// run, and `caching` says whether they were saved to the cache (`--resume` or `--force`) for the
/// next run to skip.
fn budget_stop_message(
    max_usd: f64,
    spent_usd: f64,
    next_usd: Option<f64>,
    not_done: usize,
    total: usize,
    described_now: usize,
    caching: bool,
) -> String {
    let why = match next_usd {
        Some(next) if next > max_usd => format!(
            " {} spent; the next video could cost up to {}, more than the whole cap (that is its \
             worst case; it usually costs a fraction of it). Each video needs that much room \
             before it is sent.",
            dollars(spent_usd),
            dollars(next)
        ),
        Some(next) => format!(
            " {} spent; the next video could cost up to {} (its answer counted at full length, \
             though it usually costs a fraction of that), which would pass the cap. Each video \
             needs several times its usual cost set aside before it is sent, so a cap close to \
             --estimate's total can stop several videos early.",
            dollars(spent_usd),
            dollars(next)
        ),
        None => format!(" {} spent.", dollars(spent_usd)),
    };
    let stopped = format!(
        "Stopped at --max-cost {}: {not_done} of {total} videos not described.{why}",
        dollars(max_usd)
    );
    if caching {
        format!("{stopped} Raise --max-cost and run again with --resume to continue.")
    } else if described_now > 0 {
        format!(
            "{stopped} The {described_now} described in this run were not saved (no --resume), \
             so running again pays for them again too: raise --max-cost and add --resume."
        )
    } else {
        format!("{stopped} Raise --max-cost, and add --resume to save each video as it is done.")
    }
}

/// US dollars with as many decimals as they need, from 2 to 4: `$5.00`, `$0.03`, `$0.0143`.
fn dollars(usd: f64) -> String {
    let text = format!("{usd:.4}");
    let (whole, cents) = text.split_once('.').unwrap_or((&text, "0000"));
    let cents = cents.trim_end_matches('0');
    format!("${whole}.{cents:0<2}")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `videos` split into runs sharing one cache file, in order: without `caching`, one run and no
/// cache; with `cache_dir`, one run and the cache there; otherwise one run per folder (of
/// consecutive videos in the same one), each with the cache next to its videos.
fn batches_by_cache(
    videos: &[PathBuf],
    caching: bool,
    cache_dir: Option<&Path>,
) -> Vec<(Option<PathBuf>, Range<usize>)> {
    if !caching {
        return vec![(None, 0..videos.len())];
    }
    if let Some(dir) = cache_dir {
        return vec![(Some(cache_path(Path::new("."), Some(dir))), 0..videos.len())];
    }
    let folder_of = |video: &Path| {
        video
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf()
    };
    let mut batches: Vec<(Option<PathBuf>, Range<usize>)> = Vec::new();
    for (i, video) in videos.iter().enumerate() {
        let path = cache_path(&folder_of(video), None);
        match batches.last_mut() {
            Some((Some(last), range)) if *last == path => range.end = i + 1,
            _ => batches.push((Some(path), i..i + 1)),
        }
    }
    batches
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

/// One clip of a run: its length (0 when it cannot be read).
struct Clip {
    duration_s: f64,
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
                    Clip { duration_s: 0.0 }
                }
                Estimated::TooLong(duration_s) => {
                    plan.over_limit += 1;
                    Clip { duration_s }
                }
                Estimated::Ready { duration_s, usage } => {
                    plan.footage_s += duration_s;
                    plan.cost_usd += model.cost_usd(usage);
                    Clip { duration_s }
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
            "  Main: {}\u{2013}{}",
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

/// A described clip as text: the header line (length, frames, cost, whether it came from the
/// cache), the summary, the moments and, when a vocabulary was used, the tags.
fn print_clip(video: &Path, record: &ClipRecord, cached: bool) {
    let clip = &record.clip;
    let model = Model::from_id(&record.model);
    println!(
        "{}  {} · {} frames · ${:.4}{}",
        video.display(),
        format_time(clip.duration_s),
        clip.frames.len(),
        model.cost_usd(clip.usage),
        if cached { " · cached" } else { "" }
    );
    println!("  {}", clip.description.summary);
    print_main(&clip.description);
    for moment in &clip.description.segments {
        println!(
            "  {}–{}  {}",
            format_time(moment.start_s),
            format_time(moment.end_s),
            moment.description
        );
    }
    if let Some(tags) = &clip.tags {
        if !tags.tags.is_empty() {
            println!("  Tags:");
            for tag in &tags.tags {
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
        if !tags.new_tag_ideas.is_empty() {
            println!("  New tag ideas: {}", tags.new_tag_ideas.join(", "));
        }
    }
    println!();
}

/// --groups as text, after the clips: each group's label, then its footage, one video a line
/// with the time ranges it shows the group in.
fn print_groups(videos: &[&Path], grouping: &Grouping) {
    println!("Groups:");
    for group in &grouping.groups {
        println!("  {}  {}", group.id, group.label);
        for (video, clip) in videos.iter().zip(&grouping.clips) {
            let ranges: Vec<String> = clip
                .stretches
                .iter()
                .filter(|s| s.group == group.id)
                .map(|s| format!("{}–{}", format_time(s.start_s), format_time(s.end_s)))
                .collect();
            if !ranges.is_empty() {
                println!("     {}  {}", video.display(), ranges.join(", "));
            }
        }
    }
    println!();
}

/// A described clip as JSON; `tags` and `new_tag_ideas` only when a vocabulary was used,
/// `cached` only when it came from the cache (its `usage` and `cost_usd` are then what it cost when
/// it was described, not anything this run spent), and with --groups (`groups`: the whole grouping and
/// this clip's part of it) `group`, `group_label`, `stretches` and each moment's `group`.
fn clip_to_json(
    video: &Path,
    record: &ClipRecord,
    cached: bool,
    groups: Option<(&Grouping, &ClipGroups)>,
) -> serde_json::Value {
    let clip = &record.clip;
    let model = Model::from_id(&record.model);
    let label = |id: usize| {
        groups
            .and_then(|(g, _)| g.group(id))
            .map(|g| g.label.as_str())
    };
    let mut value = json!({
        "file": video.display().to_string(),
        "duration_s": clip.duration_s,
        "frames": clip.frames.len(),
        "summary": clip.description.summary,
        "main": main_json(&clip.description),
        "moments": clip.description.segments.iter().enumerate().map(|(n, m)| {
            let mut moment = json!({
                "start_s": m.start_s,
                "end_s": m.end_s,
                "description": m.description,
            });
            if let Some((_, clip_groups)) = groups {
                moment["group"] = json!(clip_groups.segments.get(n));
            }
            moment
        }).collect::<Vec<_>>(),
    });
    if let Some(tags) = &clip.tags {
        value["tags"] = json!(tags
            .tags
            .iter()
            .map(|t| json!({
                "name": t.name,
                "confidence": t.confidence,
                "ranges": t.ranges.iter().map(|r| json!({
                    "start_s": r.start_s,
                    "end_s": r.end_s,
                })).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>());
        value["new_tag_ideas"] = json!(tags.new_tag_ideas);
    }
    if let Some((_, clip_groups)) = groups {
        value["group"] = json!(clip_groups.group);
        value["group_label"] = json!(label(clip_groups.group));
        value["stretches"] = json!(clip_groups
            .stretches
            .iter()
            .map(|s| json!({
                "start_s": s.start_s,
                "end_s": s.end_s,
                "group": s.group,
                "label": label(s.group),
            }))
            .collect::<Vec<_>>());
    }
    value["model"] = json!(model.id);
    value["usage"] = json!({
        "input_tokens": clip.usage.input_tokens,
        "output_tokens": clip.usage.output_tokens,
    });
    value["cost_usd"] = json!(model.cost_usd(clip.usage));
    if cached {
        value["cached"] = json!(true);
    }
    value
}

/// What a run shows while it goes: the display of [`Batch`] (a line per video in work, a result
/// line per video as it ends, one bar for the run) and each clip's text (unless the output is
/// JSON) in input order as soon as every earlier clip is done.
struct Status {
    batch: Batch,
    names: Vec<String>,
    /// Each video's length in seconds, 0 when it could not be read.
    durations: Vec<f64>,
    model: Model,
    print_text: bool,
    totals: Totals,
    /// The next clip (by input index) to print.
    next: usize,
    /// Finished clips waiting for an earlier one.
    finished: BTreeMap<usize, (PathBuf, ClipOutcome)>,
}

impl Status {
    fn new(
        batch: Batch,
        names: Vec<String>,
        durations: Vec<f64>,
        model: Model,
        print_text: bool,
    ) -> Self {
        Self {
            batch,
            names,
            durations,
            model,
            print_text,
            totals: Totals::default(),
            next: 0,
            finished: BTreeMap::new(),
        }
    }

    /// `event` of the run over the videos from `offset` on.
    fn on_event(&mut self, offset: usize, event: FolderEvent<'_>) {
        match event {
            FolderEvent::Started { index, .. } => {
                let id = offset + index;
                self.batch.begin(id, &self.names[id]);
            }
            FolderEvent::Stage { index, stage, .. } => self.batch.stage(offset + index, stage),
            FolderEvent::Warning { video, message, .. } => {
                self.batch
                    .note(&format!("warning: {}: {message}", video.display()));
            }
            FolderEvent::Finished {
                index,
                video,
                outcome,
            } => {
                let id = offset + index;
                self.end_video(id, outcome);
                self.finished
                    .insert(id, (video.to_path_buf(), outcome.clone()));
                self.print_ready();
            }
        }
    }

    /// Show how video `id` ended and count it for the summary.
    fn end_video(&mut self, id: usize, outcome: &ClipOutcome) {
        let (name, duration_s) = (self.names[id].clone(), self.durations[id]);
        let model = self.model;
        let line = match outcome {
            ClipOutcome::Described(record) | ClipOutcome::Cached(record) => {
                // A clip from the cache cost nothing in this run.
                let cost = if matches!(outcome, ClipOutcome::Cached(_)) {
                    0.0
                } else {
                    model.cost_usd(record.clip.usage)
                };
                self.totals.described += 1;
                self.totals.moments += record.clip.description.segments.len();
                self.batch.finish_video(
                    id,
                    &name,
                    duration_s,
                    &Outcome::of(&record.clip.description, cost),
                )
            }
            ClipOutcome::Failed(Error::Cancelled) => {
                // The video in work was started: it is not "not tried".
                self.totals.cancelled = true;
                self.batch.abandon(id);
                return;
            }
            ClipOutcome::Failed(Error::TooLong(_)) => {
                self.totals.skipped += 1;
                let why = format!("over the {} limit", format_time(MAX_DURATION_S));
                self.batch
                    .finish_video(id, &name, 0.0, &Outcome::Skipped(why))
            }
            ClipOutcome::Failed(e) => {
                if let Error::BadAnswer { usage, .. } = e {
                    self.batch.add_cost(model.cost_usd(*usage));
                }
                // A rejected key or an empty balance, which would fail every video left the
                // same way, is said once at the end (`Stop::Job`).
                self.batch
                    .finish_video(id, &name, duration_s, &Outcome::Failed(e.to_string()))
            }
            ClipOutcome::OverBudget => self.batch.finish_video(
                id,
                &name,
                0.0,
                &Outcome::Skipped("not sent: over --max-cost".to_string()),
            ),
            ClipOutcome::NotStarted => return,
        };
        if matches!(outcome, ClipOutcome::Failed(_) | ClipOutcome::OverBudget) {
            self.totals.problems.push(line);
        }
    }

    /// Print the text of every finished clip no earlier clip is still waiting for.
    fn print_ready(&mut self) {
        while let Some((video, outcome)) = self.finished.remove(&self.next) {
            if let ClipOutcome::Described(record) | ClipOutcome::Cached(record) = &outcome {
                if self.print_text {
                    let cached = matches!(outcome, ClipOutcome::Cached(_));
                    self.batch.print(|| print_clip(&video, record, cached));
                }
            }
            self.next += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipscribe::{
        CacheKey, DescribedClip, Description, FileIdentity, FrameFingerprint, Segment, TagRange,
        TagSuggestion, TagSuggestions,
    };

    fn sample_shots() -> clipscribe::shots::Shots {
        use clipscribe::shots::{Shot, Shots, Transition, TransitionKind};
        Shots {
            shots: vec![
                Shot {
                    start_s: 0.0,
                    end_s: 4.0,
                    enters_by: None,
                },
                Shot {
                    start_s: 4.0,
                    end_s: 9.83,
                    enters_by: Some(TransitionKind::Cut),
                },
                Shot {
                    start_s: 10.83,
                    end_s: 12.0,
                    enters_by: Some(TransitionKind::Dissolve),
                },
            ],
            transitions: vec![
                Transition {
                    kind: TransitionKind::Cut,
                    start_s: 4.0,
                    end_s: 4.0,
                },
                Transition {
                    kind: TransitionKind::Dissolve,
                    start_s: 9.83,
                    end_s: 10.83,
                },
            ],
        }
    }

    #[test]
    fn shots_to_json_lists_shots_and_transitions() {
        let json = shots_to_json(Path::new("edit.mp4"), &sample_shots());
        assert_eq!(json["video"], "edit.mp4");
        assert_eq!(json["shots"].as_array().unwrap().len(), 3);
        assert!(json["shots"][0]["enters_by"].is_null());
        assert_eq!(json["shots"][1]["enters_by"], "cut");
        assert_eq!(json["shots"][2]["start_s"], 10.83);
        assert_eq!(json["transitions"][1]["kind"], "dissolve");
        assert_eq!(json["transitions"][1]["end_s"], 10.83);
    }

    #[test]
    fn shots_conflicts_name_real_arguments() {
        // A mistyped id would make clap panic in debug builds when the command is built.
        use clap::CommandFactory;
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["clipscribe", "a.mp4", "--shots", "--json"]).is_ok());
        assert!(
            Cli::try_parse_from(["clipscribe", "a.mp4", "--shots", "--model", "haiku"]).is_err()
        );
        assert!(Cli::try_parse_from(["clipscribe", "a.mp4", "--shots", "--estimate"]).is_err());
    }

    fn sample() -> ClipRecord {
        ClipRecord {
            file: PathBuf::from("hike.mp4"),
            key: CacheKey {
                identity: FileIdentity {
                    size: 1,
                    modified_ns: 2,
                    sample_hash: 3,
                },
                settings: "model=claude-haiku-4-5".to_string(),
            },
            model: MODELS[0].id.to_string(),
            clip: DescribedClip {
                description: Description {
                    main: None,
                    summary: "Two hikers reach a viewpoint over a valley with goats grazing below."
                        .to_string(),
                    segments: vec![Segment {
                        start_s: 12.0,
                        end_s: 20.0,
                        description: "A herd of goats crosses the path in front of the hikers."
                            .to_string(),
                    }],
                },
                tags: Some(TagSuggestions {
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
                }),
                usage: AiUsage {
                    input_tokens: 3965,
                    output_tokens: 210,
                },
                duration_s: 30.0,
                frames: (0..16)
                    .map(|i| FrameFingerprint {
                        time_s: f64::from(i) * 2.0,
                        fingerprint: vec![0; 64],
                    })
                    .collect(),
            },
        }
    }

    /// The same clip without a vocabulary: no tags.
    fn sample_without_tags() -> ClipRecord {
        let mut record = sample();
        record.clip.tags = None;
        record
    }

    /// Printed with `cargo test print_clip -- --nocapture`, for the PR's sample output: no live
    /// API key is needed since this exercises the CLI's own formatting, not a real answer.
    #[test]
    fn print_clip_shows_the_summary_segments_tags_and_ideas() {
        print_clip(Path::new("hike.mp4"), &sample(), false);
        print_clip(Path::new("hike.mp4"), &sample_without_tags(), true);
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
    fn clip_to_json_carries_the_tags_and_new_tag_ideas_fields() {
        let json = clip_to_json(Path::new("hike.mp4"), &sample(), false, None);
        assert_eq!(json["frames"], 16);
        assert_eq!(json["tags"][0]["name"], "Goat");
        assert_eq!(json["tags"][0]["ranges"][0]["start_s"], 12.0);
        assert_eq!(json["tags"][1]["name"], "Outdoor");
        assert!(json["tags"][1]["ranges"].as_array().unwrap().is_empty());
        assert_eq!(json["new_tag_ideas"], json!(["Hiking trail"]));
        assert!(json.get("summary").is_some());
        assert!(json.get("moments").is_some());
        assert!(
            json.get("cached").is_none(),
            "only on a clip from the cache"
        );
        assert!(json.get("group").is_none(), "only with --groups");
    }

    /// Without --tags, --groups or the cache, a clip's JSON has exactly the fields it had before
    /// folder runs, so existing scripts see no difference.
    #[test]
    fn clip_to_json_without_tags_groups_or_cache_keeps_the_old_fields() {
        let json = clip_to_json(Path::new("hike.mp4"), &sample_without_tags(), false, None);
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "cost_usd",
                "duration_s",
                "file",
                "frames",
                "main",
                "model",
                "moments",
                "summary",
                "usage"
            ]
        );
        let cached = clip_to_json(Path::new("hike.mp4"), &sample_without_tags(), true, None);
        assert_eq!(cached["cached"], true);
    }

    /// Printed with `cargo test clip_to_json_with_groups -- --nocapture`, for the PR's
    /// `--groups --json` sample output.
    #[test]
    fn clip_to_json_with_groups_adds_the_group_its_label_stretches_and_moment_groups() {
        let record = sample_without_tags();
        let grouping = group_clips(&[&record.clip], &GroupOptions::default());
        let json = clip_to_json(
            Path::new("hike.mp4"),
            &record,
            false,
            Some((&grouping, &grouping.clips[0])),
        );
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
        assert_eq!(json["group"], 1);
        assert_eq!(json["group_label"], grouping.groups[0].label);
        assert_eq!(json["stretches"][0]["group"], 1);
        assert_eq!(json["stretches"][0]["start_s"], 0.0);
        assert_eq!(json["stretches"][0]["end_s"], 30.0);
        assert_eq!(json["moments"][0]["group"], 1);
    }

    /// Printed with `cargo test print_groups -- --nocapture`, for the PR's `--groups` sample
    /// output: two takes of one shot (the second brighter), a third of the same trail filmed from
    /// elsewhere (other pictures, a description in much the same words), and a black clip.
    #[test]
    fn print_groups_lists_each_group_with_its_footage() {
        let mut hike = sample_without_tags();
        hike.clip.description.summary = "A black screen.".to_string();
        let mut retake = sample_without_tags();
        retake.clip.description.summary = "A trail winds up a hillside in the evening.".to_string();
        retake.clip.description.segments.clear();
        for frame in &mut retake.clip.frames {
            frame.fingerprint = (0..64).map(|i| 40 + (i % 8) * 20).collect();
        }
        let mut again = retake.clone();
        for frame in &mut again.clip.frames {
            frame.fingerprint.iter_mut().for_each(|v| *v += 30);
        }
        again.clip.description.summary = "The same trail again, in brighter light.".to_string();
        let mut elsewhere = retake.clone();
        elsewhere.clip.description.summary =
            "Walking up the trail on the hillside in the evening light.".to_string();
        for frame in &mut elsewhere.clip.frames {
            frame.fingerprint = (0..64)
                .map(|i| if (i / 8 + i % 8) % 2 == 0 { 200 } else { 20 })
                .collect();
        }
        let grouping = group_clips(
            &[&retake.clip, &hike.clip, &again.clip, &elsewhere.clip],
            &GroupOptions { words: true },
        );
        let videos = [
            Path::new("trail-1.mp4"),
            Path::new("black.mp4"),
            Path::new("trail-2.mp4"),
            Path::new("trail-3.mp4"),
        ];
        print_groups(&videos, &grouping);
        let ids: Vec<usize> = grouping.clips.iter().map(|c| c.group).collect();
        assert_eq!(ids, [1, 2, 1, 1]);
    }

    #[test]
    fn a_cache_per_folder_unless_a_directory_is_given_or_nothing_is_cached() {
        let videos: Vec<PathBuf> = ["a/1.mp4", "a/2.mp4", "b/3.mp4", "a/4.mp4", "5.mp4"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(batches_by_cache(&videos, false, None), vec![(None, 0..5)]);
        assert_eq!(
            batches_by_cache(&videos, true, Some(Path::new("caches"))),
            vec![(Some(cache_path(Path::new("caches"), None)), 0..5)]
        );
        let per_folder = batches_by_cache(&videos, true, None);
        let expected = [("a", 0..2), ("b", 2..3), ("a", 3..4), (".", 4..5)];
        assert_eq!(per_folder.len(), expected.len(), "{per_folder:?}");
        for ((path, range), (folder, want)) in per_folder.iter().zip(expected) {
            assert_eq!(
                path.as_deref(),
                Some(cache_path(Path::new(folder), None).as_path())
            );
            assert_eq!(*range, want);
        }
    }

    /// Printed with `cargo test --bin clipscribe budget_stop -- --nocapture`, for the PR's sample
    /// output.
    #[test]
    fn a_budget_stop_says_what_to_do_and_whether_anything_was_saved() {
        // `--max-cost 0.03` on tests/clips (estimated at $0.025 in all): two described, then
        // the third's bound does not fit next to what they cost.
        let near_estimate = budget_stop_message(0.03, 0.0143, Some(0.0231), 2, 4, 2, true);
        eprintln!("{near_estimate}");
        assert_eq!(
            near_estimate,
            "Stopped at --max-cost $0.03: 2 of 4 videos not described. $0.0143 spent; the next \
             video could cost up to $0.0231 (its answer counted at full length, though it usually \
             costs a fraction of that), which would pass the cap. Each video needs several times \
             its usual cost set aside before it is sent, so a cap close to --estimate's total can \
             stop several videos early. Raise --max-cost and run again with --resume to continue."
        );
        let unsaved = budget_stop_message(5.0, 4.98, Some(0.09), 120, 400, 280, false);
        eprintln!("{unsaved}");
        assert!(
            unsaved.starts_with("Stopped at --max-cost $5.00"),
            "{unsaved}"
        );
        assert!(
            unsaved.contains("The 280 described in this run were not saved"),
            "{unsaved}"
        );
        assert!(unsaved.contains("add --resume"), "{unsaved}");
        let nothing = budget_stop_message(0.01, 0.0, Some(0.0231), 4, 4, 0, false);
        eprintln!("{nothing}");
        assert!(nothing.contains("$0.00 spent"), "{nothing}");
        assert!(nothing.contains("more than the whole cap"), "{nothing}");
        assert!(
            !nothing.contains("--estimate"),
            "not about being close: {nothing}"
        );
        assert!(nothing.contains("Raise --max-cost"), "{nothing}");
    }

    #[test]
    fn dollars_show_as_many_decimals_as_they_need() {
        assert_eq!(dollars(5.0), "$5.00");
        assert_eq!(dollars(0.03), "$0.03");
        assert_eq!(dollars(0.025), "$0.025");
        assert_eq!(dollars(0.01432), "$0.0143");
        assert_eq!(dollars(0.0), "$0.00");
    }

    #[test]
    fn parse_cost_takes_dollars_above_zero() {
        assert_eq!(parse_cost("5"), Ok(5.0));
        assert_eq!(parse_cost("$0.50"), Ok(0.5));
        assert!(parse_cost("0").is_err());
        assert!(parse_cost("-1").is_err());
        assert!(parse_cost("lots").is_err());
        assert!(parse_cost("inf").is_err());
    }

    #[test]
    fn the_command_line_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let parse = |args: &[&str]| Cli::try_parse_from([&["clipscribe"], args].concat());
        let cli = parse(&["footage/", "--resume", "--groups", "--json"]).expect("the issue's");
        assert!(cli.resume && cli.groups == Some(GroupsArg::Pictures) && cli.json);
        assert_eq!(usize::from(cli.jobs), DEFAULT_JOBS);
        assert!(
            parse(&["a.mp4", "--resume", "--force"]).is_err(),
            "one or the other"
        );
        assert!(
            parse(&["a.mp4", "--cache-dir", "c"]).is_err(),
            "a cache needs --resume/--force"
        );
        assert!(parse(&["a.mp4", "--force", "--cache-dir", "c"]).is_ok());
        assert!(parse(&["a.mp4", "--jobs", "0"]).is_err());
        assert!(parse(&["a.mp4", "--at", "5", "--groups"]).is_err());
        // `--groups` takes a value only with `=`, so it never swallows the path after it.
        let cli = parse(&["--groups", "footage/"]).expect("a bare --groups");
        assert_eq!(cli.groups, Some(GroupsArg::Pictures));
        assert_eq!(cli.inputs, [PathBuf::from("footage/")]);
        let cli = parse(&["footage/", "--groups=words"]).expect("words");
        assert_eq!(cli.groups, Some(GroupsArg::Words));
        assert_eq!(parse(&["footage/"]).expect("none").groups, None);
        assert!(parse(&["footage/", "--groups=other"]).is_err());
        assert!(parse(&["a.mp4", "--estimate", "--max-cost", "1"]).is_err());
    }

    #[test]
    fn the_main_range_is_printed_and_in_the_json_and_null_when_absent() {
        let mut described = sample();
        assert_eq!(
            main_json(&described.clip.description),
            serde_json::Value::Null
        );
        described.clip.description.main = Some(clipscribe::MainRange {
            start_s: 7.0,
            end_s: 28.5,
        });
        let json = clip_to_json(Path::new("hike.mp4"), &described, false, None);
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
