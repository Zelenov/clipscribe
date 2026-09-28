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
/// (shorter words are dropped in every language), and English words about the footage itself —
/// how it was framed or filmed rather than what it shows. Best effort, not complete: the model
/// has more ways to say "close view" than any list. Matched against the whole word, and against
/// the word without a final "s" ("clips", "views"), never against its four-letter stem (which
/// would drop "football" with "footage").
const STOP_WORDS: &[&str] = &[
    // Function words.
    "about",
    "above",
    "across",
    "after",
    "against",
    "along",
    "also",
    "among",
    "around",
    "before",
    "behind",
    "being",
    "below",
    "beneath",
    "beside",
    "between",
    "both",
    "during",
    "each",
    "from",
    "have",
    "into",
    "just",
    "near",
    "onto",
    "other",
    "over",
    "some",
    "that",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "through",
    "toward",
    "towards",
    "under",
    "very",
    "where",
    "which",
    "while",
    "with",
    // The footage itself: what filmed it, how it is framed, how it is seen.
    "aerial",
    "angle",
    "background",
    "camera",
    "captured",
    "clip",
    "close",
    "closer",
    "closeup",
    "filmed",
    "footage",
    "foreground",
    "frame",
    "framed",
    "framing",
    "lapse",
    "overhead",
    "panning",
    "scene",
    "seen",
    "shot",
    "show",
    "shown",
    "showing",
    "time",
    "timelapse",
    "video",
    "view",
    "viewed",
    "visible",
    "wide",
    "wider",
    "zoom",
    "zoomed",
    "zooming",
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
///   clip or no segment falls in it; common words and words about the framing dropped, each word
///   cut to its first four letters) — the same subject or activity, even after the camera moved
///   or zoomed.
///
/// Groups are what those links connect, with one rule: words never put two stretches of one clip
/// in the same group, not even through other clips (stretches of one clip often carry the same
/// summary, so every clip described like it would otherwise glue them together). Only their
/// pictures can: a cut back to the same shot. A link by words that would bring two stretches of
/// one clip together is skipped; the links by words are taken in input order, so which stretch
/// of a clip joins a group like that is the first one. A stretch whose frames are all blank
/// (black, a flat wall) is a group of its own.
///
/// The word signal only sees what the descriptions say, and it has false positives: clips of the
/// same place or person doing different things, and sometimes unrelated clips described in
/// similar everyday words ("a woman in a bright kitchen", "a woman in a bright office"), share
/// enough words to be joined; two of the same thing described in different words are not. Groups
/// are connected sets, so on a large folder from one shoot these links can chain several groups
/// into one: check groups before relying on them. A group is labelled with the description of its
/// most typical stretch; no request is made. Deterministic: the same clips in the same order
/// always give the same groups and ids.
pub fn group_clips(clips: &[&DescribedClip]) -> Grouping {
    let mut vocabulary: HashMap<String, usize> = HashMap::new();
    let mut pieces: Vec<Piece> = Vec::new();
    for (clip, described) in clips.iter().enumerate() {
        let stretches = stretches_of(described);
        let texts = stretch_texts(described, &stretches);
        for ((start_s, end_s, signature), text) in stretches.into_iter().zip(texts) {
            pieces.push(Piece {
                clip,
                start_s,
                end_s,
                turns: signature.map(Turns::new),
                words: word_ids(text, &mut vocabulary),
            });
        }
    }

    // Pictures first: they may join any two stretches, of one clip or not.
    let mut sets = DisjointSets::new(pieces.len());
    for (i, a) in pieces.iter().enumerate() {
        for (j, b) in pieces.iter().enumerate().skip(i + 1) {
            if same_shot(a, b) {
                sets.union(i, j);
            }
        }
    }
    // Then words, only between groups with no clip in common: so no stretch of a clip can reach
    // another stretch of it through a link by words, directly or through other clips. (When two
    // stretches of one clip end up in one group, the group held both before any link by words was
    // made, so pictures alone joined them.)
    let mut clips_in: Vec<BTreeSet<usize>> =
        pieces.iter().map(|p| BTreeSet::from([p.clip])).collect();
    for i in 0..pieces.len() {
        let root = sets.find(i);
        if root != i {
            merge_into(&mut clips_in, root, i);
        }
    }
    for (i, a) in pieces.iter().enumerate() {
        for (j, b) in pieces.iter().enumerate().skip(i + 1) {
            if !same_words(a, b) {
                continue;
            }
            let (ra, rb) = (sets.find(i), sets.find(j));
            if ra == rb || !clips_in[ra].is_disjoint(&clips_in[rb]) {
                continue;
            }
            let (root, other) = sets.union(ra, rb);
            merge_into(&mut clips_in, root, other);
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
                        .map(|&j| link_distance(&pieces[i], &pieces[j]).unwrap_or(0.0))
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

/// One stretch of one clip, as [`group_clips`] compares it.
struct Piece {
    /// Its clip's index in the slice given to [`group_clips`].
    clip: usize,
    start_s: f64,
    end_s: f64,
    /// Its picture; `None` when every frame of it is blank.
    turns: Option<Turns>,
    /// Its words, as sorted ids (see [`word_ids`]); empty when too few to compare.
    words: Vec<usize>,
}

/// The picture signal between two stretches ([`shot_distance`]); `None` when either is blank.
/// With [`word_signal`], the only place that says which stretches a signal may compare:
/// [`same_shot`], [`same_words`] and [`link_distance`] only weigh the results.
fn picture_signal(a: &Piece, b: &Piece) -> Option<f64> {
    Some(shot_distance(a.turns.as_ref()?, b.turns.as_ref()?))
}

/// The word signal between two stretches (the [`jaccard`] index of their words); `None` when
/// either is blank (its words would gather every black leader into one group), both are of one
/// clip (see [`group_clips`] for through other clips), or either has too few words.
fn word_signal(a: &Piece, b: &Piece) -> Option<f64> {
    let comparable = a.turns.is_some()
        && b.turns.is_some()
        && a.clip != b.clip
        && !a.words.is_empty()
        && !b.words.is_empty();
    comparable.then(|| jaccard(&a.words, &b.words))
}

/// Whether two stretches are the same shot (see [`SAME_SHOT`]).
fn same_shot(a: &Piece, b: &Piece) -> bool {
    picture_signal(a, b).is_some_and(|d| d < SAME_SHOT)
}

/// Whether two stretches are described in the same words (see [`SAME_TEXT`]).
fn same_words(a: &Piece, b: &Piece) -> bool {
    word_signal(a, b).is_some_and(|w| w >= SAME_TEXT)
}

/// How far apart two stretches are, for picking a group's most typical one: each signal's
/// distance over its threshold (1.0 at the threshold), the closer of the two; `None` when one of
/// them is blank.
fn link_distance(a: &Piece, b: &Piece) -> Option<f64> {
    let picture = picture_signal(a, b)? / SAME_SHOT;
    let words = word_signal(a, b).map_or(f64::INFINITY, |w| (1.0 - w) / (1.0 - SAME_TEXT));
    Some(picture.min(words))
}

/// Moves the clips of group `from` into group `into`'s (the larger set taking the smaller).
fn merge_into(clips_in: &mut [BTreeSet<usize>], into: usize, from: usize) {
    let mut moved = std::mem::take(&mut clips_in[from]);
    if moved.len() > clips_in[into].len() {
        std::mem::swap(&mut moved, &mut clips_in[into]);
    }
    clips_in[into].extend(moved);
}

/// `words` as a sorted set of ids into `vocabulary` (new words added to it); empty when there are
/// fewer than [`MIN_WORDS`] distinct ones, too few to compare.
fn word_ids(words: Vec<String>, vocabulary: &mut HashMap<String, usize>) -> Vec<usize> {
    let set: BTreeSet<usize> = words
        .into_iter()
        .map(|word| {
            let next = vocabulary.len();
            *vocabulary.entry(word).or_insert(next)
        })
        .collect();
    if set.len() >= MIN_WORDS {
        set.into_iter().collect()
    } else {
        Vec::new()
    }
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
/// (with or without a final "s") dropped, each cut to its first [`STEM_LETTERS`] letters. Works
/// the same in every language the descriptions come in, except that only English has stop words
/// beyond the length rule. The length rule drops short content words too ("dog", "car", "sea",
/// "red"): they never count.
fn words(text: &str) -> Vec<String> {
    let stop = |word: &str| {
        STOP_WORDS.contains(&word)
            || word
                .strip_suffix('s')
                .is_some_and(|w| STOP_WORDS.contains(&w))
    };
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.chars().count() >= 4 && !stop(word))
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
    let a = word_ids(words(a), &mut vocabulary);
    let b = word_ids(words(b), &mut vocabulary);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    jaccard(&a, &b)
}

/// The picture signal between `a` and `b` (the words are not looked at): the smallest
/// [`shot_distance`] between a stretch of one and a stretch of the other (`None` when either has
/// no stretch with a signature). For measuring [`SAME_SHOT`] on real clips.
#[cfg(test)]
pub(crate) fn clip_distance(a: &DescribedClip, b: &DescribedClip) -> Option<f64> {
    let signatures = |clip: &DescribedClip| -> Vec<Turns> {
        stretches_of(clip)
            .into_iter()
            .filter_map(|(_, _, signature)| signature.map(Turns::new))
            .collect()
    };
    let (a, b) = (signatures(a), signatures(b));
    a.iter()
        .flat_map(|x| b.iter().map(move |y| shot_distance(x, y)))
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
    // Four running sums rather than one, so the compiler can overlap the additions: this runs
    // four times for every pair of stretches.
    let mut sums = [0.0; 4];
    let ((a_quads, a_rest), (b_quads, b_rest)) = (a.as_chunks::<4>(), b.as_chunks::<4>());
    let tail: f64 = a_rest.iter().zip(b_rest).map(|(x, y)| x * y).sum();
    for (x, y) in a_quads.iter().zip(b_quads) {
        for k in 0..4 {
            sums[k] += x[k] * y[k];
        }
    }
    (sums.iter().sum::<f64>() + tail) / a.len() as f64
}

/// A stretch signature and its three 90° turns (just the signature when the grid is not square),
/// made once per stretch so that comparing two stretches allocates nothing.
struct Turns(Vec<Vec<f64>>);

impl Turns {
    fn new(signature: Vec<f64>) -> Self {
        let mut turns = vec![signature];
        if let Some(side) = square_side(turns[0].len()) {
            for _ in 0..3 {
                let next = rotate(&turns[turns.len() - 1], side);
                turns.push(next);
            }
        }
        Self(turns)
    }
}

/// `1 − r` between two standardised grids, the smallest over the four 90° rotations of `b` (when
/// the grid is square): 0.0 for the same layout of light and dark, up to 2.0 for its negative.
/// Compared with [`SAME_SHOT`].
fn shot_distance(a: &Turns, b: &Turns) -> f64 {
    b.0.iter()
        .map(|turned| 1.0 - correlation(&a.0[0], turned))
        .fold(f64::INFINITY, f64::min)
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
    /// Returns the root kept and the root joined to it.
    fn union(&mut self, a: usize, b: usize) -> (usize, usize) {
        let (a, b) = (self.find(a), self.find(b));
        let (low, high) = (a.min(b), a.max(b));
        self.parent[high] = low;
        (low, high)
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
    const DESCRIPTIONS: &[(&str, &str)] = &[
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
        // A review's counterexample: three subjects of one hike that used to be joined on their
        // framing ("close view", "wide view") rather than on what they show.
        ("boots", "Close view of hiking boots on a winding dirt trail."),
        ("valley", "Wide view of the mountain valley with a river winding through it."),
        ("flowers", "A close view of wildflowers swaying in the wind beside the path."),
    ];

    /// Different things described in the same everyday words, as one-sentence summaries often
    /// are: what the word signal cannot tell apart. Printed, not asserted either way — they are
    /// here so the table shows the risk next to the favourable cases.
    const SAME_WORDS_OTHER_THINGS: &[(&str, &str)] = &[
        (
            "A woman chops vegetables in a bright kitchen.",
            "A woman types on a laptop in a bright office.",
        ),
        (
            "A family walks along a sandy beach at sunset.",
            "A family walks along a forest trail in the afternoon.",
        ),
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
        eprintln!("different things in the same everyday words (not asserted):");
        for (a, b) in SAME_WORDS_OTHER_THINGS {
            let s = text_similarity(a, b);
            eprintln!("  {s:.2} {:<6} {a} / {b}", joined(s));
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
        assert_eq!(
            words("Close views of goats, seen in two clips, wide shots, time-lapse background."),
            ["goat"],
            "framing words and their plurals"
        );
        assert_eq!(
            words("Footage of football."),
            ["foot"],
            "not dropped by stem"
        );
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

    /// A clip cut into stretches that all carry its summary (no segment covers them), and another
    /// clip described in the same words: the other clip may join one of them, never glue them
    /// together. The same for two segments of one clip each described like a third clip. Pictures
    /// still join stretches of one clip: a cut back to the same shot.
    #[test]
    fn words_never_join_two_stretches_of_one_clip_even_through_another_clip() {
        let text = "A hiker in a red jacket walks along a rocky ridge at sunset.";
        let checkers: Vec<u8> = (0..64)
            .map(|i| if (i / 8 + i % 8) % 2 == 0 { 200 } else { 20 })
            .collect();
        let cut = clip(
            text,
            8.0,
            vec![
                (0.0, scene(1, 1, 20)),
                (2.0, scene(1, 1, 20)),
                (6.0, ramp()),
            ],
        );
        let alone = group_clips(&[&cut]);
        assert_eq!(
            alone.clips[0]
                .stretches
                .iter()
                .map(|s| s.group)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        // Another picture, described like the first clip's summary.
        let other = clip(text, 4.0, vec![(0.0, checkers.clone())]);
        let grouping = group_clips(&[&cut, &other]);
        let stretches: Vec<usize> = grouping.clips[0]
            .stretches
            .iter()
            .map(|s| s.group)
            .collect();
        assert_eq!(stretches.len(), 2);
        assert_ne!(stretches[0], stretches[1], "{grouping:?}");
        assert_eq!(
            grouping.clips[1].group, stretches[0],
            "the other clip joins the first stretch"
        );

        // Two segments, one per stretch, each described like a third clip.
        let segmented = DescribedClip {
            description: Description {
                summary: "A walk in the hills.".to_string(),
                segments: vec![
                    Segment {
                        start_s: 0.0,
                        end_s: 3.0,
                        description: text.to_string(),
                    },
                    Segment {
                        start_s: 5.0,
                        end_s: 8.0,
                        description: "The hiker in the red jacket walks on along the rocky ridge."
                            .to_string(),
                    },
                ],
            },
            ..cut.clone()
        };
        let grouping = group_clips(&[&other, &segmented]);
        let stretches: Vec<usize> = grouping.clips[1]
            .stretches
            .iter()
            .map(|s| s.group)
            .collect();
        assert_eq!(stretches.len(), 2);
        assert_ne!(stretches[0], stretches[1], "{grouping:?}");

        // A cut away and back to the same shot: its pictures join the first and last stretch.
        let back = clip(
            text,
            12.0,
            vec![
                (0.0, scene(1, 1, 20)),
                (5.0, ramp()),
                (10.0, scene(1, 1, 22)),
            ],
        );
        let grouping = group_clips(&[&back, &other]);
        let stretches: Vec<usize> = grouping.clips[0]
            .stretches
            .iter()
            .map(|s| s.group)
            .collect();
        assert_eq!(stretches.len(), 3, "{grouping:?}");
        assert_eq!(stretches[0], stretches[2]);
        assert_ne!(stretches[0], stretches[1]);
    }

    #[test]
    fn rotation_is_exact_on_the_grid_and_distances_are_bounded() {
        let values: Vec<f64> = (0..64).map(f64::from).collect();
        let four = (0..4).fold(values.clone(), |g, _| rotate(&g, 8));
        assert_eq!(four, values, "four quarter turns are the identity");
        let a = normalise(&scene(1, 1, 20)).expect("structure");
        let a_turns = Turns::new(a.clone());
        assert!(shot_distance(&a_turns, &a_turns).abs() < 1e-9);
        let negative: Vec<f64> = a.iter().map(|v| -v).collect();
        assert!(
            (1.0 - correlation(&a, &negative) - 2.0).abs() < 1e-9,
            "the negative: 2.0"
        );
        let other = Turns::new(normalise(&ramp()).expect("structure"));
        let d = shot_distance(&a_turns, &other);
        assert!((SAME_SHOT..=2.0).contains(&d), "{d}");
        assert!(normalise(&[7; 64]).is_none(), "flat is blank");
    }
}
