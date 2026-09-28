//! Grouping similar footage across clips, and stretches within clips, from two signals each
//! described clip already carries: the fingerprints of the frames it was described from, and the
//! words of its description. See [`group_clips`] and the design notes in
//! `docs/design/whole-folders.md`.
//!
//! No GStreamer or image dependency: a [`DescribedClip`] carries its frames' fingerprints (8×8
//! grids of average luma, the block-mean-value hash key frames already use) and its description,
//! so grouping runs without the `frames` feature, on clips described earlier and read back from
//! the cache.

use std::collections::{BTreeSet, HashMap};

use crate::describe::fingerprint_diff;
use crate::folder::{DescribedClip, FrameFingerprint};

/// A grid whose cells spread less than this (standard deviation, 0–255 scale) has no structure to
/// compare — black, a flat wall, fine noise — and never matches anything.
const BLANK_SPREAD: f64 = 4.0;
/// Two consecutive frames are a cut when their structure differs by more than this (`1 − r`)...
const CUT_STRUCTURE: f64 = 0.5;
/// ...and their level of light by more than this (mean absolute difference, 0.0–1.0).
const CUT_LEVEL: f64 = 0.05;
/// Two stretches whose pictures are closer than this (`1 − r`, over the four rotations) are the
/// same shot: a duplicate, a re-export, the clip stored sideways, a camera that did not move.
pub(crate) const SAME_SHOT: f64 = 0.2;
/// Two stretches of different clips whose descriptions share at least this fraction of their
/// words (Jaccard index of [`words`]) are described as the same thing: the same subject or
/// activity, even filmed from a moved or zoomed camera.
pub(crate) const SAME_TEXT: f64 = 0.25;
/// A stretch described in fewer [`words`] than this has too little text to compare.
const MIN_WORDS: usize = 3;
/// How many letters of a word [`words`] keeps: a crude, language-blind stemmer ("goat" and
/// "goats", "hiker" and "hikes", "красной" and "красная" come out the same).
const STEM_LETTERS: usize = 4;
/// A description segment names a stretch when it covers at least this fraction of it.
const LABEL_COVERAGE: f64 = 0.5;

/// Words that say nothing about what a clip shows: English function words of four letters or more
/// (shorter words are dropped in every language), and words about the footage itself.
const STOP_WORDS: [&str; 52] = [
    "about", "above", "across", "after", "against", "along", "also", "among", "around", "before",
    "behind", "being", "below", "beneath", "beside", "between", "both", "during", "each", "from",
    "have", "into", "just", "near", "onto", "other", "over", "some", "that", "their", "them",
    "then", "there", "these", "they", "this", "through", "toward", "towards", "under", "very",
    "where", "which", "while", "with", "camera", "clip", "footage", "frame", "scene", "shot",
    "video",
];

/// Similar footage: groups, and which group each clip, stretch and segment belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Grouping {
    /// Every group, ordered by id.
    pub groups: Vec<Group>,
    /// One per clip given to [`group_clips`], in the same order: `clips[n]` is the `n`th clip of
    /// the slice passed in. When that slice was filtered (say, only the described clips of a
    /// [`crate::FolderRun`]), keep the original index of each clip next to it to find its video.
    pub clips: Vec<ClipGroups>,
}

impl Grouping {
    /// The group with this id.
    pub fn group(&self, id: usize) -> Option<&Group> {
        self.groups.get(id.checked_sub(1)?)
    }
}

/// Footage that looks like the same shot (a duplicate, a re-export, the clip stored sideways, a
/// camera that did not move), or whose descriptions say much the same thing (the same subject or
/// activity, the camera moved or not); see [`group_clips`] for what that does and does not catch.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// From 1, in order of first appearance (clips in the order given, stretches in time order).
    pub id: usize,
    /// What the group shows: the description of its most typical stretch (the segment covering
    /// it, else its clip's summary).
    pub label: String,
    /// How many stretches, over all clips, belong to it.
    pub stretches: usize,
}

/// Where one clip's footage belongs.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipGroups {
    /// The group covering most of the clip.
    pub group: usize,
    /// The clip cut where the picture changes to something else, covering it from 0 to its
    /// duration.
    pub stretches: Vec<Stretch>,
    /// The group of each of the description's segments, in order: the group of the stretch
    /// overlapping it most.
    pub segments: Vec<usize>,
}

/// A stretch of a clip between two cuts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stretch {
    /// Where it starts, in seconds from the start of the clip.
    pub start_s: f64,
    /// Where it ends, in seconds from the start of the clip.
    pub end_s: f64,
    /// The [`Group::id`] it belongs to.
    pub group: usize,
}

/// Group `clips`, and stretches within them: each clip is cut into stretches where the picture
/// changes to something else, and two stretches share a group when either
///
/// - their pictures are the same shot: the same average layout of light and dark, whatever the
///   exposure and in any of the four 90° rotations — a duplicate, a re-export, a clip stored
///   sideways, a camera that did not move (any two stretches, in the same clip or not); or
/// - they are in different clips and their descriptions share at least a quarter of their words
///   (the segments falling in the stretch, and the clip's summary when the stretch is the whole
///   clip or no segment falls in it; common words dropped, each word cut to its first four
///   letters) — the same subject or activity, even after the camera moved or zoomed.
///
/// Groups are what those links connect. A stretch whose frames are all blank (black, a flat wall)
/// is a group of its own. The word signal only sees what the descriptions say: two clips of the
/// same place doing different things can share enough words to be joined, and two of the same
/// thing described in different words are not. A group is labelled with the description of its
/// most typical stretch; no request is made. Deterministic: the same clips always give the same
/// groups and ids.
pub fn group_clips(clips: &[&DescribedClip]) -> Grouping {
    struct Piece {
        clip: usize,
        start_s: f64,
        end_s: f64,
        signature: Option<Vec<f64>>,
        /// The stretch's words, as ids into `vocabulary`, sorted; empty when too few to compare.
        words: Vec<usize>,
    }
    let mut vocabulary: HashMap<String, usize> = HashMap::new();
    let mut pieces: Vec<Piece> = Vec::new();
    for (clip, described) in clips.iter().enumerate() {
        let stretches = stretches_of(described);
        let texts = stretch_texts(described, &stretches);
        for ((start_s, end_s, signature), text) in stretches.into_iter().zip(texts) {
            let words: BTreeSet<usize> = text
                .into_iter()
                .map(|word| {
                    let next = vocabulary.len();
                    *vocabulary.entry(word).or_insert(next)
                })
                .collect();
            let words = if words.len() >= MIN_WORDS {
                words.into_iter().collect()
            } else {
                Vec::new()
            };
            pieces.push(Piece {
                clip,
                start_s,
                end_s,
                signature,
                words,
            });
        }
    }
    // How far apart two stretches are, 0.0 up, below 1.0 when they belong together: each signal's
    // distance over its threshold, the closer of the two. `None` when one of them is blank.
    let link = |a: &Piece, b: &Piece| -> Option<f64> {
        let (Some(a_sig), Some(b_sig)) = (&a.signature, &b.signature) else {
            return None;
        };
        let picture = scene_distance(a_sig, b_sig) / SAME_SHOT;
        let text = if a.clip == b.clip || a.words.is_empty() || b.words.is_empty() {
            f64::INFINITY
        } else {
            // At the threshold exactly this is 1.0, which still joins: see `joined`.
            (1.0 - jaccard(&a.words, &b.words)) / (1.0 - SAME_TEXT)
        };
        Some(picture.min(text))
    };
    let joined = |a: &Piece, b: &Piece| -> bool {
        let Some((a_sig, b_sig)) = a.signature.as_ref().zip(b.signature.as_ref()) else {
            return false;
        };
        scene_distance(a_sig, b_sig) < SAME_SHOT
            || (a.clip != b.clip
                && !a.words.is_empty()
                && !b.words.is_empty()
                && jaccard(&a.words, &b.words) >= SAME_TEXT)
    };

    let mut sets = DisjointSets::new(pieces.len());
    for (i, a) in pieces.iter().enumerate() {
        for (j, b) in pieces.iter().enumerate().skip(i + 1) {
            if joined(a, b) {
                sets.union(i, j);
            }
        }
    }

    // Ids in order of first appearance; members of each group in piece order.
    let mut id_of_root = HashMap::new();
    let mut members: Vec<Vec<usize>> = Vec::new();
    let group_of: Vec<usize> = (0..pieces.len())
        .map(|i| {
            let root = sets.find(i);
            let id = *id_of_root.entry(root).or_insert_with(|| {
                members.push(Vec::new());
                members.len()
            });
            members[id - 1].push(i);
            id
        })
        .collect();

    let groups = members
        .iter()
        .enumerate()
        .map(|(index, members)| {
            // The medoid: smallest total distance to the other members, the first on a tie.
            let medoid = members
                .iter()
                .copied()
                .map(|i| {
                    let total: f64 = members
                        .iter()
                        .filter(|&&j| j != i)
                        .map(|&j| link(&pieces[i], &pieces[j]).unwrap_or(0.0))
                        .sum();
                    (i, total)
                })
                .fold(None, |best: Option<(usize, f64)>, (i, total)| match best {
                    Some((_, best_total)) if best_total <= total => best,
                    _ => Some((i, total)),
                })
                .map_or(members[0], |(i, _)| i);
            let piece = &pieces[medoid];
            Group {
                id: index + 1,
                label: label_for(clips[piece.clip], piece.start_s, piece.end_s),
                stretches: members.len(),
            }
        })
        .collect();

    let clips = clips
        .iter()
        .enumerate()
        .map(|(clip, described)| {
            let stretches: Vec<Stretch> = pieces
                .iter()
                .zip(&group_of)
                .filter(|(piece, _)| piece.clip == clip)
                .map(|(piece, &group)| Stretch {
                    start_s: piece.start_s,
                    end_s: piece.end_s,
                    group,
                })
                .collect();
            let group = main_group(&stretches);
            let segments = described
                .description
                .segments
                .iter()
                .map(|segment| {
                    most_overlapping(
                        stretches.iter().map(|s| (s.start_s, s.end_s)),
                        segment.start_s,
                        segment.end_s,
                    )
                    .map_or(group, |i| stretches[i].group)
                })
                .collect();
            ClipGroups {
                group,
                stretches,
                segments,
            }
        })
        .collect();

    Grouping { groups, clips }
}

/// The index of the span of `spans` overlapping `start_s..end_s` most (the first on a tie), if any
/// overlaps it at all.
fn most_overlapping(
    spans: impl Iterator<Item = (f64, f64)>,
    start_s: f64,
    end_s: f64,
) -> Option<usize> {
    spans
        .enumerate()
        .map(|(i, (a, b))| (i, overlap(a, b, start_s, end_s)))
        .filter(|(_, o)| *o > 0.0)
        .fold(None, |best: Option<(usize, f64)>, (i, o)| match best {
            Some((_, best_o)) if best_o >= o => best,
            _ => Some((i, o)),
        })
        .map(|(i, _)| i)
}

/// The words each of `stretches` of `clip` is described in (see [`words`]): the description
/// segments overlapping it more than any other stretch, and the clip's summary when the stretch is
/// the whole clip or no segment falls in it.
fn stretch_texts(
    clip: &DescribedClip,
    stretches: &[(f64, f64, Option<Vec<f64>>)],
) -> Vec<Vec<String>> {
    let mut texts: Vec<Vec<String>> = vec![Vec::new(); stretches.len()];
    let mut has_segment = vec![false; stretches.len()];
    for segment in &clip.description.segments {
        let spans = stretches.iter().map(|(a, b, _)| (*a, *b));
        if let Some(i) = most_overlapping(spans, segment.start_s, segment.end_s) {
            texts[i].extend(words(&segment.description));
            has_segment[i] = true;
        }
    }
    for (i, text) in texts.iter_mut().enumerate() {
        if stretches.len() == 1 || !has_segment[i] {
            text.extend(words(&clip.description.summary));
        }
    }
    texts
}

/// The words of `text` that say what it shows, for comparing descriptions: lower-cased, split at
/// anything that is not a letter or a digit, words shorter than four letters and [`STOP_WORDS`]
/// dropped, each cut to its first [`STEM_LETTERS`] letters. Works the same in every language the
/// descriptions come in, except that only English has stop words beyond the length rule.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 4 && !STOP_WORDS.contains(word))
        .map(|word| word.chars().take(STEM_LETTERS).collect())
        .collect()
}

/// The Jaccard index of two sorted sets of word ids: shared over all, 0.0–1.0.
fn jaccard(a: &[usize], b: &[usize]) -> f64 {
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let all = a.len() + b.len() - shared;
    if all == 0 {
        0.0
    } else {
        shared as f64 / all as f64
    }
}

/// The Jaccard index of the [`words`] of two texts (0.0 when either has fewer than
/// [`MIN_WORDS`]): what [`group_clips`] compares descriptions by. For measuring the threshold.
#[cfg(test)]
pub(crate) fn text_similarity(a: &str, b: &str) -> f64 {
    let mut vocabulary: HashMap<String, usize> = HashMap::new();
    let mut ids = |text: &str| -> Vec<usize> {
        let set: BTreeSet<usize> = words(text)
            .into_iter()
            .map(|word| {
                let next = vocabulary.len();
                *vocabulary.entry(word).or_insert(next)
            })
            .collect();
        set.into_iter().collect()
    };
    let (a, b) = (ids(a), ids(b));
    if a.len() < MIN_WORDS || b.len() < MIN_WORDS {
        return 0.0;
    }
    jaccard(&a, &b)
}

/// The distance [`group_clips`] would join `a` and `b` by: the smallest [`scene_distance`] between
/// a stretch of one and a stretch of the other (`None` when either has no stretch with a
/// signature). For measuring the thresholds on real clips.
#[cfg(test)]
pub(crate) fn clip_distance(a: &DescribedClip, b: &DescribedClip) -> Option<f64> {
    let signatures = |clip: &DescribedClip| -> Vec<Vec<f64>> {
        stretches_of(clip)
            .into_iter()
            .filter_map(|(_, _, signature)| signature)
            .collect()
    };
    let (a, b) = (signatures(a), signatures(b));
    a.iter()
        .flat_map(|x| b.iter().map(move |y| scene_distance(x, y)))
        .min_by(f64::total_cmp)
}

/// The group covering most of a clip's stretches' time; the lowest id on a tie.
fn main_group(stretches: &[Stretch]) -> usize {
    let mut time: Vec<(usize, f64)> = Vec::new();
    for s in stretches {
        let length = (s.end_s - s.start_s).max(0.0);
        match time.iter_mut().find(|(g, _)| *g == s.group) {
            Some((_, t)) => *t += length,
            None => time.push((s.group, length)),
        }
    }
    time.sort_by_key(|(g, _)| *g);
    time.iter()
        .fold(None, |best: Option<(usize, f64)>, &(g, t)| match best {
            Some((_, best_t)) if best_t >= t => best,
            _ => Some((g, t)),
        })
        .map_or(1, |(g, _)| g)
}

/// How long `[a_start, a_end]` and `[b_start, b_end]` overlap, in seconds.
fn overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

/// The words naming the stretch `start_s..end_s` of `clip`: the description segment covering at
/// least half of it (the one covering most), else the clip's summary.
fn label_for(clip: &DescribedClip, start_s: f64, end_s: f64) -> String {
    let length = (end_s - start_s).max(f64::EPSILON);
    clip.description
        .segments
        .iter()
        .map(|s| (s, overlap(start_s, end_s, s.start_s, s.end_s)))
        .filter(|(_, o)| *o >= LABEL_COVERAGE * length)
        .fold(
            None,
            |best: Option<(&crate::Segment, f64)>, (s, o)| match best {
                Some((_, best_o)) if best_o >= o => best,
                _ => Some((s, o)),
            },
        )
        .map_or_else(
            || clip.description.summary.clone(),
            |(s, _)| s.description.clone(),
        )
}

/// `clip` cut into stretches (start, end, signature): between two consecutive frames where both
/// the structure and the level of light change (see [`is_cut`]), halfway between them. A stretch
/// whose frames are all blank has no signature.
fn stretches_of(clip: &DescribedClip) -> Vec<(f64, f64, Option<Vec<f64>>)> {
    let mut frames: Vec<&FrameFingerprint> = clip.frames.iter().collect();
    frames.sort_by(|a, b| a.time_s.total_cmp(&b.time_s));
    let end = clip.duration_s.max(0.0);
    if frames.is_empty() {
        return vec![(0.0, end, None)];
    }
    let mut stretches = Vec::new();
    let mut first = 0;
    let mut start_s = 0.0;
    for i in 1..=frames.len() {
        let cut = i == frames.len() || is_cut(frames[i - 1], frames[i]);
        if !cut {
            continue;
        }
        let end_s = if i == frames.len() {
            end
        } else {
            ((frames[i - 1].time_s + frames[i].time_s) / 2.0).clamp(start_s, end)
        };
        stretches.push((start_s, end_s, signature(&frames[first..i])));
        first = i;
        start_s = end_s;
    }
    stretches
}

/// Whether the picture changes to something else between two consecutive frames: the structure
/// changes (or one side has none) *and* the level of light does, so that camera motion or a subject
/// moving over the same background stays one stretch.
fn is_cut(a: &FrameFingerprint, b: &FrameFingerprint) -> bool {
    let structure = match (normalise(&a.fingerprint), normalise(&b.fingerprint)) {
        (Some(a), Some(b)) => 1.0 - correlation(&a, &b) > CUT_STRUCTURE,
        _ => true,
    };
    structure && fingerprint_diff(&a.fingerprint, &b.fingerprint) > CUT_LEVEL
}

/// The average normalised grid of `frames`' non-blank ones, normalised again; `None` when every
/// frame is blank.
fn signature(frames: &[&FrameFingerprint]) -> Option<Vec<f64>> {
    let grids: Vec<Vec<f64>> = frames
        .iter()
        .filter_map(|f| normalise(&f.fingerprint))
        .collect();
    let len = grids.first()?.len();
    let mut mean = vec![0.0; len];
    let mut count = 0.0;
    for grid in grids.iter().filter(|g| g.len() == len) {
        for (m, v) in mean.iter_mut().zip(grid) {
            *m += v;
        }
        count += 1.0;
    }
    mean.iter_mut().for_each(|m| *m /= count);
    standardise(&mean, 1e-6)
}

/// `fingerprint` with its mean subtracted and divided by its spread; `None` when it is blank
/// (spread under [`BLANK_SPREAD`]) or empty.
fn normalise(fingerprint: &[u8]) -> Option<Vec<f64>> {
    let values: Vec<f64> = fingerprint.iter().map(|&v| f64::from(v)).collect();
    standardise(&values, BLANK_SPREAD)
}

/// `values` with zero mean and unit (population) standard deviation; `None` when the standard
/// deviation is below `min_spread`.
fn standardise(values: &[f64], min_spread: f64) -> Option<Vec<f64>> {
    if values.is_empty() {
        return None;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let spread = (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n).sqrt();
    (spread >= min_spread).then(|| values.iter().map(|v| (v - mean) / spread).collect())
}

/// Pearson correlation of two standardised vectors of the same length (−1.0 to 1.0); `0.0` when
/// the lengths differ.
fn correlation(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>() / a.len() as f64
}

/// `1 − r` between two standardised grids, the smallest over the four 90° rotations of `b` (when
/// the grid is square): 0.0 for the same layout of light and dark, up to 2.0 for its negative.
fn scene_distance(a: &[f64], b: &[f64]) -> f64 {
    let mut best = 1.0 - correlation(a, b);
    if let Some(side) = square_side(b.len()) {
        let mut turned = b.to_vec();
        for _ in 0..3 {
            turned = rotate(&turned, side);
            best = best.min(1.0 - correlation(a, &turned));
        }
    }
    best
}

/// The side of a square grid of `len` cells, if it is one.
fn square_side(len: usize) -> Option<usize> {
    let side = (len as f64).sqrt().round() as usize;
    (side > 1 && side * side == len).then_some(side)
}

/// A `side`×`side` grid (rows packed) turned 90° clockwise.
fn rotate(grid: &[f64], side: usize) -> Vec<f64> {
    (0..side * side)
        .map(|i| {
            let (y, x) = (i / side, i % side);
            grid[(side - 1 - x) * side + y]
        })
        .collect()
}

/// Union-find over `0..n`, for the connected sets of stretches.
struct DisjointSets {
    parent: Vec<usize>,
}

impl DisjointSets {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, i: usize) -> usize {
        let mut root = i;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        let mut i = i;
        while self.parent[i] != root {
            let next = self.parent[i];
            self.parent[i] = root;
            i = next;
        }
        root
    }

    /// Joins the sets of `a` and `b`, the smaller root becoming the root, so roots stay stable.
    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        let (low, high) = (a.min(b), a.max(b));
        self.parent[high] = low;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AiUsage, Description, Segment};

    /// An 8×8 grid with a bright block at `(x, y)` (3×3 cells) on a background of `level`.
    fn scene(x: usize, y: usize, level: u8) -> Vec<u8> {
        (0..64)
            .map(|i| {
                let (cy, cx) = (i / 8, i % 8);
                if (x..x + 3).contains(&cx) && (y..y + 3).contains(&cy) {
                    level.saturating_add(150)
                } else {
                    level
                }
            })
            .collect()
    }

    /// A left-to-right ramp: another scene, unlike any block.
    fn ramp() -> Vec<u8> {
        (0..64).map(|i| ((i % 8) * 30) as u8).collect()
    }

    fn turned(grid: &[u8]) -> Vec<u8> {
        let values: Vec<f64> = grid.iter().map(|&v| f64::from(v)).collect();
        rotate(&values, 8).into_iter().map(|v| v as u8).collect()
    }

    fn clip(summary: &str, duration_s: f64, frames: Vec<(f64, Vec<u8>)>) -> DescribedClip {
        DescribedClip {
            description: Description {
                summary: summary.to_string(),
                segments: Vec::new(),
            },
            tags: None,
            usage: AiUsage::default(),
            duration_s,
            frames: frames
                .into_iter()
                .map(|(time_s, fingerprint)| FrameFingerprint {
                    time_s,
                    fingerprint,
                })
                .collect(),
        }
    }

    #[test]
    fn the_same_shot_groups_across_clips_whatever_the_exposure_and_rotation() {
        let a = clip(
            "A lamp on a desk.",
            4.0,
            vec![(0.0, scene(1, 1, 20)), (2.0, scene(1, 1, 22))],
        );
        // The same set-up filmed brighter, then stored sideways.
        let brighter = clip("The same lamp.", 4.0, vec![(0.0, scene(1, 1, 90))]);
        let sideways = clip(
            "The lamp, sideways.",
            4.0,
            vec![(0.0, turned(&scene(1, 1, 20)))],
        );
        let other = clip("A gradient.", 4.0, vec![(0.0, ramp())]);
        let grouping = group_clips(&[&a, &brighter, &sideways, &other]);
        let ids: Vec<usize> = grouping.clips.iter().map(|c| c.group).collect();
        assert_eq!(ids, vec![1, 1, 1, 2]);
        assert_eq!(grouping.groups.len(), 2);
        assert_eq!(grouping.groups[0].stretches, 3);
        assert_eq!(
            grouping.group(2).map(|g| g.label.as_str()),
            Some("A gradient.")
        );
    }

    #[test]
    fn a_blank_clip_is_a_group_of_its_own() {
        let black = clip("Black.", 2.0, vec![(0.0, vec![0; 64])]);
        let also_black = clip("Also black.", 2.0, vec![(0.0, vec![1; 64])]);
        let grouping = group_clips(&[&black, &also_black]);
        assert_eq!(grouping.clips[0].group, 1);
        assert_eq!(
            grouping.clips[1].group, 2,
            "nothing to compare: never matched"
        );
    }

    #[test]
    fn a_cut_splits_a_clip_and_each_stretch_finds_its_own_group() {
        // 0–6 s the lamp, then a cut to the gradient; another clip shows only the gradient.
        let cut = DescribedClip {
            description: Description {
                summary: "A lamp, then a gradient.".to_string(),
                segments: vec![
                    Segment {
                        start_s: 0.5,
                        end_s: 5.0,
                        description: "The lamp is switched on.".to_string(),
                    },
                    Segment {
                        start_s: 7.0,
                        end_s: 9.5,
                        description: "A gradient fills the screen.".to_string(),
                    },
                ],
            },
            ..clip(
                "",
                10.0,
                vec![
                    (0.0, scene(1, 1, 20)),
                    (2.0, scene(1, 1, 20)),
                    (4.0, scene(1, 1, 22)),
                    (8.0, ramp()),
                ],
            )
        };
        let gradient = clip("A gradient.", 5.0, vec![(1.0, ramp()), (3.0, ramp())]);
        let grouping = group_clips(&[&cut, &gradient]);
        let stretches = &grouping.clips[0].stretches;
        assert_eq!(stretches.len(), 2, "{stretches:?}");
        assert_eq!((stretches[0].start_s, stretches[0].end_s), (0.0, 6.0));
        assert_eq!((stretches[1].start_s, stretches[1].end_s), (6.0, 10.0));
        assert_eq!(stretches[0].group, 1);
        assert_eq!(stretches[1].group, 2);
        assert_eq!(
            grouping.clips[1].group, 2,
            "the gradient clip joins the cut's second part"
        );
        assert_eq!(grouping.clips[0].segments, vec![1, 2]);
        assert_eq!(
            grouping.clips[0].group, 1,
            "the lamp covers more of the clip"
        );
        assert_eq!(grouping.groups[0].label, "The lamp is switched on.");
    }

    #[test]
    fn a_small_change_is_not_a_cut() {
        // The block moves by one cell: most of the layout of light and dark stays where it was.
        let moving = clip(
            "A lamp being moved.",
            4.0,
            vec![(0.0, scene(1, 1, 20)), (2.0, scene(2, 1, 20))],
        );
        assert_eq!(group_clips(&[&moving]).clips[0].stretches.len(), 1);
    }

    #[test]
    fn ids_follow_first_appearance_and_are_stable() {
        let a = clip("Ramp.", 2.0, vec![(0.0, ramp())]);
        let b = clip("Lamp.", 2.0, vec![(0.0, scene(1, 1, 20))]);
        let c = clip("Ramp again.", 2.0, vec![(0.0, ramp())]);
        let first = group_clips(&[&a, &b, &c]);
        assert_eq!(
            first.clips.iter().map(|c| c.group).collect::<Vec<_>>(),
            vec![1, 2, 1]
        );
        assert_eq!(first, group_clips(&[&a, &b, &c]), "deterministic");
    }

    #[test]
    fn a_clip_without_frames_still_gets_a_group() {
        let empty = clip("Nothing.", 3.0, vec![]);
        let grouping = group_clips(&[&empty]);
        assert_eq!(grouping.clips[0].group, 1);
        assert_eq!(grouping.clips[0].stretches.len(), 1);
        assert_eq!(grouping.groups[0].label, "Nothing.");
    }

    /// Descriptions in the style the model writes them (a one-sentence summary, a moment or
    /// two), for measuring the word signal: written for this test, not model output (no live
    /// requests are made in the tests). Letters name a subject; `a`/`b` are the same subject or
    /// activity filmed from another position or zoom, as a re-shoot would be.
    const DESCRIPTIONS: [(&str, &str); 19] = [
        ("hike a", "A hiker in a red jacket walks along a rocky ridge at sunset, with mountains in the distance."),
        ("hike b", "Close view of a hiker in a red jacket crossing a rocky ridge as the sun goes down."),
        ("tent a", "Two people pitch a green tent in a grassy clearing beside a lake. They spread the tent out on the grass. The poles go in and the tent stands up."),
        ("tent b", "From the lake shore, two campers finish setting up a green tent in the clearing."),
        ("goats a", "A herd of goats grazes on a steep hillside above the trail."),
        ("goats b", "The camera pans across goats grazing on the hillside."),
        ("onion a", "A woman chops onions on a wooden cutting board in a small kitchen."),
        ("onion b", "Close-up of a knife slicing an onion on a cutting board."),
        ("waves a", "Waves break on a sandy beach under a cloudy sky."),
        ("waves b", "Seen from the dunes, grey waves roll onto the beach beneath heavy clouds."),
        ("хребет a", "Турист в красной куртке идёт по скалистому хребту на закате."),
        ("хребет b", "Турист в красной куртке пересекает скалистый хребет, солнце садится."),
        // The same shoot, something else: the same hiker, the same place, the same beach.
        ("drink", "A hiker in a red jacket drinks water from a stream."),
        ("bike", "A man in a red jacket rides a mountain bike along a rocky ridge at sunset."),
        ("stove", "Two people cook dinner on a camp stove beside the green tent."),
        ("lake", "The sun sets over the lake; the water is calm."),
        ("dog", "A dog runs along the sandy beach chasing a ball."),
        ("street", "A car drives down a city street at night, its headlights on."),
        ("football", "Children play football in a school yard."),
    ];

    /// Pairs of [`DESCRIPTIONS`] that show the same place with something else happening: what
    /// the issue's "the same scene or activity" may or may not mean. Printed, not asserted.
    const SAME_PLACE: [(&str, &str); 5] = [
        ("hike a", "bike"),
        ("hike b", "bike"),
        ("tent a", "stove"),
        ("tent b", "stove"),
        ("waves a", "dog"),
    ];

    /// The measurement behind [`SAME_TEXT`] (`docs/design/whole-folders.md`, "Measured"): the
    /// word similarity of every pair of [`DESCRIPTIONS`]. Prints the table (`cargo test --lib
    /// text_similarity -- --nocapture`); every re-shoot pair must reach the threshold and every
    /// pair of different subjects stay under it.
    #[test]
    fn text_similarity_of_descriptions() {
        let subject = |name: &str| name.split(' ').next().unwrap_or(name).to_string();
        let mut same = Vec::new();
        let mut place = Vec::new();
        let mut different = Vec::new();
        for (i, (a, a_text)) in DESCRIPTIONS.iter().enumerate() {
            for (b, b_text) in DESCRIPTIONS.iter().skip(i + 1) {
                let similarity = text_similarity(a_text, b_text);
                if subject(a) == subject(b) {
                    same.push((similarity, *a, *b));
                } else if SAME_PLACE.contains(&(*a, *b)) {
                    place.push((similarity, *a, *b));
                } else {
                    different.push((similarity, *a, *b));
                }
            }
        }
        different.sort_by(|x, y| y.0.total_cmp(&x.0));
        let joined = |s: f64| if s >= SAME_TEXT { "joined" } else { "" };
        eprintln!("word similarity (Jaccard), joined at >= {SAME_TEXT}:");
        eprintln!("the same subject, filmed again:");
        for (s, a, b) in &same {
            eprintln!("  {a:<9} {b:<9} {s:.2} {}", joined(*s));
        }
        eprintln!("the same place, something else happening:");
        for (s, a, b) in &place {
            eprintln!("  {a:<9} {b:<9} {s:.2} {}", joined(*s));
        }
        eprintln!(
            "different subjects ({} pairs), the closest:",
            different.len()
        );
        for (s, a, b) in different.iter().take(6) {
            eprintln!("  {a:<9} {b:<9} {s:.2} {}", joined(*s));
        }
        for (s, a, b) in &same {
            assert!(*s >= SAME_TEXT, "{a} / {b}: {s}");
        }
        for (s, a, b) in &different {
            assert!(*s < SAME_TEXT, "{a} / {b}: {s}");
        }
    }

    #[test]
    fn words_drop_what_says_nothing_and_keep_four_letters() {
        assert_eq!(
            words("The camera follows two hikers, then a goat."),
            ["foll", "hike", "goat"]
        );
        assert_eq!(words("Козы пасутся на склоне"), ["козы", "пасу", "скло"]);
        assert_eq!(text_similarity("A goat.", "A goat."), 0.0, "too few words");
    }

    /// Two clips whose pictures have nothing in common (the camera moved) but whose descriptions
    /// say the same thing are grouped; the same summary never joins two stretches of one clip, and
    /// a blank stretch stays on its own whatever its description says.
    #[test]
    fn descriptions_saying_the_same_thing_group_clips_the_pictures_do_not() {
        let text = "A hiker in a red jacket walks along a rocky ridge at sunset.";
        let near = clip(text, 4.0, vec![(0.0, scene(1, 1, 20))]);
        let far = clip(
            "Close view of a hiker in a red jacket crossing a rocky ridge at sunset.",
            4.0,
            vec![(0.0, ramp())],
        );
        let checkers: Vec<u8> = (0..64)
            .map(|i| if (i / 8 + i % 8) % 2 == 0 { 200 } else { 20 })
            .collect();
        let other = clip(
            "Children play football in a school yard.",
            4.0,
            vec![(0.0, checkers)],
        );
        let blank = clip(text, 4.0, vec![(0.0, vec![9; 64])]);
        let grouping = group_clips(&[&near, &far, &other, &blank]);
        let ids: Vec<usize> = grouping.clips.iter().map(|c| c.group).collect();
        assert_eq!(ids, [1, 1, 2, 3]);

        // One clip, a cut from the block to the ramp, no segments: both stretches carry the
        // summary, and still are two groups.
        let cut = clip(
            text,
            8.0,
            vec![
                (0.0, scene(1, 1, 20)),
                (2.0, scene(1, 1, 20)),
                (6.0, ramp()),
            ],
        );
        let grouping = group_clips(&[&cut]);
        let stretches = &grouping.clips[0].stretches;
        assert_eq!(stretches.len(), 2, "{stretches:?}");
        assert_ne!(stretches[0].group, stretches[1].group);
    }

    #[test]
    fn rotation_is_exact_on_the_grid_and_distances_are_bounded() {
        let values: Vec<f64> = (0..64).map(f64::from).collect();
        let four = (0..4).fold(values.clone(), |g, _| rotate(&g, 8));
        assert_eq!(four, values, "four quarter turns are the identity");
        let a = normalise(&scene(1, 1, 20)).expect("structure");
        assert!(scene_distance(&a, &a).abs() < 1e-9);
        let negative: Vec<f64> = a.iter().map(|v| -v).collect();
        assert!(
            (1.0 - correlation(&a, &negative) - 2.0).abs() < 1e-9,
            "the negative: 2.0"
        );
        let other = normalise(&ramp()).expect("structure");
        let d = scene_distance(&a, &other);
        assert!((SAME_SHOT..=2.0).contains(&d), "{d}");
        assert!(normalise(&[7; 64]).is_none(), "flat is blank");
    }
}
