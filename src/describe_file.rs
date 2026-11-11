//! The video description file, version 1: `clip.describe.json` next to the video, as
//! `docs/describe-format.md` defines it. One JSON file per video with what a description run
//! found (summary, parts, moments, people, faces, texts, tags) and what a person set (In/Out,
//! marker names, colours), so markers, names and caption placement all read the same data.
//!
//! - [`DescribeFile::parse`] / [`DescribeFile::to_json`] read and write it. Fields this version
//!   does not know are kept (each struct's `extra`) and written back.
//! - [`DescribeFile::problems`] lists every rule of the format the file breaks.
//! - [`DescribeFile::from_described`] makes one from a [`crate::Described`] run.
//! - [`DescribeFile::merge_run`] puts a new run over an existing file: everything `by: user` is
//!   kept, everything `by: ai` is replaced.
//! - [`render_mentions`] shows `@pN` as a name.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::describe::{marker_title, Description, MomentsMode, MAX_TITLE_CHARS, MAX_TITLE_WORDS};
use crate::shots::{Shots, TransitionKind};
use crate::tags::TagSuggestions;

/// The format version this crate reads and writes.
pub const FORMAT_VERSION: u64 = 1;

/// The extension of the file after the video's stem: `clip.mp4` → `clip.describe.json`.
pub const EXTENSION: &str = "describe.json";

/// A new AI moment closer than this to a user point moment is dropped by
/// [`DescribeFile::merge_run`]: one marker per moment.
pub const MOMENT_SNAP_S: f64 = 0.5;

/// A new AI part left shorter than this by [`DescribeFile::merge_run`] is dropped, its time going
/// to the part before it.
pub const MIN_PART_S: f64 = 1.0;

/// Tags under this confidence are not stored by [`save_run`].
pub const DEFAULT_MIN_TAG_CONFIDENCE: f64 = 0.5;

/// The longest text of a text [`Track`], in characters.
pub const MAX_TEXT_CHARS: usize = 200;

/// Premiere's nine marker colours, the names [`Marker::color`] uses.
pub const PREMIERE_COLORS: [&str; 9] = [
    "green", "red", "orange", "yellow", "white", "blue", "cyan", "lavender", "magenta",
];

/// Where the description file of `video` is: `/dir/clip.mp4` → `/dir/clip.describe.json`.
pub fn describe_file_path(video: &Path) -> PathBuf {
    video.with_extension(EXTENSION)
}

/// Fields of a JSON object that this version does not know, kept as they are.
pub type Extra = Map<String, Value>;

/// One video's description file. `None` (and an empty `Vec` where noted) is an absent field:
/// for analysis it means "not done", for user data "not set"; `Some(vec![])` is "done, nothing
/// found".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DescribeFile {
    pub version: u64,
    pub duration_s: f64,
    pub frame: FrameInfo,
    /// The language of `summary`, titles, descriptions and labels (not of the speech).
    pub lang: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The AI's suggested start and end of the real content; never applied by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usable: Option<Span>,
    /// The In and Out a person set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_out: Option<InOut>,
    /// Shot changes, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cuts: Option<Vec<f64>>,
    /// Chapters covering 0..`duration_s` without gaps or overlaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<Marker>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moments: Option<Vec<Marker>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub people: Option<Vec<Person>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faces: Option<Vec<Track>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texts: Option<Vec<Track>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<FileTag>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The picture as shown: size after rotation and pixel aspect, and the nominal frame rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A stretch of the video, seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub start_s: f64,
    pub end_s: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The In and Out a person set; a missing one is the video's own start or end.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InOut {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_s: Option<f64>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Who made an item: a description run (replaced by the next one) or a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum By {
    Ai,
    User,
}

/// A part or a moment: a marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: String,
    pub by: By,
    pub start_s: f64,
    /// Always set on parts; absent on a point moment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The editor's colour code, kept exactly: one of [`PREMIERE_COLORS`] or `#rrggbb[aa]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Marker {
    /// Where it ends: `end_s`, or `start_s` for a point.
    pub fn end_or_start(&self) -> f64 {
        self.end_s.unwrap_or(self.start_s)
    }
}

/// A person the text talks about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    /// `p` and a number.
    pub id: String,
    pub by: By,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The speaker id in the video's CAPT file (`speaker:1` → `"1"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faces: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A face (`f…`) or a text (`x…`) on screen and where it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub start_s: f64,
    pub end_s: f64,
    pub track: Vec<Sample>,
    /// Texts only: what it says, lines as `\n`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Where a face or text is at `t_s`: `[x, y, width, height]` as fractions of the frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub t_s: f64,
    #[serde(rename = "box")]
    pub bbox: [f64; 4],
    #[serde(flatten)]
    pub extra: Extra,
}

/// A tag from the user's list that applies, to the whole video or to `ranges`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTag {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranges: Option<Vec<Span>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Why a file could not be read.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadError {
    /// Not JSON, or not the shape of this format; the reason is for the log.
    Invalid(String),
    /// A version this crate does not read.
    Version(u64),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(reason) => write!(f, "not a description file ({reason})"),
            Self::Version(v) => write!(
                f,
                "description file version {v}; this version reads {FORMAT_VERSION}"
            ),
        }
    }
}

impl std::error::Error for ReadError {}

/// What a run knows beyond its [`Description`], for [`DescribeFile::from_described`].
#[derive(Debug, Clone)]
pub struct RunFacts<'a> {
    pub duration_s: f64,
    pub frame: FrameInfo,
    /// The language when the description does not report one ([`Description::lang`]).
    pub fallback_lang: &'a str,
    /// How the run was asked for moments: [`MomentsMode::Full`] segments cover the clip and
    /// become parts; [`MomentsMode::Important`] segments become moments.
    pub moments: MomentsMode,
    /// The shots, when the run found them: their changes become `cuts`.
    pub shots: Option<&'a Shots>,
    /// Suggested tags, when the run asked for them.
    pub tags: Option<&'a TagSuggestions>,
    /// Tags under this confidence are not stored.
    pub min_tag_confidence: f64,
}

impl DescribeFile {
    /// Read a file. Unknown fields are kept; a newer version is refused.
    pub fn parse(text: &str) -> Result<Self, ReadError> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| ReadError::Invalid(e.to_string()))?;
        match value.get("version").and_then(Value::as_u64) {
            Some(FORMAT_VERSION) => {}
            Some(other) => return Err(ReadError::Version(other)),
            None => return Err(ReadError::Invalid("no version".to_string())),
        }
        serde_json::from_value(value).map_err(|e| ReadError::Invalid(e.to_string()))
    }

    /// Read the file at `path`; `Ok(None)` when there is none.
    pub fn read(path: &Path) -> std::io::Result<Option<Result<Self, ReadError>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(Self::parse(&String::from_utf8_lossy(&bytes)))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The file as pretty JSON, ending with a newline. Never writes `null`.
    pub fn to_json(&self) -> String {
        // An unknown field kept as `null` is left out: the format never uses `null`.
        let mut value = serde_json::to_value(self).unwrap_or_default();
        drop_nulls(&mut value);
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Write the file to `path`, replacing it whole (through a temporary file next to it).
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.to_json())?;
        std::fs::rename(&tmp, path)
    }

    /// A file from one description run: summary, usable range, parts or moments (all `by: ai`),
    /// cuts and tags as far as the run made them. People, faces and texts are left absent (not
    /// done): this run does not look for them.
    pub fn from_described(description: &Description, facts: RunFacts) -> Self {
        // Compared with the rounded times below, so no part ends up zero-long at the end.
        let duration_s = round_ms(facts.duration_s);
        // This run names no people, so a stray `@pN` from the model names no one.
        let plain = |text: &str| drop_unknown_mentions(text, &[]);
        let mut taken = HashSet::new();
        let marker =
            |taken: &mut HashSet<String>, start_s: f64, end_s: f64, title: &str, text: &str| {
                let id = new_id(taken);
                taken.insert(id.clone());
                let title = marker_title(title, text);
                Marker {
                    id,
                    by: By::Ai,
                    start_s: round_ms(start_s),
                    end_s: Some(round_ms(end_s)),
                    description: (!text.trim().is_empty() && text.trim() != title)
                        .then(|| plain(text.trim())),
                    title: Some(title),
                    color: None,
                    extra: Extra::new(),
                }
            };
        let (parts, moments) = match facts.moments {
            MomentsMode::Full => {
                let mut segments = description.segments.clone();
                segments.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
                // Each part runs from its start to the next one's, the first from 0 and the last
                // to the end: the parts cover the clip with no gap or overlap.
                let mut starts: Vec<(f64, &crate::Segment)> = Vec::new();
                for (n, s) in segments.iter().enumerate() {
                    let start = if n == 0 {
                        0.0
                    } else {
                        round_ms(s.start_s.clamp(0.0, duration_s))
                    };
                    if starts.last().is_none_or(|(prev, _)| start > *prev) && start < duration_s {
                        starts.push((start, s));
                    }
                }
                let parts = starts
                    .iter()
                    .enumerate()
                    .map(|(n, (start, s))| {
                        let end = starts.get(n + 1).map_or(duration_s, |(next, _)| *next);
                        marker(&mut taken, *start, end, &s.title, &s.description)
                    })
                    .collect();
                (Some(parts), None)
            }
            MomentsMode::Important => {
                let mut moments: Vec<Marker> = description
                    .segments
                    .iter()
                    .filter_map(|s| {
                        let start = s.start_s.clamp(0.0, duration_s);
                        let end = s.end_s.clamp(0.0, duration_s);
                        (round_ms(end) > round_ms(start))
                            .then(|| marker(&mut taken, start, end, &s.title, &s.description))
                    })
                    .collect();
                sort_markers(&mut moments);
                (None, Some(moments))
            }
        };
        let cuts = facts.shots.map(|shots| {
            shots
                .transitions
                .iter()
                .map(|t| match t.kind {
                    TransitionKind::Cut => round_ms(t.start_s),
                    _ => round_ms((t.start_s + t.end_s) / 2.0),
                })
                .filter(|t| *t > 0.0 && *t < duration_s)
                .collect()
        });
        let tags = facts.tags.map(|tags| {
            tags.tags
                .iter()
                .filter(|t| t.confidence >= facts.min_tag_confidence)
                .map(|t| FileTag {
                    name: t.name.clone(),
                    ranges: (!t.ranges.is_empty()).then(|| {
                        t.ranges
                            .iter()
                            .map(|r| Span {
                                start_s: round_ms(r.start_s.clamp(0.0, duration_s)),
                                end_s: round_ms(r.end_s.clamp(0.0, duration_s)),
                                extra: Extra::new(),
                            })
                            .filter(|r| r.end_s > r.start_s)
                            .collect()
                    }),
                    extra: Extra::new(),
                })
                .collect()
        });
        Self {
            version: FORMAT_VERSION,
            duration_s: round_ms(duration_s),
            frame: facts.frame,
            lang: description
                .lang
                .clone()
                .unwrap_or_else(|| facts.fallback_lang.to_string()),
            summary: Some(plain(&description.summary)).filter(|s| !s.trim().is_empty()),
            usable: description.main.map(|m| Span {
                start_s: round_ms(m.start_s.clamp(0.0, duration_s)),
                end_s: round_ms(m.end_s.clamp(0.0, duration_s)),
                extra: Extra::new(),
            }),
            in_out: None,
            cuts,
            parts,
            moments,
            people: None,
            faces: None,
            texts: None,
            tags,
            extra: Extra::new(),
        }
    }

    /// Every id in the file (parts, moments, people, faces, texts).
    pub fn ids(&self) -> HashSet<String> {
        let markers = self
            .parts
            .iter()
            .chain(&self.moments)
            .flatten()
            .map(|m| &m.id);
        let people = self.people.iter().flatten().map(|p| &p.id);
        let tracks = self
            .faces
            .iter()
            .chain(&self.texts)
            .flatten()
            .map(|t| &t.id);
        markers.chain(people).chain(tracks).cloned().collect()
    }

    /// `new`, a fresh run's file, put over this one (the file as it was):
    ///
    /// - Everything `by: user` here is kept, everything `by: ai` here is replaced by `new`'s.
    /// - A new AI moment within [`MOMENT_SNAP_S`] of a user point moment is dropped.
    /// - New AI parts are cut back where a user part is; one left shorter than [`MIN_PART_S`]
    ///   is dropped, its time going to the AI part before it (or after it). A user part is never
    ///   moved: a short AI part between two user parts stays.
    /// - A new AI person matches a kept person only when its faces meet exactly one.
    /// - A new AI person whose faces overlap a kept person's faces (in time and place) is that
    ///   person: the kept person gets the new faces, and `@pN` in new text is rewritten.
    /// - Faces, texts, cuts, tags, `summary`, `usable`, `duration_s`, `frame` and `lang` come
    ///   from `new`; `in_out` and unknown fields from here.
    /// - New AI ids never repeat an id kept here.
    pub fn merge_run(&self, new: DescribeFile) -> DescribeFile {
        let mut out = new;
        out.in_out = self.in_out.clone();
        for (key, value) in &self.extra {
            out.extra
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        // Analysis the new run did not do (absent) is not "found nothing": what the old file has
        // stays, so a run without faces never loses the faces a person linked to someone.
        let faces_redone = out.faces.is_some();
        if !faces_redone {
            out.faces = self.faces.clone();
        }
        if out.texts.is_none() {
            out.texts = self.texts.clone();
        }
        if out.cuts.is_none() {
            out.cuts = self.cuts.clone();
        }
        if out.tags.is_none() {
            out.tags = self.tags.clone();
        }
        let new_summary = out.summary.take();

        // People first: matching renames new people, which their text refers to. Mentions in
        // the new run's text that name no one in its own people are dropped first.
        let new_people_given = out.people.is_some();
        let new_people: Vec<Person> = out.people.take().unwrap_or_default();
        let mut people: Vec<Person> = self
            .people
            .iter()
            .flatten()
            .filter(|p| p.by == By::User || !new_people_given)
            .cloned()
            .collect();
        people.sort_by_key(|p| p.by != By::User);
        if faces_redone {
            // A kept person's old face ids are gone with the old run (ids are made again by every
            // run): give them the new faces that lie where theirs were, each new face to one
            // person only. User people go first.
            let old_faces: HashMap<&str, &Track> = self
                .faces
                .iter()
                .flatten()
                .map(|t| (t.id.as_str(), t))
                .collect();
            let mut new_faces: Vec<&Track> = out.faces.iter().flatten().collect();
            for person in people.iter_mut() {
                let theirs: Vec<&Track> = person
                    .faces
                    .iter()
                    .filter_map(|f| old_faces.get(f.as_str()).copied())
                    .collect();
                let (mine, rest): (Vec<&Track>, Vec<&Track>) = new_faces
                    .into_iter()
                    .partition(|new| theirs.iter().any(|old| same_place(old, new)));
                new_faces = rest;
                let mut faces: Vec<String> = mine.iter().map(|t| t.id.clone()).collect();
                faces.sort();
                person.faces = faces;
            }
        }
        let mut rename: BTreeMap<String, String> = BTreeMap::new();
        let mut next_person = max_person_number(self.people.iter().flatten())
            .max(max_person_number(people.iter()))
            + 1;
        // Kept people that already took in a person of this run: one each, since a wrong merge
        // is worse than a wrong split.
        let mut absorbed: HashSet<usize> = HashSet::new();
        for mut person in new_people.iter().cloned() {
            // A new AI person is a kept one when it shares exactly one kept person's speaker,
            // or else when its faces meet exactly one kept person's.
            let free = |n: &usize| !absorbed.contains(n) && people[*n].by == By::User;
            let by_speaker: Vec<usize> = (0..people.len())
                .filter(|n| person.speaker.is_some() && people[*n].speaker == person.speaker)
                .collect();
            let by_face: Vec<usize> = (0..people.len())
                .filter(|n| person.faces.iter().any(|f| people[*n].faces.contains(f)))
                .collect();
            let candidates: Vec<usize> = match by_speaker.as_slice() {
                [] => by_face.iter().copied().filter(free).collect(),
                some => some.iter().copied().filter(free).collect(),
            };
            match candidates.as_slice() {
                [only] => {
                    absorbed.insert(*only);
                    let kept = &mut people[*only];
                    for face in &person.faces {
                        if !kept.faces.contains(face) {
                            kept.faces.push(face.clone());
                        }
                    }
                    kept.faces.sort();
                    if kept.speaker.is_none() {
                        kept.speaker = person.speaker.clone();
                    }
                    rename.insert(person.id.clone(), kept.id.clone());
                }
                _ => {
                    // Unsure: a person of its own, without what a kept person already has.
                    if people.iter().any(|kept| kept.speaker == person.speaker) {
                        person.speaker = None;
                    }
                    person
                        .faces
                        .retain(|f| !people.iter().any(|kept| kept.faces.contains(f)));
                    let id = format!("p{next_person}");
                    next_person += 1;
                    rename.insert(person.id.clone(), id.clone());
                    people.push(Person { id, ..person });
                }
            }
        }
        out.people = (self.people.is_some() || new_people_given).then_some(people);
        let renamed = |text: &Option<String>| {
            text.as_deref()
                .map(|t| rewrite_mentions(&drop_unknown_mentions(t, &new_people), &rename))
        };

        // Ids kept from here must not be reused by the new run.
        let mut taken: HashSet<String> = self.ids();

        // Moments: a run that did not look for them leaves the old ones.
        match out.moments.take() {
            None => out.moments = self.moments.clone(),
            Some(new_moments) => {
                let user_moments: Vec<Marker> = self
                    .moments
                    .iter()
                    .flatten()
                    .filter(|m| m.by == By::User)
                    .cloned()
                    .collect();
                let mut moments = user_moments.clone();
                for mut moment in new_moments {
                    let snapped = user_moments.iter().any(|u| {
                        u.end_s.is_none() && (u.start_s - moment.start_s).abs() <= MOMENT_SNAP_S
                    });
                    if snapped {
                        continue;
                    }
                    moment.description = renamed(&moment.description);
                    fresh_id(&mut moment.id, &mut taken);
                    moments.push(moment);
                }
                sort_markers(&mut moments);
                out.moments = Some(moments);
            }
        }

        // Parts: a run that did not make them leaves the old ones (which cover the clip).
        match out.parts.take() {
            None => out.parts = self.parts.clone(),
            Some(new_parts) => {
                let user_parts: Vec<Marker> = self
                    .parts
                    .iter()
                    .flatten()
                    .filter(|p| p.by == By::User)
                    .cloned()
                    .collect();
                let mut all = user_parts.clone();
                for part in new_parts {
                    for (start, end) in subtract(part.start_s, part.end_or_start(), &user_parts) {
                        let mut piece = part.clone();
                        piece.start_s = start;
                        piece.end_s = Some(end);
                        piece.description = renamed(&part.description);
                        all.push(piece);
                    }
                }
                sort_markers(&mut all);
                let old_ai: Vec<&Marker> = self
                    .parts
                    .iter()
                    .flatten()
                    .filter(|p| p.by == By::Ai)
                    .collect();
                let all = cover(all, out.duration_s, &old_ai);
                let mut parts = absorb_short_parts(all);
                for part in parts.iter_mut().filter(|p| p.by == By::Ai) {
                    fresh_id(&mut part.id, &mut taken);
                }
                out.parts = Some(parts);
            }
        }

        // New text may name a person by an id the merge changed.
        out.summary = match &new_summary {
            Some(_) => renamed(&new_summary),
            None => self.summary.clone(),
        };
        for (key, value) in &self.frame.extra {
            out.frame
                .extra
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }

        // An id that is not a short one (a Premiere GUID from an older file) gets a short one.
        let mut ids = out.ids();
        for marker in out.parts.iter_mut().chain(out.moments.iter_mut()).flatten() {
            if !is_short_id(&marker.id) {
                fresh_id(&mut marker.id, &mut ids);
            }
        }
        out.fit_to_duration();

        // Text kept from the old file may name an AI person this run replaced: such mentions
        // become "someone", as any mention of no one does.
        let people = out.people.clone().unwrap_or_default();
        let fix = |text: &mut Option<String>| {
            if let Some(t) = text.as_mut() {
                *t = drop_unknown_mentions(t, &people);
            }
        };
        fix(&mut out.summary);
        for marker in out.parts.iter_mut().chain(out.moments.iter_mut()).flatten() {
            fix(&mut marker.description);
        }
        out
    }

    /// Bring everything kept from an older file inside `duration_s` (a new probe can measure a
    /// few frames less or more): times past the end are cut to it, items that start past it are
    /// dropped, and parts are made to cover the clip again.
    fn fit_to_duration(&mut self) {
        let d = self.duration_s;
        if let Some(io) = &mut self.in_out {
            io.start_s = io.start_s.filter(|t| *t < d);
            io.end_s = io.end_s.map(|t| t.min(d));
            if let (Some(a), Some(b)) = (io.start_s, io.end_s) {
                if b <= a {
                    io.end_s = None;
                }
            }
        }
        if let Some(cuts) = &mut self.cuts {
            cuts.retain(|t| *t < d);
        }
        if let Some(moments) = &mut self.moments {
            moments.retain(|m| m.start_s < d);
            for m in moments.iter_mut() {
                m.end_s = m.end_s.map(|e| e.min(d)).filter(|e| *e > m.start_s);
            }
        }
        if let Some(parts) = self.parts.take() {
            let mut parts: Vec<Marker> = parts.into_iter().filter(|p| p.start_s < d).collect();
            for p in &mut parts {
                p.end_s = Some(p.end_or_start().min(d));
            }
            let mut taken = self.ids();
            let mut parts = absorb_short_parts(cover(parts, d, &[]));
            for p in parts.iter_mut().filter(|p| p.id.is_empty()) {
                fresh_id(&mut p.id, &mut taken);
            }
            self.parts = Some(parts);
        }
        for tracks in [&mut self.faces, &mut self.texts].into_iter().flatten() {
            {
                tracks.retain(|t| t.start_s < d);
                for t in tracks.iter_mut() {
                    t.end_s = t.end_s.min(d);
                    let first = t.track.first().cloned();
                    t.track.retain(|s| s.t_s <= d);
                    if let (true, Some(mut first)) = (t.track.is_empty(), first) {
                        first.t_s = t.start_s;
                        t.track.push(first);
                    }
                }
            }
        }
        if let Some(faces) = &self.faces {
            let ids: HashSet<&str> = faces.iter().map(|f| f.id.as_str()).collect();
            for p in self.people.iter_mut().flatten() {
                p.faces.retain(|f| ids.contains(f.as_str()));
            }
        }
        for tag in self.tags.iter_mut().flatten() {
            if let Some(ranges) = &mut tag.ranges {
                ranges.retain(|r| r.start_s < d);
                for r in ranges.iter_mut() {
                    r.end_s = r.end_s.min(d);
                }
            }
        }
    }

    /// Every rule of the format this file breaks, one line each; empty when it is valid.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let d = self.duration_s;
        let mut bad = |p: String| problems.push(p);
        if self.version != FORMAT_VERSION {
            bad(format!("version is {}, not {FORMAT_VERSION}", self.version));
        }
        if !(d.is_finite() && d > 0.0) {
            bad(format!("duration_s {d} is not a length"));
        }
        if self.frame.width == 0
            || self.frame.height == 0
            || self.frame.fps.is_nan()
            || self.frame.fps <= 0.0
        {
            bad("frame needs a width, a height and an fps".to_string());
        }
        if self.lang.trim().is_empty() {
            bad("lang is empty".to_string());
        }
        let in_video = |t: f64| t.is_finite() && (0.0..=d).contains(&t);
        if let Some(u) = &self.usable {
            if !(in_video(u.start_s) && in_video(u.end_s) && u.end_s > u.start_s) {
                bad(format!(
                    "usable {}–{} is not a stretch of the video",
                    u.start_s, u.end_s
                ));
            }
        }
        if let Some(io) = &self.in_out {
            for t in io.start_s.iter().chain(&io.end_s) {
                if !in_video(*t) {
                    bad(format!("in_out time {t} is outside the video"));
                }
            }
            if let (Some(a), Some(b)) = (io.start_s, io.end_s) {
                if b <= a {
                    bad("in_out ends before it starts".to_string());
                }
            }
        }
        if let Some(cuts) = &self.cuts {
            if cuts.iter().any(|t| !in_video(*t)) {
                bad("a cut is outside the video".to_string());
            }
            if cuts.windows(2).any(|w| w[1] < w[0]) {
                bad("cuts are not sorted".to_string());
            }
        }

        // Ids unique in the file.
        let mut seen = HashSet::new();
        let all_ids = self
            .parts
            .iter()
            .chain(&self.moments)
            .flatten()
            .map(|m| &m.id)
            .chain(self.people.iter().flatten().map(|p| &p.id))
            .chain(
                self.faces
                    .iter()
                    .chain(&self.texts)
                    .flatten()
                    .map(|t| &t.id),
            );
        for id in all_ids {
            if id.is_empty() {
                bad("an empty id".to_string());
            } else if !seen.insert(id.clone()) {
                bad(format!("id {id} is used twice"));
            }
        }

        let people: HashSet<&str> = self
            .people
            .iter()
            .flatten()
            .map(|p| p.id.as_str())
            .collect();
        let check_marker = |m: &Marker, kind: &str, bad: &mut dyn FnMut(String)| {
            if !is_short_id(&m.id) {
                bad(format!(
                    "{kind} id {:?} is not 6 Crockford base32 characters",
                    m.id
                ));
            }
            if !in_video(m.start_s) {
                bad(format!("{kind} {} starts outside the video", m.id));
            }
            if let Some(end) = m.end_s {
                if !in_video(end) || end <= m.start_s {
                    bad(format!("{kind} {} does not end after it starts", m.id));
                }
            }
            if let Some(color) = &m.color {
                if !is_color(color) {
                    bad(format!("{kind} {} has an unknown colour {color:?}", m.id));
                }
            }
            if m.by == By::Ai {
                match &m.title {
                    None => bad(format!("{kind} {} (ai) has no title", m.id)),
                    Some(t) => {
                        let words = t.split_whitespace().count();
                        if words == 0
                            || words > MAX_TITLE_WORDS
                            || t.chars().count() > MAX_TITLE_CHARS
                            || t.trim_end().ends_with('.')
                            || t.contains('\n')
                            || !mentions(t).is_empty()
                        {
                            bad(format!(
                                "{kind} {} (ai) title {t:?} breaks the title rule",
                                m.id
                            ));
                        }
                    }
                }
            }
            for text in m.description.iter().chain(&m.title) {
                for mention in mentions(text) {
                    if !people.contains(mention.as_str()) {
                        bad(format!(
                            "{kind} {} names {mention}, who is not in people",
                            m.id
                        ));
                    }
                }
            }
        };
        if let Some(parts) = &self.parts {
            for p in parts {
                if p.end_s.is_none() {
                    bad(format!("part {} has no end", p.id));
                }
                check_marker(p, "part", &mut bad);
            }
            if !sorted(parts) {
                bad("parts are not sorted".to_string());
            }
            let mut at = 0.0;
            for p in parts {
                if (p.start_s - at).abs() > 1e-3 {
                    bad(format!("part {} leaves a gap or overlaps at {at}", p.id));
                }
                at = p.end_or_start();
            }
            if !parts.is_empty() && (at - d).abs() > 1e-3 {
                bad(format!("parts end at {at}, not at the video's end"));
            }
        }
        if let Some(moments) = &self.moments {
            for m in moments {
                check_marker(m, "moment", &mut bad);
            }
            if !sorted(moments) {
                bad("moments are not sorted".to_string());
            }
        }
        if let Some(summary) = &self.summary {
            for mention in mentions(summary) {
                if !people.contains(mention.as_str()) {
                    bad(format!("the summary names {mention}, who is not in people"));
                }
            }
        }

        let faces: HashSet<&str> = self.faces.iter().flatten().map(|f| f.id.as_str()).collect();
        let mut speakers = HashSet::new();
        for person in self.people.iter().flatten() {
            if let Some(speaker) = &person.speaker {
                if !speakers.insert(speaker.as_str()) {
                    bad(format!(
                        "two people are speaker {speaker}: they are one person"
                    ));
                }
            }
        }
        for person in self.people.iter().flatten() {
            if !is_numbered(&person.id, 'p') {
                bad(format!("person id {} is not p and a number", person.id));
            }
            for face in &person.faces {
                if !faces.contains(face.as_str()) {
                    bad(format!(
                        "person {} has face {face}, which is not in faces",
                        person.id
                    ));
                }
            }
        }
        for (list, prefix) in [(&self.faces, 'f'), (&self.texts, 'x')] {
            for t in list.iter().flatten() {
                check_track(t, prefix, d, &mut bad);
            }
            if let Some(list) = list {
                if list
                    .windows(2)
                    .any(|w| (w[1].start_s, w[1].end_s) < (w[0].start_s, w[0].end_s))
                {
                    bad(format!("{prefix} tracks are not sorted"));
                }
            }
        }
        for tag in self.tags.iter().flatten() {
            for r in tag.ranges.iter().flatten() {
                if !(in_video(r.start_s) && in_video(r.end_s) && r.end_s > r.start_s) {
                    bad(format!("tag {} has a range outside the video", tag.name));
                }
            }
        }
        problems
    }
}

/// Why [`save_run`] wrote nothing.
#[derive(Debug)]
pub enum SaveError {
    /// The video could not be read for its frame size or shots.
    Video(crate::Error),
    /// A file is already there that this version cannot read: it is left as it is rather than
    /// lose what a person put in it.
    Existing(ReadError),
    Io(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Video(e) => write!(f, "{e}"),
            Self::Existing(e) => write!(f, "the file there is kept: {e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// Write the description file of `video` from one described clip: the picture's size and frame
/// rate and the shots are read from the video (on this computer, nothing is sent), the rest is
/// `clip`. A file already there is merged with [`DescribeFile::merge_run`], so what a person set
/// stays. `fallback_lang` is the language when the answer did not say (see
/// [`crate::SummaryLanguage::code`]). Returns the path written.
#[cfg(feature = "frames")]
pub fn save_run(
    video: &Path,
    clip: &crate::DescribedClip,
    moments: MomentsMode,
    fallback_lang: &str,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<PathBuf, SaveError> {
    let path = describe_file_path(video);
    let existing = match DescribeFile::read(&path).map_err(SaveError::Io)? {
        None => None,
        Some(Ok(file)) => Some(file),
        Some(Err(e)) => return Err(SaveError::Existing(e)),
    };
    let (frame, _) = crate::frames::probe_frame(video).map_err(SaveError::Video)?;
    let shots = match crate::frames::detect_shots(video, cancel) {
        Ok(shots) => Some(shots),
        Err(crate::Error::Cancelled) => return Err(SaveError::Video(crate::Error::Cancelled)),
        Err(e) => {
            log::info!("clipscribe: no shots for {}: {e}", video.display());
            None
        }
    };
    let new = DescribeFile::from_described(
        &clip.description,
        RunFacts {
            duration_s: clip.duration_s,
            frame,
            fallback_lang,
            moments,
            shots: shots.as_ref(),
            tags: clip.tags.as_ref(),
            min_tag_confidence: DEFAULT_MIN_TAG_CONFIDENCE,
        },
    );
    let file = match existing {
        Some(old) => old.merge_run(new),
        None => new,
    };
    for problem in file.problems() {
        log::warn!("clipscribe: {}: {problem}", path.display());
    }
    file.write(&path).map_err(SaveError::Io)?;
    Ok(path)
}

/// `parts` (sorted) with every stretch of 0..`duration_s` they leave uncovered filled: by the
/// old AI parts there (cut to the gap), else by one AI part named after the part before it.
/// User parts are never moved.
fn cover(parts: Vec<Marker>, duration_s: f64, old_ai: &[&Marker]) -> Vec<Marker> {
    let mut covered = parts.clone();
    let mut gaps = Vec::new();
    let mut at = 0.0;
    for p in &parts {
        if p.start_s > at + 1e-6 {
            gaps.push((at, p.start_s));
        }
        at = f64::max(at, p.end_or_start());
    }
    if at < duration_s - 1e-6 {
        gaps.push((at, duration_s));
    }
    for (gap_start, gap_end) in gaps {
        let mut filled_to = gap_start;
        for old in old_ai {
            let (s, e) = (old.start_s.max(filled_to), old.end_or_start().min(gap_end));
            if e > s {
                if s > filled_to + 1e-6 {
                    covered.push(filler(filled_to, s, &covered));
                }
                let mut piece = (*old).clone();
                piece.start_s = s;
                piece.end_s = Some(e);
                covered.push(piece);
                filled_to = e;
            }
        }
        if filled_to < gap_end - 1e-6 {
            covered.push(filler(filled_to, gap_end, &covered));
        }
    }
    sort_markers(&mut covered);
    covered
}

/// An AI part over `start..end`, named like the part ending there (or "Part" when none).
fn filler(start: f64, end: f64, parts: &[Marker]) -> Marker {
    let before = parts
        .iter()
        .find(|p| (p.end_or_start() - start).abs() < 1e-6)
        .and_then(|p| p.title.clone())
        .map(|t| marker_title(&t, ""))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Part".to_string());
    Marker {
        id: String::new(),
        by: By::Ai,
        start_s: start,
        end_s: Some(end),
        title: Some(before),
        description: None,
        color: None,
        extra: Extra::new(),
    }
}

/// `value` without object members that are `null`, at every depth.
fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(drop_nulls);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_nulls),
        _ => {}
    }
}

/// `parts` (sorted, covering the clip) with each AI part shorter than [`MIN_PART_S`] given to
/// the AI part before it, or after it when there is none before; a user part is never moved,
/// so a short AI part between two user parts stays.
fn absorb_short_parts(parts: Vec<Marker>) -> Vec<Marker> {
    let short = |p: &Marker| p.by == By::Ai && p.end_or_start() - p.start_s < MIN_PART_S;
    let mut out: Vec<Marker> = Vec::new();
    let mut pending: Option<Marker> = None; // a short part waiting for an AI part after it
    for mut part in parts {
        if let Some(waiting) = pending.take() {
            if part.by == By::Ai {
                part.start_s = waiting.start_s;
            } else {
                out.push(waiting);
            }
        }
        if short(&part) {
            if let Some(prev) = out.last_mut().filter(|p| p.by == By::Ai) {
                prev.end_s = part.end_s;
                continue;
            }
            pending = Some(part);
            continue;
        }
        out.push(part);
    }
    out.extend(pending);
    out
}

fn check_track(t: &Track, prefix: char, d: f64, bad: &mut dyn FnMut(String)) {
    if !is_numbered(&t.id, prefix) {
        bad(format!("track id {} is not {prefix} and a number", t.id));
    }
    if !(t.start_s >= 0.0 && t.end_s <= d && t.end_s > t.start_s) {
        bad(format!("track {} is not a stretch of the video", t.id));
    }
    if t.track.is_empty() {
        bad(format!("track {} has no samples", t.id));
    }
    if t.track.windows(2).any(|w| w[1].t_s < w[0].t_s) {
        bad(format!("track {} samples are not in time order", t.id));
    }
    for s in &t.track {
        let [x, y, w, h] = s.bbox;
        let inside = [x, y, w, h].iter().all(|v| (0.0..=1.0).contains(v))
            && x + w <= 1.0 + 1e-9
            && y + h <= 1.0 + 1e-9;
        if !inside {
            bad(format!(
                "track {} has a box outside the frame at {}",
                t.id, s.t_s
            ));
        }
        if s.t_s < t.start_s || s.t_s > t.end_s {
            bad(format!(
                "track {} has a sample outside its time at {}",
                t.id, s.t_s
            ));
        }
    }
    if let Some(text) = &t.text {
        if text.chars().count() > MAX_TEXT_CHARS {
            bad(format!(
                "text {} is longer than {MAX_TEXT_CHARS} characters",
                t.id
            ));
        }
    }
}

/// Whether `id` is `prefix` and a number (`p3`).
fn is_numbered(id: &str, prefix: char) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The highest `N` of the `pN` ids among `people`; 0 for none.
fn max_person_number<'a>(people: impl Iterator<Item = &'a Person>) -> u64 {
    people
        .filter_map(|p| p.id.strip_prefix('p')?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
}

/// Whether a marker colour is one the format allows.
pub fn is_color(color: &str) -> bool {
    if PREMIERE_COLORS.contains(&color) {
        return true;
    }
    color
        .strip_prefix('#')
        .is_some_and(|hex| matches!(hex.len(), 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Whether `markers` are in the format's order: by `start_s`, then `end_s`.
fn sorted(markers: &[Marker]) -> bool {
    markers
        .windows(2)
        .all(|w| (w[0].start_s, w[0].end_or_start()) <= (w[1].start_s, w[1].end_or_start()))
}

fn sort_markers(markers: &mut [Marker]) {
    markers.sort_by(|a, b| {
        a.start_s
            .total_cmp(&b.start_s)
            .then(a.end_or_start().total_cmp(&b.end_or_start()))
    });
}

/// `start..end` without the stretches of `parts`, as the pieces left.
fn subtract(start: f64, end: f64, parts: &[Marker]) -> Vec<(f64, f64)> {
    let mut pieces = vec![(start, end)];
    for p in parts {
        let (a, b) = (p.start_s, p.end_or_start());
        pieces = pieces
            .into_iter()
            .flat_map(|(s, e)| {
                if b <= s || a >= e {
                    vec![(s, e)]
                } else {
                    [(s, a.max(s)), (b.min(e), e)]
                        .into_iter()
                        .filter(|(s, e)| e > s)
                        .collect()
                }
            })
            .collect();
    }
    pieces
}

/// Whether two face tracks are the same face: they overlap in time and, at a moment both are on
/// screen, their boxes overlap by at least a third (intersection over union).
fn same_place(a: &Track, b: &Track) -> bool {
    let from = a.start_s.max(b.start_s);
    let to = a.end_s.min(b.end_s);
    if to < from {
        return false;
    }
    let times: Vec<f64> = a
        .track
        .iter()
        .chain(&b.track)
        .map(|s| s.t_s.clamp(from, to))
        .chain([from, to, (from + to) / 2.0])
        .collect();
    times
        .into_iter()
        .any(|t| iou(box_at(a, t), box_at(b, t)) >= 1.0 / 3.0)
}

/// Where a track's box is at `t`: in a straight line between samples, held before the first and
/// after the last.
pub fn box_at(track: &Track, t: f64) -> [f64; 4] {
    let samples = &track.track;
    let Some(first) = samples.first() else {
        return [0.0; 4];
    };
    if t <= first.t_s {
        return first.bbox;
    }
    for w in samples.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if t <= b.t_s {
            let span = b.t_s - a.t_s;
            let k = if span > 0.0 { (t - a.t_s) / span } else { 1.0 };
            let mut out = a.bbox;
            for (o, (x, y)) in out.iter_mut().zip(a.bbox.iter().zip(&b.bbox)) {
                *o = x + (y - x) * k;
            }
            return out;
        }
    }
    samples.last().map_or([0.0; 4], |s| s.bbox)
}

fn iou(a: [f64; 4], b: [f64; 4]) -> f64 {
    let x = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
    let y = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
    if x <= 0.0 || y <= 0.0 {
        return 0.0;
    }
    let inter = x * y;
    let union = a[2] * a[3] + b[2] * b[3] - inter;
    if union > 0.0 {
        inter / union
    } else {
        0.0
    }
}

/// Seconds rounded to the millisecond, so files do not carry float noise.
fn round_ms(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

/// Crockford base32 without `i l o u`, the characters of a new id.
const ID_ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// A new 6-character id (Crockford base32, like CAPT block ids) not in `taken`.
pub fn new_id(taken: &HashSet<String>) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            hasher.write_u128(now.as_nanos());
        }
        let mut bits = hasher.finish();
        let id: String = (0..6)
            .map(|_| {
                let c = ID_ALPHABET[(bits % 32) as usize] as char;
                bits /= 32;
                c
            })
            .collect();
        if !taken.contains(&id) {
            return id;
        }
    }
}

/// Whether `id` is a marker id of the format: 6 characters of [`ID_ALPHABET`].
pub fn is_short_id(id: &str) -> bool {
    id.len() == 6 && id.bytes().all(|c| ID_ALPHABET.contains(&c))
}

/// Keep `id` if it is a free short id, else give it a new one; either way it is taken afterwards.
fn fresh_id(id: &mut String, taken: &mut HashSet<String>) {
    if !is_short_id(id) || taken.contains(id) {
        *id = new_id(taken);
    }
    taken.insert(id.clone());
}

/// The `@pN` mentions in `text`, in order (an escaped `\@` is not one).
pub fn mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    scan_mentions(text, |m| {
        out.push(m.to_string());
        None
    });
    out
}

/// `text` with each `@pN` replaced by what `replace` gives for `pN` (`None` keeps it as is).
/// `\@` stays as written.
fn scan_mentions(text: &str, mut replace: impl FnMut(&str) -> Option<String>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'@') {
            out.push_str("\\@");
            i += 2;
            continue;
        }
        if chars[i] == '@' && chars.get(i + 1) == Some(&'p') {
            let mut j = i + 2;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let ends_word = chars
                .get(j)
                .is_none_or(|c| !c.is_alphanumeric() && *c != '_');
            if j > i + 2 && ends_word {
                let id: String = chars[i + 1..j].iter().collect();
                match replace(&id) {
                    Some(with) => out.push_str(&with),
                    None => out.extend(&chars[i..j]),
                }
                i = j;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `text` with the person ids in `rename` changed (`@p4` → `@p1`).
fn rewrite_mentions(text: &str, rename: &BTreeMap<String, String>) -> String {
    scan_mentions(text, |id| rename.get(id).map(|to| format!("@{to}")))
}

/// `text` without the `@pN` that name no one in `people` (each becomes "someone").
pub fn drop_unknown_mentions(text: &str, people: &[Person]) -> String {
    scan_mentions(text, |id| {
        (!people.iter().any(|p| p.id == id)).then(|| "someone".to_string())
    })
}

/// `text` as a person reads it: each `@pN` is the name of that person's speaker in the CAPT file
/// (`speaker_name`, given the speaker id), else the person's `label`, else "someone"; `\@` is a
/// plain `@`.
pub fn render_mentions(
    text: &str,
    people: &[Person],
    speaker_name: impl Fn(&str) -> Option<String>,
) -> String {
    let shown = scan_mentions(text, |id| {
        let person = people.iter().find(|p| p.id == id);
        Some(
            person
                .and_then(|p| p.speaker.as_deref().and_then(&speaker_name))
                .or_else(|| person.and_then(|p| p.label.clone()))
                .unwrap_or_else(|| "someone".to_string()),
        )
    });
    shown.replace("\\@", "@")
}

#[cfg(test)]
mod tests;
