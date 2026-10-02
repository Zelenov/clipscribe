//! What the command line shows while it describes a batch of videos, on stderr: a plan line, one
//! bar for the whole run, a line for the video in work, one permanent result line per video and
//! a summary. Without a terminal (piped, CI) or with `--quiet` there are no bars, only lines.
//!
//! The line formats are plain functions of their inputs, so they are tested without a terminal;
//! [`Batch`] only draws them. Nothing here writes to stdout except [`Batch::print`], which
//! steps the bars aside first.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clipscribe::{format_time, Model, Provider, RetryReason, Stage};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

/// The width lines are cut to when stderr is not a terminal.
const PLAIN_WIDTH: usize = 100;

/// What describing the inputs is expected to take, for the line printed before starting.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub videos: usize,
    /// Seconds of footage in the clips that can be read.
    pub footage_s: f64,
    pub cost_usd: f64,
    pub model: &'static str,
    /// Clips over the length limit: they are not sent.
    pub over_limit: usize,
    pub limit_s: f64,
    /// Clips that could not be read at all.
    pub unreadable: usize,
}

/// `24 videos: 41:12 of footage, ≈ $0.38 with Claude Haiku 4.5`, plus what will not be sent.
pub fn plan_line(plan: &Plan) -> String {
    let videos = if plan.videos == 1 { "video" } else { "videos" };
    let mut line = format!(
        "{} {videos}: {} of footage, \u{2248} ${:.2} with {}",
        plan.videos,
        format_time(plan.footage_s),
        plan.cost_usd,
        plan.model
    );
    if plan.over_limit > 0 {
        line.push_str(&format!(
            ", {} over the {} limit (skipped)",
            plan.over_limit,
            format_time(plan.limit_s)
        ));
    }
    if plan.unreadable > 0 {
        line.push_str(&format!(", {} unreadable", plan.unreadable));
    }
    line
}

/// How one video ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Described {
        moments: usize,
        main: Option<(f64, f64)>,
        cost_usd: f64,
    },
    /// Not sent, and why.
    Skipped(String),
    Failed(String),
}

impl Outcome {
    /// A described video: its moments, the main range if there is one, and what it cost.
    pub fn of(description: &clipscribe::Description, cost_usd: f64) -> Self {
        Self::Described {
            moments: description.segments.len(),
            main: description.main.map(|m| (m.start_s, m.end_s)),
            cost_usd,
        }
    }
}

/// The permanent line of one video, cut to `width` columns when there is one (the name gives way
/// first); a line without a width is never cut, so an error is printed whole:
/// `✓ clip.mp4  3 moments · main 0:04–0:51 · $0.012`, `· clip.mp4  skipped (…)` or
/// `✗ clip.mp4  error text`.
pub fn result_line(name: &str, outcome: &Outcome, width: Option<usize>) -> String {
    let (mark, detail) = match outcome {
        Outcome::Described {
            moments,
            main,
            cost_usd,
        } => {
            let mut parts = vec![match moments {
                0 => "no moments".to_string(),
                1 => "1 moment".to_string(),
                n => format!("{n} moments"),
            }];
            if let Some((start, end)) = main {
                parts.push(format!(
                    "main {}\u{2013}{}",
                    format_time(*start),
                    format_time(*end)
                ));
            }
            parts.push(format!("${cost_usd:.3}"));
            ("\u{2713}", parts.join(" \u{b7} "))
        }
        Outcome::Skipped(why) => ("\u{b7}", format!("skipped ({why})")),
        Outcome::Failed(message) => ("\u{2717}", message.clone()),
    };
    // A message is one line, however it was written.
    let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(width) = width else {
        return format!("{mark} {name}  {detail}");
    };
    let fixed = console::measure_text_width(&format!("{mark}   {detail}"));
    let name_width = width.saturating_sub(fixed).max(8);
    let name = console::truncate_str(name, name_width, "\u{2026}");
    let line = format!("{mark} {name}  {detail}");
    console::truncate_str(&line, width.max(20), "\u{2026}").into_owned()
}

/// What the run came to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Totals {
    pub total_videos: usize,
    /// Videos that were described.
    pub described: usize,
    pub moments: usize,
    pub cost_usd: f64,
    pub elapsed: Duration,
    /// Videos not sent because they are over the length limit.
    pub skipped: usize,
    /// Ctrl+C ended the run before the last video.
    pub cancelled: bool,
    /// A rejected key or an empty balance ended the run before the last video.
    pub stopped: bool,
    /// Videos that were never tried because the run ended early.
    pub not_tried: usize,
    /// The result lines of the videos that did not come out, for the end.
    pub problems: Vec<String>,
}

/// The lines closing a run: the totals, then the problems again.
pub fn summary_lines(totals: &Totals) -> Vec<String> {
    let mut head = if totals.cancelled || totals.stopped {
        format!(
            "{}: {} of {} videos finished",
            if totals.cancelled {
                "cancelled"
            } else {
                "stopped"
            },
            totals.described,
            totals.total_videos
        )
    } else {
        let videos = if totals.described == 1 {
            "video"
        } else {
            "videos"
        };
        format!("done: {} {videos}", totals.described)
    };
    head.push_str(&format!(
        " \u{b7} {} moments \u{b7} ${:.2} \u{b7} {}",
        totals.moments,
        totals.cost_usd,
        format_time(totals.elapsed.as_secs_f64())
    ));
    if totals.skipped > 0 {
        head.push_str(&format!(" \u{b7} {} skipped", totals.skipped));
    }
    if totals.not_tried > 0 {
        head.push_str(&format!(" \u{b7} {} not tried", totals.not_tried));
    }
    let mut lines = vec![head];
    if !totals.problems.is_empty() {
        let n = totals.problems.len();
        lines.push(format!(
            "{n} {}:",
            if n == 1 { "problem" } else { "problems" }
        ));
        lines.extend(totals.problems.iter().map(|p| format!("  {p}")));
    }
    lines
}

/// What the video in work is doing, as the second line of the display.
pub fn stage_text(stage: Stage, model: &str) -> String {
    match stage {
        Stage::Frame { done, total } => format!("reading frames {}/{total}", done + 1),
        Stage::Asking { provider } => format!("asking {} ({model})", provider.label()),
        Stage::Retrying { after, reason } => {
            let reason = match reason {
                RetryReason::RateLimit => "rate limit",
                RetryReason::Temporary => "temporary failure",
            };
            format!(
                "retrying in {} s ({reason})",
                (after.as_secs() + u64::from(after.subsec_nanos() > 0)).max(1)
            )
        }
        // `Stage` may grow: a stage this version does not know is shown as working.
        _ => "working".to_string(),
    }
}

/// The terminal indicatif draws on, two columns narrower than it is. indicatif pads every line to
/// the full width, so the `^C` the terminal echoes for Ctrl+C would wrap onto the next row and
/// leave indicatif erasing one row too few: a bar would stay behind. With room to spare the echo
/// stays on its row.
#[derive(Debug)]
struct Margin(console::Term);

impl indicatif::TermLike for Margin {
    fn width(&self) -> u16 {
        self.0.size().1.saturating_sub(2).max(20)
    }
    fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_up(n)
    }
    fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_down(n)
    }
    fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_right(n)
    }
    fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
        self.0.move_cursor_left(n)
    }
    fn write_line(&self, s: &str) -> std::io::Result<()> {
        self.0.write_line(s)
    }
    fn write_str(&self, s: &str) -> std::io::Result<()> {
        self.0.write_str(s)
    }
    fn clear_line(&self) -> std::io::Result<()> {
        self.0.clear_line()
    }
    fn flush(&self) -> std::io::Result<()> {
        std::io::Write::flush(&mut &self.0)
    }
}

/// What the line of the video in work says. Kept apart from the bar so the text can change as
/// time passes: a retry countdown, and back to "asking" when the wait is over.
#[derive(Debug, Clone, Copy)]
struct Shown {
    stage: Option<Stage>,
    since: Instant,
    /// Who the request went to, for the line after a retry wait.
    asking: Option<Provider>,
}

fn shown_text(shown: &Shown, now: Instant, model: &str) -> String {
    match shown.stage {
        None => "starting".to_string(),
        Some(Stage::Retrying { after, reason }) => {
            let left = after.saturating_sub(now.saturating_duration_since(shown.since));
            if !left.is_zero() {
                stage_text(
                    Stage::Retrying {
                        after: left,
                        reason,
                    },
                    model,
                )
            } else if let Some(provider) = shown.asking {
                stage_text(Stage::Asking { provider }, model)
            } else {
                "waiting for the answer".to_string()
            }
        }
        Some(stage) => stage_text(stage, model),
    }
}

/// The bars of the video in work.
struct Current {
    shown: Arc<Mutex<Shown>>,
    line: ProgressBar,
    frames: ProgressBar,
}

/// The display of one run. Draws on stderr only, and only on a terminal.
pub struct Batch {
    multi: MultiProgress,
    overall: ProgressBar,
    current: Option<Current>,
    interactive: bool,
    quiet: bool,
    width: usize,
    model: Model,
    total_videos: usize,
    total_footage_ms: u64,
    done_footage_ms: u64,
    finished: usize,
    cost_usd: f64,
    started: Instant,
    /// Videos finished, shared with the bar's ETA, which waits for the first one.
    finished_count: Arc<AtomicUsize>,
}

fn millis(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

impl Batch {
    /// A display for `total_videos` videos with `footage_s` seconds in all. `quiet` turns the
    /// bars, the plan and the summary off; a stderr that is not a terminal does the same for the
    /// bars.
    pub fn new(model: Model, total_videos: usize, footage_s: f64, quiet: bool) -> Self {
        let terminal = console::Term::stderr().is_term();
        let width = if terminal {
            usize::from(console::Term::stderr().size().1).saturating_sub(2)
        } else {
            PLAIN_WIDTH
        };
        let target = if terminal && !quiet {
            ProgressDrawTarget::term_like(Box::new(Margin(console::Term::stderr())))
        } else {
            ProgressDrawTarget::hidden()
        };
        Self::with_target(
            target,
            terminal && !quiet,
            quiet,
            width,
            model,
            total_videos,
            footage_s,
        )
    }

    fn with_target(
        target: ProgressDrawTarget,
        interactive: bool,
        quiet: bool,
        width: usize,
        model: Model,
        total_videos: usize,
        footage_s: f64,
    ) -> Self {
        let multi = MultiProgress::with_draw_target(target);
        let total_footage_ms = millis(footage_s).max(1);
        let overall = multi.add(ProgressBar::new(total_footage_ms));
        let finished_count = Arc::new(AtomicUsize::new(0));
        let bar_width = if width < 60 { 10 } else { 30 };
        let eta_seen = Arc::clone(&finished_count);
        overall.set_style(
            ProgressStyle::with_template(&format!(
                "{{bar:{bar_width}.cyan/blue}} {{percent:>3}}% {{msg}}{{eta_part}}"
            ))
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .with_key(
                "eta_part",
                move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    // No ETA until a video is done: before that there is no rate to go by.
                    if eta_seen.load(Ordering::Relaxed) > 0 {
                        let _ = write!(w, "  ETA {}", format_time(state.eta().as_secs_f64()));
                    }
                },
            )
            .progress_chars("=> "),
        );
        let mut batch = Self {
            multi,
            overall,
            current: None,
            interactive,
            quiet,
            width: width.max(40),
            model,
            total_videos,
            total_footage_ms,
            done_footage_ms: 0,
            finished: 0,
            cost_usd: 0.0,
            started: Instant::now(),
            finished_count,
        };
        batch.refresh_overall();
        batch
    }

    fn refresh_overall(&mut self) {
        self.overall.set_position(self.done_footage_ms);
        self.overall.set_message(format!(
            "{}/{} videos \u{b7} {} of {} \u{b7} ${:.3}",
            self.finished,
            self.total_videos,
            format_time(self.done_footage_ms as f64 / 1000.0),
            format_time(self.total_footage_ms as f64 / 1000.0),
            self.cost_usd
        ));
    }

    /// Print a line on stderr above the bars (or plainly when there are none).
    fn println(&self, line: &str) {
        // `suspend` erases the bars, prints a whole line and draws them again; indicatif's own
        // `println` relies on lines filling the terminal width, which the margin below does not.
        if self.interactive {
            self.multi.suspend(|| eprintln!("{line}"));
        } else {
            eprintln!("{line}");
        }
    }

    /// The plan, before the first video. Not with `--quiet`.
    pub fn plan(&self, plan: &Plan) {
        if !self.quiet {
            self.println(&plan_line(plan));
        }
    }

    /// The video `name` starts.
    pub fn begin(&mut self, name: &str) {
        if !self.interactive {
            return;
        }
        let shown = Arc::new(Mutex::new(Shown {
            stage: None,
            since: Instant::now(),
            asking: None,
        }));
        let line = self.multi.add(ProgressBar::new_spinner());
        let (text, model) = (Arc::clone(&shown), self.model.label);
        line.set_style(
            ProgressStyle::with_template("  {spinner} {prefix}: {stage} ({elapsed})")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .with_key(
                    "stage",
                    move |_: &ProgressState, w: &mut dyn std::fmt::Write| {
                        if let Ok(shown) = text.lock() {
                            let _ = w.write_str(&shown_text(&shown, Instant::now(), model));
                        }
                    },
                ),
        );
        let prefix_width = (self.width / 3).max(12);
        line.set_prefix(console::truncate_str(name, prefix_width, "\u{2026}").into_owned());
        line.enable_steady_tick(Duration::from_millis(120));
        let frames = self.multi.add(ProgressBar::new(1));
        let bar_width = if self.width < 60 { 10 } else { 20 };
        frames.set_style(
            ProgressStyle::with_template(&format!("    {{bar:{bar_width}}} {{pos}}/{{len}}"))
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        self.current = Some(Current {
            shown,
            line,
            frames,
        });
    }

    /// The video in work reached `stage`.
    pub fn stage(&mut self, stage: Stage) {
        let Some(current) = &self.current else {
            return;
        };
        if let Ok(mut shown) = current.shown.lock() {
            shown.stage = Some(stage);
            shown.since = Instant::now();
            if let Stage::Asking { provider } = stage {
                shown.asking = Some(provider);
            }
        }
        // The text comes from `shown` when the line is drawn: draw now, not at the next tick.
        current.line.tick();
        match stage {
            Stage::Frame { done, total } => {
                current.frames.set_length(total.max(1) as u64);
                current.frames.set_position(done as u64 + 1);
            }
            Stage::Asking { .. } => {
                // The time shown is the time since the request went out.
                current.line.reset_elapsed();
                current.frames.finish_and_clear();
            }
            _ => current.frames.finish_and_clear(),
        }
    }

    /// Cost the bar did not see: a billed answer that could not be used.
    pub fn add_cost(&mut self, cost_usd: f64) {
        self.cost_usd += cost_usd;
        self.refresh_overall();
    }

    /// The video `name` (`duration_s` of footage) ended as `outcome`; its line stays on screen.
    /// Returns the line, for the summary.
    pub fn finish_video(&mut self, name: &str, duration_s: f64, outcome: &Outcome) -> String {
        if let Some(current) = self.current.take() {
            current.frames.finish_and_clear();
            current.line.finish_and_clear();
        }
        self.finished += 1;
        self.finished_count.store(self.finished, Ordering::Relaxed);
        self.done_footage_ms =
            (self.done_footage_ms + millis(duration_s)).min(self.total_footage_ms);
        if let Outcome::Described { cost_usd, .. } = outcome {
            self.cost_usd += cost_usd;
        }
        self.refresh_overall();
        let width = self.interactive.then_some(self.width);
        self.println(&result_line(name, outcome, width));
        result_line(name, outcome, None)
    }

    /// A line of its own on stderr, above the bars: a warning or why the run stopped.
    pub fn note(&self, line: &str) {
        self.println(line);
    }

    /// Run `print` (which writes to stdout) without the bars in the way.
    pub fn print(&self, print: impl FnOnce()) {
        if self.interactive {
            self.multi.suspend(print);
        } else {
            print();
        }
    }

    /// Clear the bars and print the summary (not with `--quiet`).
    pub fn finish(&mut self, mut totals: Totals) {
        if let Some(current) = self.current.take() {
            current.frames.finish_and_clear();
            current.line.finish_and_clear();
        }
        self.overall.finish_and_clear();
        let _ = self.multi.clear();
        totals.total_videos = self.total_videos;
        totals.elapsed = self.started.elapsed();
        if !self.quiet {
            for line in summary_lines(&totals) {
                eprintln!("{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipscribe::{Provider, MODELS};
    use std::sync::{Arc, Mutex};

    fn plan() -> Plan {
        Plan {
            videos: 24,
            footage_s: 2472.0,
            cost_usd: 0.384,
            model: "Claude Haiku 4.5",
            over_limit: 0,
            limit_s: 1800.0,
            unreadable: 0,
        }
    }

    fn described(moments: usize, main: Option<(f64, f64)>) -> Outcome {
        Outcome::Described {
            moments,
            main,
            cost_usd: 0.0123,
        }
    }

    #[test]
    fn the_plan_line_has_count_footage_cost_and_model() {
        assert_eq!(
            plan_line(&plan()),
            "24 videos: 41:12 of footage, \u{2248} $0.38 with Claude Haiku 4.5"
        );
        let one = Plan {
            videos: 1,
            ..plan()
        };
        assert!(plan_line(&one).starts_with("1 video: "));
    }

    #[test]
    fn the_plan_line_reports_skipped_and_unreadable_files() {
        let line = plan_line(&Plan {
            over_limit: 2,
            unreadable: 1,
            ..plan()
        });
        assert!(
            line.ends_with(", 2 over the 30:00 limit (skipped), 1 unreadable"),
            "{line}"
        );
    }

    #[test]
    fn result_lines_for_a_described_skipped_and_failed_video() {
        assert_eq!(
            result_line("clip.mp4", &described(3, Some((4.0, 51.0))), Some(100)),
            "\u{2713} clip.mp4  3 moments \u{b7} main 0:04\u{2013}0:51 \u{b7} $0.012"
        );
        assert_eq!(
            result_line("a.mp4", &described(1, None), Some(100)),
            "\u{2713} a.mp4  1 moment \u{b7} $0.012"
        );
        assert_eq!(
            result_line("a.mp4", &described(0, None), Some(100)),
            "\u{2713} a.mp4  no moments \u{b7} $0.012"
        );
        assert_eq!(
            result_line(
                "long.mp4",
                &Outcome::Skipped("over the 30:00 limit".into()),
                Some(100)
            ),
            "\u{b7} long.mp4  skipped (over the 30:00 limit)"
        );
        assert_eq!(
            result_line(
                "bad.mp4",
                &Outcome::Failed("could not be read".into()),
                Some(100)
            ),
            "\u{2717} bad.mp4  could not be read"
        );
    }

    #[test]
    fn a_multi_line_error_is_one_line() {
        let line = result_line(
            "a.mp4",
            &Outcome::Failed("first\n  second".into()),
            Some(100),
        );
        assert_eq!(line, "\u{2717} a.mp4  first second");
    }

    #[test]
    fn cyrillic_names_are_kept_and_long_names_are_cut_to_the_width() {
        let name = "\u{41e}\u{442}\u{43f}\u{443}\u{441}\u{43a}_\u{43d}\u{430}_\u{43c}\u{43e}\u{440}\u{435}_\u{43f}\u{435}\u{440}\u{432}\u{44b}\u{439}_\u{434}\u{435}\u{43d}\u{44c}.mp4";
        let outcome = described(3, Some((4.0, 51.0)));
        let full = result_line(name, &outcome, Some(120));
        assert!(full.contains(name), "{full}");
        for width in [50, 60, 80] {
            let line = result_line(name, &outcome, Some(width));
            assert!(
                console::measure_text_width(&line) <= width,
                "{width}: {line}"
            );
            if width < console::measure_text_width(&full) {
                assert!(line.contains('\u{2026}'), "{line}");
            }
            assert!(line.ends_with("$0.012"), "the details stay: {line}");
        }
        // Too narrow for the details: the line is cut, never wider than the terminal.
        for width in [20, 30, 40] {
            let line = result_line(name, &outcome, Some(width));
            assert!(
                console::measure_text_width(&line) <= width,
                "{width}: {line}"
            );
        }
        let long = "x".repeat(200);
        let line = result_line(&long, &Outcome::Failed("boom".into()), Some(50));
        assert!(console::measure_text_width(&line) <= 50, "{line}");
        assert!(line.ends_with("boom"), "{line}");
    }

    #[test]
    fn no_line_has_an_escape_code() {
        let lines = [
            plan_line(&plan()),
            result_line("a.mp4", &described(2, Some((1.0, 2.0))), Some(80)),
            result_line("a.mp4", &Outcome::Failed("x".into()), Some(80)),
            summary_lines(&Totals {
                problems: vec!["\u{2717} a.mp4  x".into()],
                ..Totals::default()
            })
            .join("\n"),
            stage_text(
                Stage::Retrying {
                    after: Duration::from_secs(8),
                    reason: RetryReason::RateLimit,
                },
                "m",
            ),
        ];
        for line in lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
        }
    }

    #[test]
    fn the_summary_has_totals_and_lists_the_problems_again() {
        let totals = Totals {
            total_videos: 3,
            described: 2,
            moments: 7,
            cost_usd: 0.0456,
            elapsed: Duration::from_secs(252),
            problems: vec!["\u{2717} bad.mp4  could not be read".into()],
            ..Totals::default()
        };
        assert_eq!(
            summary_lines(&totals),
            [
                "done: 2 videos \u{b7} 7 moments \u{b7} $0.05 \u{b7} 4:12",
                "1 problem:",
                "  \u{2717} bad.mp4  could not be read",
            ]
        );
        let cancelled = summary_lines(&Totals {
            cancelled: true,
            ..totals
        });
        assert!(cancelled[0].starts_with("cancelled: 2 of 3 videos finished"));
    }

    #[test]
    fn the_asking_stage_names_the_provider_that_is_asked() {
        let asking = |provider| stage_text(Stage::Asking { provider }, "Some Model");
        assert_eq!(asking(Provider::Anthropic), "asking Anthropic (Some Model)");
        assert_eq!(asking(Provider::OpenAi), "asking OpenAI (Some Model)");
    }

    #[test]
    fn stages_read_as_frames_asking_and_retrying() {
        assert_eq!(
            stage_text(
                Stage::Frame {
                    done: 11,
                    total: 60
                },
                "m"
            ),
            "reading frames 12/60"
        );
        let retry = |reason| {
            stage_text(
                Stage::Retrying {
                    after: Duration::from_secs(8),
                    reason,
                },
                "m",
            )
        };
        assert_eq!(
            retry(RetryReason::RateLimit),
            "retrying in 8 s (rate limit)"
        );
        assert_eq!(
            retry(RetryReason::Temporary),
            "retrying in 8 s (temporary failure)"
        );
    }

    /// A terminal that records what is drawn on it.
    #[derive(Debug, Clone, Default)]
    struct Recorder(Arc<Mutex<String>>);

    impl indicatif::TermLike for Recorder {
        fn width(&self) -> u16 {
            80
        }
        fn move_cursor_up(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn write_line(&self, s: &str) -> std::io::Result<()> {
            self.0.lock().expect("lock").push_str(&format!("{s}\n"));
            Ok(())
        }
        fn write_str(&self, s: &str) -> std::io::Result<()> {
            self.0.lock().expect("lock").push_str(s);
            Ok(())
        }
        fn clear_line(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_run_draws_the_batch_bar_the_stage_and_the_result_on_the_terminal() {
        let screen = Recorder::default();
        // A high refresh rate: indicatif drops draws that come too fast, and this run is instant.
        let target = ProgressDrawTarget::term_like_with_hz(Box::new(screen.clone()), 255);
        let mut batch = Batch::with_target(target, true, false, 80, MODELS[0], 2, 60.0);
        let pause = || std::thread::sleep(Duration::from_millis(20));
        pause();
        batch.begin("clip.mp4");
        pause();
        batch.stage(Stage::Frame { done: 4, total: 60 });
        pause();
        batch.stage(Stage::Asking {
            provider: Provider::OpenAi,
        });
        pause();
        // What the line of the video says is its state, whichever frames indicatif drew.
        let shown = *batch
            .current
            .as_ref()
            .expect("a video is in work")
            .shown
            .lock()
            .expect("lock");
        assert_eq!(shown_text(&shown, Instant::now(), "M"), "asking OpenAI (M)");
        pause();
        let line = batch.finish_video("clip.mp4", 30.0, &described(2, None));
        // The result line itself goes to stderr between two draws of the bars; the display
        // returns it for the summary.
        assert!(line.starts_with("\u{2713} clip.mp4  2 moments"), "{line}");
        let drawn = screen.0.lock().expect("lock").clone();
        assert!(drawn.contains("0/2 videos"), "{drawn}");
        assert!(drawn.contains("1/2 videos"), "{drawn}");
    }

    #[test]
    fn without_a_terminal_nothing_is_drawn_and_the_bars_are_hidden() {
        // `interactive` false is what `Batch::new` picks for a stderr that is not a terminal:
        // its draw target is hidden, so no escape code can reach a pipe or a log.
        let mut batch = Batch::with_target(
            ProgressDrawTarget::hidden(),
            false,
            false,
            100,
            MODELS[0],
            1,
            10.0,
        );
        batch.begin("clip.mp4");
        batch.stage(Stage::Asking {
            provider: Provider::Anthropic,
        });
        batch.finish_video("clip.mp4", 10.0, &described(1, None));
        assert!(batch.current.is_none());
        assert!(batch.overall.is_hidden());
        // On a developer's terminal `cargo test` still has a terminal behind its captured
        // stderr, so only check what holds there too.
        let quiet = Batch::new(MODELS[0], 1, 10.0, true);
        assert!(!quiet.interactive && quiet.overall.is_hidden());
        if !console::Term::stderr().is_term() {
            let plain = Batch::new(MODELS[0], 1, 10.0, false);
            assert!(!plain.interactive && plain.overall.is_hidden());
        }
    }

    #[test]
    fn an_error_is_printed_whole_without_a_width_and_cut_with_one() {
        let long = "x ".repeat(100);
        let outcome = Outcome::Failed(long.trim().to_string());
        let whole = result_line("a.mp4", &outcome, None);
        assert!(whole.ends_with(long.trim()), "{whole}");
        let cut = result_line("a.mp4", &outcome, Some(60));
        assert!(console::measure_text_width(&cut) <= 60, "{cut}");
    }

    #[test]
    fn a_stopped_run_says_how_many_videos_were_never_tried() {
        let lines = summary_lines(&Totals {
            total_videos: 3,
            described: 1,
            moments: 2,
            skipped: 1,
            stopped: true,
            not_tried: 1,
            elapsed: Duration::from_secs(5),
            ..Totals::default()
        });
        assert_eq!(
            lines[0],
            "stopped: 1 of 3 videos finished \u{b7} 2 moments \u{b7} $0.00 \u{b7} 0:05 \u{b7} 1 skipped \u{b7} 1 not tried"
        );
    }

    #[test]
    fn the_retry_line_counts_down_and_then_asks_again() {
        let start = Instant::now();
        let shown = Shown {
            stage: Some(Stage::Retrying {
                after: Duration::from_secs(8),
                reason: RetryReason::RateLimit,
            }),
            since: start,
            asking: Some(Provider::Anthropic),
        };
        let at = |secs| shown_text(&shown, start + Duration::from_secs(secs), "Some Model");
        assert_eq!(at(0), "retrying in 8 s (rate limit)");
        assert_eq!(at(5), "retrying in 3 s (rate limit)");
        assert_eq!(at(8), "asking Anthropic (Some Model)");
        assert_eq!(at(30), "asking Anthropic (Some Model)");
        let fresh = Shown {
            stage: None,
            since: start,
            asking: None,
        };
        assert_eq!(shown_text(&fresh, start, "m"), "starting");
    }

    #[test]
    fn the_batch_bar_is_weighted_by_footage_not_by_file_count() {
        let mut batch = Batch::with_target(
            ProgressDrawTarget::hidden(),
            false,
            false,
            100,
            MODELS[0],
            2,
            100.0,
        );
        batch.finish_video("short.mp4", 10.0, &described(0, None));
        assert_eq!(batch.overall.position(), 10_000);
        assert_eq!(batch.overall.length(), Some(100_000));
        batch.finish_video("long.mp4", 90.0, &Outcome::Failed("x".into()));
        assert_eq!(batch.overall.position(), 100_000);
    }
}
