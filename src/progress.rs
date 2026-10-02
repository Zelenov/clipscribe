//! What the command line shows while it describes a batch of videos, on stderr: a plan line, one
//! bar for the whole run, a line for the video in work, one permanent result line per video and
//! a summary. Without a terminal (piped, CI) or with `--quiet` there are no bars, only lines.
//!
//! The line formats are plain functions of their inputs, so they are tested without a terminal;
//! [`Batch`] only draws them. Nothing here writes to stdout except [`Batch::print`], which
//! steps the bars aside first.

use std::time::{Duration, Instant};

use clipscribe::{format_time, Model, RetryReason, Stage};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

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

/// The permanent line of one video, cut to `width` columns (the name gives way first):
/// `✓ clip.mp4  3 moments · main 0:04–0:51 · $0.012`, `· clip.mp4  skipped (…)` or
/// `✗ clip.mp4  error text`.
pub fn result_line(name: &str, outcome: &Outcome, width: usize) -> String {
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
    /// Ctrl+C ended the run before the last video.
    pub cancelled: bool,
    /// The result lines of the videos that did not come out, for the end.
    pub problems: Vec<String>,
}

/// The lines closing a run: the totals, then the problems again.
pub fn summary_lines(totals: &Totals) -> Vec<String> {
    let mut head = if totals.cancelled {
        format!(
            "cancelled: {} of {} videos finished",
            totals.described, totals.total_videos
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
            format!("retrying in {} s ({reason})", after.as_secs().max(1))
        }
        // Stage may grow; an unknown one is shown as waiting.
        #[allow(unreachable_patterns)]
        _ => "working".to_string(),
    }
}

/// The bars of the video in work.
struct Current {
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
            usize::from(console::Term::stderr().size().1)
        } else {
            PLAIN_WIDTH
        };
        let target = if terminal && !quiet {
            ProgressDrawTarget::stderr()
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
        overall.set_style(
            ProgressStyle::with_template("{bar:30.cyan/blue} {percent:>3}% {msg}  ETA {eta}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
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
        if self.interactive {
            let _ = self.multi.println(line);
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
        let line = self.multi.add(ProgressBar::new_spinner());
        line.set_style(
            ProgressStyle::with_template("  {spinner} {prefix}: {msg} ({elapsed})")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        let prefix_width = (self.width / 3).max(12);
        line.set_prefix(console::truncate_str(name, prefix_width, "\u{2026}").into_owned());
        line.enable_steady_tick(Duration::from_millis(120));
        let frames = self.multi.add(ProgressBar::new(1));
        frames.set_style(
            ProgressStyle::with_template("    {bar:20} {pos}/{len}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
        );
        self.current = Some(Current { line, frames });
    }

    /// The video in work reached `stage`.
    pub fn stage(&mut self, stage: Stage) {
        let Some(current) = &self.current else {
            return;
        };
        current
            .line
            .set_message(stage_text(stage, self.model.label));
        match stage {
            Stage::Frame { done, total } => {
                current.frames.set_length(total.max(1) as u64);
                current.frames.set_position(done as u64 + 1);
            }
            _ => current.frames.finish_and_clear(),
        }
    }

    /// The video `name` (`duration_s` of footage) ended as `outcome`; its line stays on screen.
    /// Returns the line, for the summary.
    pub fn finish_video(&mut self, name: &str, duration_s: f64, outcome: &Outcome) -> String {
        if let Some(current) = self.current.take() {
            current.frames.finish_and_clear();
            current.line.finish_and_clear();
        }
        self.finished += 1;
        self.done_footage_ms =
            (self.done_footage_ms + millis(duration_s)).min(self.total_footage_ms);
        if let Outcome::Described { cost_usd, .. } = outcome {
            self.cost_usd += cost_usd;
        }
        self.refresh_overall();
        let line = result_line(name, outcome, self.width);
        self.println(&line);
        line
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
            result_line("clip.mp4", &described(3, Some((4.0, 51.0))), 100),
            "\u{2713} clip.mp4  3 moments \u{b7} main 0:04\u{2013}0:51 \u{b7} $0.012"
        );
        assert_eq!(
            result_line("a.mp4", &described(1, None), 100),
            "\u{2713} a.mp4  1 moment \u{b7} $0.012"
        );
        assert_eq!(
            result_line("a.mp4", &described(0, None), 100),
            "\u{2713} a.mp4  no moments \u{b7} $0.012"
        );
        assert_eq!(
            result_line(
                "long.mp4",
                &Outcome::Skipped("over the 30:00 limit".into()),
                100
            ),
            "\u{b7} long.mp4  skipped (over the 30:00 limit)"
        );
        assert_eq!(
            result_line("bad.mp4", &Outcome::Failed("could not be read".into()), 100),
            "\u{2717} bad.mp4  could not be read"
        );
    }

    #[test]
    fn a_multi_line_error_is_one_line() {
        let line = result_line("a.mp4", &Outcome::Failed("first\n  second".into()), 100);
        assert_eq!(line, "\u{2717} a.mp4  first second");
    }

    #[test]
    fn cyrillic_names_are_kept_and_long_names_are_cut_to_the_width() {
        let name = "\u{41e}\u{442}\u{43f}\u{443}\u{441}\u{43a}_\u{43d}\u{430}_\u{43c}\u{43e}\u{440}\u{435}_\u{43f}\u{435}\u{440}\u{432}\u{44b}\u{439}_\u{434}\u{435}\u{43d}\u{44c}.mp4";
        let outcome = described(3, Some((4.0, 51.0)));
        let full = result_line(name, &outcome, 120);
        assert!(full.contains(name), "{full}");
        for width in [50, 60, 80] {
            let line = result_line(name, &outcome, width);
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
            let line = result_line(name, &outcome, width);
            assert!(
                console::measure_text_width(&line) <= width,
                "{width}: {line}"
            );
        }
        let long = "x".repeat(200);
        let line = result_line(&long, &Outcome::Failed("boom".into()), 50);
        assert!(console::measure_text_width(&line) <= 50, "{line}");
        assert!(line.ends_with("boom"), "{line}");
    }

    #[test]
    fn no_line_has_an_escape_code() {
        let lines = [
            plan_line(&plan()),
            result_line("a.mp4", &described(2, Some((1.0, 2.0))), 80),
            result_line("a.mp4", &Outcome::Failed("x".into()), 80),
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
            cancelled: false,
            problems: vec!["\u{2717} bad.mp4  could not be read".into()],
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
        let target = ProgressDrawTarget::term_like(Box::new(screen.clone()));
        let mut batch = Batch::with_target(target, true, false, 80, MODELS[0], 2, 60.0);
        batch.begin("clip.mp4");
        batch.stage(Stage::Frame { done: 4, total: 60 });
        batch.stage(Stage::Asking {
            provider: Provider::OpenAi,
        });
        batch.finish_video("clip.mp4", 30.0, &described(2, None));
        let drawn = screen.0.lock().expect("lock").clone();
        assert!(drawn.contains("0/2 videos"), "{drawn}");
        assert!(drawn.contains("1/2 videos"), "{drawn}");
        assert!(drawn.contains("asking OpenAI"), "{drawn}");
        assert!(drawn.contains("\u{2713} clip.mp4  2 moments"), "{drawn}");
    }

    #[test]
    fn without_a_terminal_nothing_is_drawn_and_the_bars_are_hidden() {
        let screen = Recorder::default();
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
        assert!(screen.0.lock().expect("lock").is_empty());
        assert!(batch.current.is_none());
        // CI and pipes: no terminal, so nothing is drawn. (On a developer's terminal `cargo test`
        // still has a terminal behind its captured stderr, so only check what holds there too.)
        let quiet = Batch::new(MODELS[0], 1, 10.0, true);
        assert!(!quiet.interactive && quiet.overall.is_hidden());
        if !console::Term::stderr().is_term() {
            let plain = Batch::new(MODELS[0], 1, 10.0, false);
            assert!(!plain.interactive && plain.overall.is_hidden());
        }
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
