//! A description written into files an editor or a website can use: JSON, Markdown, plain
//! text, SRT and WebVTT cues, CSV, YouTube-style chapters and a Premiere Pro XMP sidecar.
//!
//! Everything here is pure: no network, no clock, no randomness, so the same description renders
//! to the same bytes. [`render`] gives one format as a `String`; [`write_all`] writes several
//! next to the video or into a folder.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::provider::AiUsage;
use crate::{
    format_time, Described, DescribedWithTags, Description, Model, Segment, TagSuggestions,
};

/// A file format a description can be written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// The object `--json` prints for one video, including the main range, tags and usage.
    Json,
    /// The summary, main range, moments and tags as a readable Markdown page.
    Markdown,
    /// The same as the command line prints.
    Text,
    /// Moments as timed SRT cues.
    Srt,
    /// Moments as timed WebVTT cues.
    Vtt,
    /// One row per moment (and one for the main range), for spreadsheets and bulk marker import.
    Csv,
    /// YouTube-style `0:00 Title` lines, one per moment.
    Chapters,
    /// A Premiere Pro XMP sidecar (`<video stem>.xmp`): moments as Comment markers, the main
    /// range as an InOut marker.
    Xmp,
}

impl Format {
    /// Every format, in the order `--format` lists them.
    pub const ALL: [Format; 8] = [
        Format::Json,
        Format::Markdown,
        Format::Text,
        Format::Srt,
        Format::Vtt,
        Format::Csv,
        Format::Chapters,
        Format::Xmp,
    ];

    /// The name used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Markdown => "md",
            Format::Text => "txt",
            Format::Srt => "srt",
            Format::Vtt => "vtt",
            Format::Csv => "csv",
            Format::Chapters => "chapters",
            Format::Xmp => "xmp",
        }
    }

    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Chapters => "txt",
            other => other.name(),
        }
    }
}

/// A described clip with what [`render`] needs around the description: the video it came
/// from, its cost, and the tag suggestions when there are any. Built with [`Export::new`] or
/// [`Export::with_tags`] from what [`crate::describe`] / [`crate::describe_with_tags`] returned.
#[derive(Debug, Clone)]
pub struct Export<'a> {
    video: &'a Path,
    description: &'a Description,
    tags: Option<&'a TagSuggestions>,
    duration_s: f64,
    frames: usize,
    usage: AiUsage,
    model: Model,
}

impl<'a> Export<'a> {
    /// A clip described with [`crate::describe`]; `model` prices the usage.
    pub fn new(video: &'a Path, described: &'a Described, model: Model) -> Self {
        Self {
            video,
            description: &described.description,
            tags: None,
            duration_s: described.duration_s,
            frames: described.frames,
            usage: described.usage,
            model,
        }
    }

    /// A clip described with [`crate::describe_with_tags`]; `model` prices the usage.
    pub fn with_tags(video: &'a Path, described: &'a DescribedWithTags, model: Model) -> Self {
        Self {
            video,
            description: &described.description,
            tags: Some(&described.tags),
            duration_s: described.duration_s,
            frames: described.frames,
            usage: described.usage,
            model,
        }
    }

    /// The video this description is of.
    pub fn video(&self) -> &Path {
        self.video
    }

    fn cost_usd(&self) -> f64 {
        self.model.cost_usd(self.usage)
    }

    fn stem(&self) -> String {
        stem_of(self.video)
    }

    /// The JSON object for this clip: what `--json` prints per video.
    pub fn to_json(&self) -> serde_json::Value {
        let description = self.description;
        let mut value = json!({
            "file": self.video.display().to_string(),
            "duration_s": self.duration_s,
            "frames": self.frames,
            "summary": description.summary,
            "main": match description.main {
                Some(main) => json!({"start_s": main.start_s, "end_s": main.end_s}),
                None => serde_json::Value::Null,
            },
            "moments": description.segments.iter().map(|m| json!({
                "start_s": m.start_s,
                "end_s": m.end_s,
                "description": m.description,
            })).collect::<Vec<_>>(),
            "model": self.model.id,
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens,
            },
            "cost_usd": self.cost_usd(),
        });
        if let Some(tags) = self.tags {
            value["tags"] = tags
                .tags
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "confidence": t.confidence,
                        "ranges": t.ranges.iter().map(|r| json!({
                            "start_s": r.start_s,
                            "end_s": r.end_s,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>()
                .into();
            value["new_tag_ideas"] = json!(tags.new_tag_ideas);
        }
        value
    }
}

/// The video's file name without the extension, `clip` when it has none.
fn stem_of(video: &Path) -> String {
    video
        .file_stem()
        .map_or_else(|| "clip".to_string(), |s| s.to_string_lossy().into_owned())
}

/// `export` as `format`. Always ends with a newline (except an empty SRT, which is empty).
pub fn render(export: &Export, format: Format) -> String {
    match format {
        Format::Json => {
            let mut text = serde_json::to_string_pretty(&export.to_json()).unwrap_or_default();
            text.push('\n');
            text
        }
        Format::Markdown => markdown(export),
        Format::Text => text(export),
        Format::Srt => cues(export, false),
        Format::Vtt => cues(export, true),
        Format::Csv => csv(export),
        Format::Chapters => chapters(export),
        Format::Xmp => xmp(export),
    }
}

fn range(start_s: f64, end_s: f64) -> String {
    format!("{}\u{2013}{}", format_time(start_s), format_time(end_s))
}

/// The tag suggestions as `name 85% (0:10–0:20, …)` lines' parts.
fn tag_line(tag: &crate::TagSuggestion) -> String {
    let ranges: Vec<String> = tag
        .ranges
        .iter()
        .map(|r| range(r.start_s, r.end_s))
        .collect();
    let where_ = if ranges.is_empty() {
        String::new()
    } else {
        format!(" ({})", ranges.join(", "))
    };
    format!("{} {:.0}%{where_}", tag.name, tag.confidence * 100.0)
}

fn text(export: &Export) -> String {
    let d = export.description;
    let mut out = format!(
        "{}  {} \u{b7} {} frames \u{b7} ${:.4}\n",
        export.video.display(),
        format_time(export.duration_s),
        export.frames,
        export.cost_usd()
    );
    let _ = writeln!(out, "  {}", d.summary);
    if let Some(main) = d.main {
        let _ = writeln!(out, "  Main: {}", range(main.start_s, main.end_s));
    }
    for moment in &d.segments {
        let _ = writeln!(
            out,
            "  {}  {}",
            range(moment.start_s, moment.end_s),
            moment.description
        );
    }
    if let Some(tags) = export.tags {
        if !tags.tags.is_empty() {
            out.push_str("  Tags:\n");
            for tag in &tags.tags {
                let _ = writeln!(out, "    {}", tag_line(tag));
            }
        }
        if !tags.new_tag_ideas.is_empty() {
            let _ = writeln!(out, "  New tag ideas: {}", tags.new_tag_ideas.join(", "));
        }
    }
    out
}

fn markdown(export: &Export) -> String {
    let d = export.description;
    let name = export
        .video
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut out = format!("# {name}\n\n{}\n", d.summary);
    if let Some(main) = d.main {
        let _ = write!(
            out,
            "\n**Main range:** {}\n",
            range(main.start_s, main.end_s)
        );
    }
    if !d.segments.is_empty() {
        out.push_str("\n## Moments\n\n");
        for moment in &d.segments {
            let _ = writeln!(
                out,
                "- **{}** {}",
                range(moment.start_s, moment.end_s),
                moment.description.replace('\n', " ")
            );
        }
    }
    if let Some(tags) = export.tags {
        if !tags.tags.is_empty() {
            out.push_str("\n## Tags\n\n");
            for tag in &tags.tags {
                let _ = writeln!(out, "- {}", tag_line(tag));
            }
        }
        if !tags.new_tag_ideas.is_empty() {
            let _ = write!(out, "\nNew tag ideas: {}\n", tags.new_tag_ideas.join(", "));
        }
    }
    out
}

/// `HH:MM:SS<sep>mmm`, rounded to the millisecond.
fn cue_time(seconds: f64, separator: char) -> String {
    let total_ms = millis(seconds);
    let (ms, s) = (total_ms % 1000, total_ms / 1000);
    format!(
        "{:02}:{:02}:{:02}{separator}{ms:03}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

fn cues(export: &Export, vtt: bool) -> String {
    let separator = if vtt { '.' } else { ',' };
    let mut out = String::new();
    if vtt {
        out.push_str("WEBVTT\n");
    }
    let mut number = 0;
    for moment in &export.description.segments {
        // A blank line would end the cue, so lines are trimmed and empty ones dropped.
        let text = moment
            .description
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| {
                if vtt {
                    // WebVTT reads `-->` as timing and `<` and `&` as markup.
                    l.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace("-->", "--&gt;")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        number += 1;
        if vtt {
            out.push('\n');
        } else {
            let _ = writeln!(out, "{number}");
        }
        let _ = writeln!(
            out,
            "{} --> {}",
            cue_time(moment.start_s, separator),
            cue_time(moment.end_s, separator)
        );
        let _ = writeln!(out, "{text}");
        if !vtt {
            out.push('\n');
        }
    }
    out
}

/// A field for a CSV row: quoted when it holds a comma, quote or line break, and a leading
/// `'` keeps a spreadsheet from reading it as a formula.
fn csv_field(value: &str) -> String {
    // A spreadsheet runs a cell that starts with one of these as a formula.
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        return csv_field(&format!("'{value}"));
    }
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Names of the suggested tags that apply anywhere in `start_s..end_s`: the ones without ranges
/// (the whole clip) and the ones with a range overlapping it.
fn tags_in(tags: Option<&TagSuggestions>, start_s: f64, end_s: f64) -> Vec<&str> {
    let Some(tags) = tags else {
        return Vec::new();
    };
    tags.tags
        .iter()
        .filter(|t| {
            t.ranges.is_empty()
                || t.ranges
                    .iter()
                    .any(|r| r.start_s < end_s && r.end_s > start_s)
        })
        .map(|t| t.name.as_str())
        .collect()
}

fn csv(export: &Export) -> String {
    let d = export.description;
    let mut out = String::from("kind,start_s,end_s,start,end,description,tags\n");
    let mut row = |kind: &str, start_s: f64, end_s: f64, description: &str| {
        let _ = writeln!(
            out,
            "{kind},{start_s:.3},{end_s:.3},{},{},{},{}",
            format_time(start_s),
            format_time(end_s),
            csv_field(description),
            csv_field(&tags_in(export.tags, start_s, end_s).join(";"))
        );
    };
    if let Some(main) = d.main {
        row("main", main.start_s, main.end_s, &d.summary);
    }
    for moment in &d.segments {
        row("moment", moment.start_s, moment.end_s, &moment.description);
    }
    out
}

/// A one-line title for a moment: its first sentence, cut to `max` characters.
fn title(description: &str, max: usize) -> String {
    let line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = line
        .char_indices()
        .find(|&(i, c)| matches!(c, '.' | '!' | '?') && line[i + c.len_utf8()..].starts_with(' '))
        .map_or(line.len(), |(i, c)| i + c.len_utf8());
    let sentence = line[..end].trim_end_matches('.');
    let sentence = if sentence.is_empty() { &line } else { sentence };
    if sentence.chars().count() <= max {
        sentence.to_string()
    } else {
        let cut: String = sentence.chars().take(max.saturating_sub(1)).collect();
        format!("{}\u{2026}", cut.trim_end())
    }
}

fn chapters(export: &Export) -> String {
    let mut moments = export
        .description
        .segments
        .iter()
        .filter(|m| !m.description.trim().is_empty())
        .peekable();
    let mut out = String::new();
    // YouTube wants the first chapter at 0:00.
    if moments.peek().is_some_and(|m| m.start_s >= 1.0) {
        let _ = writeln!(out, "0:00 {}", export.stem());
    }
    for moment in moments {
        let _ = writeln!(
            out,
            "{} {}",
            format_time(moment.start_s),
            title(&moment.description, 100)
        );
    }
    out
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#xD;"),
            '\r' => {}
            c if (c as u32) < 0x20 && c != '\t' => {}
            '\u{fffe}' | '\u{ffff}' => {}
            c => out.push(c),
        }
    }
    out
}

/// A stable UUID-shaped id for a marker: the 128-bit FNV-1a hash of `seed`.
fn guid(seed: &str) -> String {
    let mut hash: u128 = 0x6c62272e07bb014262b821756295c58d;
    for byte in seed.bytes() {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(0x0000000001000000000000000000013b);
    }
    let h = format!("{hash:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn millis(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

fn xmp_marker(stem: &str, index: usize, moment: &Segment) -> String {
    let start = millis(moment.start_s);
    let duration = millis(moment.end_s).saturating_sub(start);
    let id = guid(&format!("{stem}|{index}|{start}|{}", moment.description));
    format!(
        "      <rdf:li rdf:parseType=\"Resource\">\n\
         \x20      <xmpDM:startTime>{start}</xmpDM:startTime>\n\
         \x20      <xmpDM:duration>{duration}</xmpDM:duration>\n\
         \x20      <xmpDM:name>{}</xmpDM:name>\n\
         \x20      <xmpDM:comment>{}</xmpDM:comment>\n\
         \x20      <xmpDM:guid>{id}</xmpDM:guid>\n\
         \x20      <xmpDM:cuePointParams><rdf:Seq>\n\
         \x20       <rdf:li rdf:parseType=\"Resource\"><xmpDM:key>marker_guid</xmpDM:key>\n\
         \x20         <xmpDM:value>{id}</xmpDM:value></rdf:li>\n\
         \x20      </rdf:Seq></xmpDM:cuePointParams>\n\
         \x20     </rdf:li>\n",
        xml_escape(&title(&moment.description, 60)),
        xml_escape(&moment.description),
    )
}

/// The same structure frename writes: a Comment track in milliseconds (`f1000`) with the
/// moments, an InOut track with the main range. See frename's
/// `docs/research/premiere-xmp-markers.md`.
fn xmp(export: &Export) -> String {
    let d = export.description;
    let stem = export.stem();
    let mut out = String::from(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
         <rdf:Description rdf:about=\"\" xmlns:xmpDM=\"http://ns.adobe.com/xmp/1.0/DynamicMedia/\">\n",
    );
    if !d.segments.is_empty() || d.main.is_some() {
        out.push_str("   <xmpDM:Tracks><rdf:Bag>\n");
        if !d.segments.is_empty() {
            out.push_str(
                "    <rdf:li rdf:parseType=\"Resource\">\n     \
                 <xmpDM:trackName>Comment</xmpDM:trackName>\n     \
                 <xmpDM:trackType>Comment</xmpDM:trackType>\n     \
                 <xmpDM:frameRate>f1000</xmpDM:frameRate>\n     \
                 <xmpDM:markers><rdf:Seq>\n",
            );
            for (i, moment) in d.segments.iter().enumerate() {
                out.push_str(&xmp_marker(&stem, i, moment));
            }
            out.push_str("     </rdf:Seq></xmpDM:markers>\n    </rdf:li>\n");
        }
        if let Some(main) = d.main {
            let start = millis(main.start_s);
            let duration = millis(main.end_s).saturating_sub(start);
            let _ = write!(
                out,
                "    <rdf:li rdf:parseType=\"Resource\">\n     \
                 <xmpDM:trackName>InOut</xmpDM:trackName>\n     \
                 <xmpDM:trackType>InOut</xmpDM:trackType>\n     \
                 <xmpDM:frameRate>f1000</xmpDM:frameRate>\n     \
                 <xmpDM:markers><rdf:Seq><rdf:li><rdf:Description xmpDM:startTime=\"{start}\" \
                 xmpDM:duration=\"{duration}\" xmpDM:name=\"in-out\"/></rdf:li></rdf:Seq></xmpDM:markers>\n    \
                 </rdf:li>\n"
            );
        }
        out.push_str("   </rdf:Bag></xmpDM:Tracks>\n");
    }
    out.push_str("  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n");
    out
}

/// What [`write_all`] did with one format.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Written {
    /// The format that was written.
    pub format: Format,
    /// The file it went to.
    pub path: PathBuf,
    /// The file already existed and `force` was off, so it was left alone.
    pub skipped: bool,
}

/// Where `format` goes for `video`: `<stem>.clipscribe.<ext>` next to the video or in
/// `out_dir`. Two of `formats` sharing an extension (`txt` and `chapters`) are named
/// `<stem>.<format>.<ext>` instead, and the XMP sidecar is always `<stem>.xmp`, the name
/// Premiere looks for.
fn output_path(
    video: &Path,
    out_dir: Option<&Path>,
    format: Format,
    formats: &[Format],
) -> PathBuf {
    let stem = stem_of(video);
    let name = if format == Format::Xmp {
        format!("{stem}.xmp")
    } else if formats
        .iter()
        .any(|&f| f != format && f != Format::Xmp && f.extension() == format.extension())
    {
        format!("{stem}.{}.{}", format.name(), format.extension())
    } else {
        format!("{stem}.clipscribe.{}", format.extension())
    };
    let dir = out_dir
        .map(Path::to_path_buf)
        .or_else(|| video.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    dir.join(name)
}

/// The files [`write_all`] writes for `video`, in the order of `formats` (a format named twice
/// counts once), with their paths. Nothing is touched, so it also serves to check beforehand
/// what exists or collides.
pub fn plan(video: &Path, formats: &[Format], out_dir: Option<&Path>) -> Vec<(Format, PathBuf)> {
    let mut unique: Vec<Format> = Vec::new();
    for &format in formats {
        if !unique.contains(&format) {
            unique.push(format);
        }
    }
    unique
        .iter()
        .map(|&format| (format, output_path(video, out_dir, format, &unique)))
        .collect()
}

/// Write `export` in each of `formats` (see [`plan`] for the names). An existing file is left
/// alone and reported as skipped unless `force`. The output folder is created. Stops at the
/// first error; files already written stay.
pub fn write_all(
    export: &Export,
    formats: &[Format],
    out_dir: Option<&Path>,
    force: bool,
) -> std::io::Result<Vec<Written>> {
    let mut written = Vec::new();
    for (format, path) in plan(export.video, formats, out_dir) {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let text = render(export, format);
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if force {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        let skipped = match options.open(&path) {
            Ok(mut file) => {
                if let Err(e) = std::io::Write::write_all(&mut file, text.as_bytes()) {
                    // A truncated file would count as "exists" on the next run.
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                false
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => true,
            Err(e) => return Err(e),
        };
        written.push(Written {
            format,
            path,
            skipped,
        });
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
