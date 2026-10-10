//! Reading `.ass` / `.ssa` (Advanced SubStation Alpha) subtitle files into [`Cue`]s, as plain text.
//!
//! Only the dialogue is read: the `[Events]` section by its `Format:` line (files from Aegisub,
//! Subtitle Edit, YouTube and ffmpeg order the columns differently), `Dialogue:` lines only
//! (`Comment:` lines are skipped). Override tags (`{\b1}`, `{\pos(…)}`, `{\an8}`), drawing
//! commands (`{\p1}`…`{\p0}`) and styles are dropped; `\N` and `\n` become line breaks.
//! Files are read as UTF-8 (a BOM is fine); another encoding gives no cues.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Cue;

/// The extensions read, in the order they are tried.
pub const EXTENSIONS: [&str; 2] = ["ass", "ssa"];

/// Parse ASS/SSA text. Broken lines are skipped rather than failing the whole file; cues come
/// back sorted by start.
pub fn parse(source: &str) -> Vec<Cue> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let mut in_events = false;
    let mut columns: Option<Columns> = None;
    let mut cues = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.eq_ignore_ascii_case("format") {
            columns = Columns::from_format(value);
        } else if key.eq_ignore_ascii_case("dialogue") {
            if let Some(cue) = columns.as_ref().and_then(|c| c.cue(value)) {
                cues.push(cue);
            }
        }
    }
    cues.sort_by_key(|cue| cue.start);
    cues
}

/// The subtitle file of a video that exists: `clip.ass`, else `clip.ssa`.
pub fn existing_path(video: &Path) -> Option<PathBuf> {
    EXTENSIONS
        .iter()
        .map(|ext| video.with_extension(ext))
        .find(|path| path.is_file())
}

/// The cues of the `.ass` / `.ssa` next to `video`; empty when there is none. A file that exists
/// but cannot be read is an error.
pub fn load_for(video: &Path) -> std::io::Result<Vec<Cue>> {
    match existing_path(video) {
        Some(path) => Ok(parse(&String::from_utf8_lossy(&std::fs::read(path)?))),
        None => Ok(Vec::new()),
    }
}

/// Where the columns of the `Dialogue:` lines are, from the `Format:` line.
struct Columns {
    count: usize,
    start: usize,
    end: usize,
    text: usize,
}

impl Columns {
    fn from_format(format: &str) -> Option<Self> {
        let mut names: Vec<String> = format
            .split(',')
            .map(|name| name.trim().to_ascii_lowercase())
            .collect();
        // Some tools end the line with a comma.
        while names.last().is_some_and(String::is_empty) {
            names.pop();
        }
        let find = |wanted: &str| names.iter().position(|name| name == wanted);
        Some(Self {
            count: names.len(),
            start: find("start")?,
            end: find("end")?,
            text: find("text")?,
        })
    }

    fn cue(&self, dialogue: &str) -> Option<Cue> {
        // The text is the last column and may hold commas, so only `count` fields are split.
        let fields: Vec<&str> = dialogue.trim_start().splitn(self.count, ',').collect();
        if fields.len() < self.count || self.text + 1 != self.count {
            return None;
        }
        let start = parse_timestamp(fields[self.start])?;
        let end = parse_timestamp(fields[self.end])?;
        let text = plain_text(fields[self.text]);
        if text.is_empty() || end <= start {
            return None;
        }
        Some(Cue { start, end, text })
    }
}

/// `H:MM:SS.cc` (centiseconds; any number of fraction digits is read).
fn parse_timestamp(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (clock, fraction) = value.split_once('.').unwrap_or((value, "0"));
    let mut parts = clock.split(':').map(|p| p.trim().parse::<u64>().ok());
    let hours = parts.next()??;
    let minutes = parts.next()??;
    let seconds = parts.next()??;
    if parts.next().is_some()
        || fraction.is_empty()
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = &fraction[..fraction.len().min(3)];
    let millis = digits.parse::<u64>().ok()? * 10u64.pow(3 - digits.len() as u32);
    // A junk line (`99999999999999999:00:00.00`) is skipped, not an overflow.
    let seconds = hours
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(60)?
        .checked_add(seconds)?;
    Some(Duration::from_millis(
        seconds.checked_mul(1000)?.checked_add(millis)?,
    ))
}

/// The shown text: override blocks and drawings dropped, `\N` / `\n` as line breaks, `\h` as a space.
fn plain_text(raw: &str) -> String {
    let mut out = String::new();
    let mut drawing = false;
    // Once a `{` has no `}` after it, no later one has: they are text, and not searched for again.
    let mut closes = raw.contains('}');
    let mut rest = raw;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('{').filter(|_| closes) {
            // An unclosed brace is text.
            closes = after.contains('}');
            if let Some((block, tail)) = after.split_once('}') {
                if let Some(mode) = drawing_mode(block) {
                    drawing = mode;
                }
                rest = tail;
                continue;
            }
        }
        let mut chars = rest.chars();
        let c = chars.next().expect("not empty");
        let tail = chars.as_str();
        if c == '\\' {
            match tail.chars().next() {
                Some('N' | 'n') => {
                    if !drawing {
                        out.push('\n');
                    }
                    rest = &tail[1..];
                    continue;
                }
                Some('h') => {
                    if !drawing {
                        out.push(' ');
                    }
                    rest = &tail[1..];
                    continue;
                }
                _ => {}
            }
        }
        if !drawing {
            out.push(c);
        }
        rest = tail;
    }
    out.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `Some(true)` when the override block starts a drawing (`\p1`…), `Some(false)` when it ends one
/// (`\p0`), `None` when it says nothing about drawings. The last `\p` in the block wins.
fn drawing_mode(block: &str) -> Option<bool> {
    let mut mode = None;
    let mut rest = block;
    while let Some(at) = rest.find("\\p") {
        rest = &rest[at + 2..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        // `\pos(…)`, `\pbo` and the like have no digits right after `\p`.
        if let Ok(level) = digits.parse::<u32>() {
            mode = Some(level > 0);
        }
    }
    mode
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    const AEGISUB: &str =
        "[Script Info]\n; Script generated by Aegisub\nTitle: Default\nScriptType: v4.00+\n\n\
        [V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n\
        [Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
        Dialogue: 0,0:00:08.85,0:00:11.73,Default,,0,0,0,,First line\\Nsecond line\n\
        Dialogue: 0,0:00:12.81,0:00:17.31,Default,,0,0,0,,Hello, world\n";

    #[test]
    fn reads_the_aegisub_defaults() {
        let cues = parse(AEGISUB);
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (ms(8_850), ms(11_730)));
        assert_eq!(cues[0].text, "First line\nsecond line");
        assert_eq!(cues[1].text, "Hello, world", "commas in the text stay");
    }

    #[test]
    fn reads_the_columns_by_the_format_line() {
        let cues = parse(
            "[Events]\nFormat: Marked, Text, End, Start\n\
             Dialogue: Marked=0,Reordered,0:00:03.00,0:00:01.00\n",
        );
        // The text is not the last column here: such a line cannot be split safely and is skipped.
        assert!(cues.is_empty());
        let cues = parse(
            "[Events]\nFormat: Marked, End, Start, Text\n\
             Dialogue: Marked=0,0:00:03.00,0:00:01.00,Reordered\n",
        );
        assert_eq!((cues[0].start, cues[0].end), (ms(1_000), ms(3_000)));
        assert_eq!(cues[0].text, "Reordered");
    }

    #[test]
    fn drops_override_tags_and_drawings() {
        let cues = parse(
            "[Events]\nFormat: Start, End, Text\n\
             Dialogue: 0:00:01.00,0:00:02.00,{\\an8\\pos(10,20)}{\\b1}Bold{\\b0} and {\\i1}italic\n\
             Dialogue: 0:00:03.00,0:00:04.00,{\\p1}m 0 0 l 10 10{\\p0}Visible\n\
             Dialogue: 0:00:05.00,0:00:06.00,{\\p1}m 0 0 l 10 10{\\p0}\n",
        );
        let texts: Vec<&str> = cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["Bold and italic", "Visible"]);
    }

    #[test]
    fn line_breaks_and_hard_spaces() {
        let cues = parse(
            "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,a\\Nb\\nc\\hd {broken\n",
        );
        assert_eq!(cues[0].text, "a\nb\nc d {broken");
    }

    #[test]
    fn comments_are_skipped_and_cues_sorted() {
        let cues = parse(
            "[Events]\nFormat: Start, End, Text\n\
             Dialogue: 0:00:05.00,0:00:06.00,Later\n\
             Comment: 0:00:01.00,0:00:02.00,Not shown\n\
             Dialogue: broken\n\
             Dialogue: 0:00:03.00,0:00:02.00,Backwards\n\
             Dialogue: 0:00:01.00,0:00:02.00,Earlier\n",
        );
        let texts: Vec<&str> = cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["Earlier", "Later"]);
    }

    #[test]
    fn reads_a_crlf_file_with_a_bom_and_ssa_headers() {
        let cues = parse(
            "\u{feff}[Script Info]\r\nScriptType: v4.00\r\n\r\n[events]\r\n\
             Format: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n\
             Dialogue: Marked=0,0:00:01.5,0:00:02.25,Default,,0,0,0,,Hi\r\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!((cues[0].start, cues[0].end), (ms(1_500), ms(2_250)));
    }

    #[test]
    fn lines_outside_events_are_ignored() {
        let cues = parse(
            "[Fonts]\nDialogue: 0:00:01.00,0:00:02.00,Nope\n[Events]\nDialogue: 0:00:01.00,0:00:02.00,No format\n",
        );
        assert!(cues.is_empty());
    }

    #[test]
    fn absurd_times_skip_the_line_instead_of_overflowing() {
        let cues = parse(
            "[Events]\nFormat: Start, End, Text\n\
             Dialogue: 99999999999999999:00:00.00,99999999999999999:00:01.00,Huge\n\
             Dialogue: 18446744073709551615:00:00.00,18446744073709551615:00:01.00,Max\n\
             Dialogue: 0:00:01.00,0:00:02.00,Fine\n",
        );
        let texts: Vec<&str> = cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["Fine"]);
    }

    #[test]
    fn a_trailing_comma_in_format_and_a_space_before_the_colon_are_fine() {
        let cues = parse(
            "[Events]\nFormat : Start, End, Text,\nDialogue: 0:00:01.00,0:00:02.00,Hi, there\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi, there");
    }

    #[test]
    fn many_unclosed_braces_are_text_and_do_not_slow_the_parse_down() {
        let line = format!(
            "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,{}\n",
            "{".repeat(200_000)
        );
        let started = std::time::Instant::now();
        let cues = parse(&line);
        assert_eq!(cues[0].text.len(), 200_000);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_subtitle_file_sits_next_to_the_video() {
        let dir = std::env::temp_dir().join(format!("clipscribe-ass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let video = dir.join("clip.mp4");
        assert!(existing_path(&video).is_none());
        assert!(load_for(&video).expect("missing is fine").is_empty());
        std::fs::write(dir.join("clip.ssa"), AEGISUB).expect("ssa");
        assert_eq!(existing_path(&video), Some(dir.join("clip.ssa")));
        std::fs::write(dir.join("clip.ass"), AEGISUB).expect("ass");
        assert_eq!(
            existing_path(&video),
            Some(dir.join("clip.ass")),
            ".ass first"
        );
        assert_eq!(load_for(&video).expect("read").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
