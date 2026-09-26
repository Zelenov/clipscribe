//! Reading `.srt` subtitle files into [`Cue`]s for [`crate::describe`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Cue;

/// Parse SRT text. Malformed blocks are skipped rather than failing the whole file; cues come
/// back sorted by start.
pub fn parse(source: &str) -> Vec<Cue> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let normalized = source.replace("\r\n", "\n");
    let mut cues: Vec<Cue> = normalized.split("\n\n").filter_map(parse_block).collect();
    cues.sort_by_key(|cue| cue.start);
    cues
}

/// The subtitle file of a video: same folder and name, `.srt` extension.
pub fn subtitle_path(video: &Path) -> PathBuf {
    video.with_extension("srt")
}

/// The cues of the `.srt` next to `video`; empty when there is none. A file that exists but
/// cannot be read is an error.
pub fn load_for(video: &Path) -> std::io::Result<Vec<Cue>> {
    match std::fs::read(subtitle_path(video)) {
        Ok(bytes) => Ok(parse(&String::from_utf8_lossy(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn parse_block(block: &str) -> Option<Cue> {
    let mut lines = block
        .lines()
        .map(str::trim_end)
        .skip_while(|l| l.trim().is_empty());
    let mut timing = lines.next()?;
    if !timing.contains("-->") {
        // The first line was the cue number.
        timing = lines.next()?;
    }
    let (start, end) = timing.split_once("-->")?;
    let start = parse_timestamp(start)?;
    // Some files append positioning after the end time: `00:00:02,000 X1:...`.
    let end = parse_timestamp(end.split_whitespace().next()?)?;
    let text = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    if text.is_empty() || end <= start {
        return None;
    }
    Some(Cue { start, end, text })
}

/// Parse `HH:MM:SS,mmm` (a `.` separator is accepted too).
fn parse_timestamp(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (clock, millis) = value.split_once([',', '.']).unwrap_or((value, "0"));
    let mut parts = clock.split(':').map(|p| p.trim().parse::<u64>().ok());
    let hours = parts.next()??;
    let minutes = parts.next()??;
    let seconds = parts.next()??;
    if parts.next().is_some() {
        return None;
    }
    let millis: u64 = millis.trim().parse().ok()?;
    Some(Duration::from_millis(
        ((hours * 60 + minutes) * 60 + seconds) * 1000 + millis,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn parses_a_crlf_file_with_a_bom_and_a_multiline_cue() {
        let cues = parse(
            "\u{feff}1\r\n00:00:08,850 --> 00:00:11,730\r\nFirst line\r\nsecond line\r\n\r\n\
             2\r\n00:00:12.810 --> 00:00:17,310 X1:0\r\nSecond cue\r\n",
        );
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (ms(8_850), ms(11_730)));
        assert_eq!(cues[0].text, "First line\nsecond line");
        assert_eq!((cues[1].start, cues[1].end), (ms(12_810), ms(17_310)));
    }

    #[test]
    fn broken_blocks_are_skipped_and_cues_sorted() {
        let cues = parse(
            "1\n00:00:05,000 --> 00:00:06,000\nLater\n\n\
             2\nnot a time\nBroken\n\n\
             3\n00:00:03,000 --> 00:00:02,000\nBackwards\n\n\
             4\n00:00:01,000 --> 00:00:02,000\nEarlier\n",
        );
        let texts: Vec<&str> = cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["Earlier", "Later"]);
    }

    #[test]
    fn the_subtitle_file_sits_next_to_the_video() {
        assert_eq!(
            subtitle_path(Path::new("a/clip.mp4")),
            PathBuf::from("a/clip.srt")
        );
        assert!(load_for(Path::new("no/such/clip.mp4"))
            .expect("missing is fine")
            .is_empty());
    }
}
