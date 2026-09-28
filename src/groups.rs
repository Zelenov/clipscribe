//! Grouping similar footage across clips, and stretches within clips, from the fingerprints of the
//! frames each clip was described from: see [`group_clips`] and the design notes in
//! `docs/design/whole-folders.md`.
//!
//! No GStreamer or image dependency: a [`DescribedClip`] carries its frames' fingerprints (8×8
//! grids of average luma, the block-mean-value hash key frames already use), so grouping runs
//! without the `frames` feature, on clips described earlier and read back from the cache.

use crate::describe::fingerprint_diff;
use crate::folder::{DescribedClip, FrameFingerprint};

/// A grid whose cells spread less than this (standard deviation, 0–255 scale) has no structure to
/// compare — black, a flat wall, fine noise — and never matches anything.
const BLANK_SPREAD: f64 = 4.0;
/// Two consecutive frames are a cut when their structure differs by more than this (`1 − r`)...
const CUT_STRUCTURE: f64 = 0.5;
/// ...and their level of light by more than this (mean absolute difference, 0.0–1.0).
const CUT_LEVEL: f64 = 0.05;
/// Two stretches closer than this (`1 − r`, over the four rotations) show the same scene.
pub(crate) const SAME_SCENE: f64 = 0.2;
/// A description segment names a stretch when it covers at least this fraction of it.
const LABEL_COVERAGE: f64 = 0.5;

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

/// Footage that shows the same scene.
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

/// Group `clips` by what their frames look like: each clip is cut into stretches where the
/// picture changes to something else, and stretches (in any clips, or the same one) whose average
/// picture has the same layout of light and dark — whatever the exposure, and in any of the four
/// 90° rotations — share a group. A group is labelled with the description of its most typical
/// stretch; no request is made. Deterministic: the same clips always give the same groups and ids.
pub fn group_clips(clips: &[&DescribedClip]) -> Grouping {
    struct Piece {
        clip: usize,
        start_s: f64,
        end_s: f64,
        signature: Option<Vec<f64>>,
    }
    let pieces: Vec<Piece> = clips
        .iter()
        .enumerate()
        .flat_map(|(clip, described)| {
            stretches_of(described)
                .into_iter()
                .map(move |(start_s, end_s, signature)| Piece {
                    clip,
                    start_s,
                    end_s,
                    signature,
                })
        })
        .collect();

    let mut sets = DisjointSets::new(pieces.len());
    for (i, a) in pieces.iter().enumerate() {
        let Some(a_sig) = &a.signature else { continue };
        for (j, b) in pieces.iter().enumerate().skip(i + 1) {
            let Some(b_sig) = &b.signature else { continue };
            if scene_distance(a_sig, b_sig) < SAME_SCENE {
                sets.union(i, j);
            }
        }
    }

    // Ids in order of first appearance; members of each group in piece order.
    let mut id_of_root = std::collections::HashMap::new();
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
                        .map(|&j| match (&pieces[i].signature, &pieces[j].signature) {
                            (Some(a), Some(b)) => scene_distance(a, b),
                            _ => 0.0,
                        })
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
                    stretches
                        .iter()
                        .map(|s| {
                            (
                                s.group,
                                overlap(s.start_s, s.end_s, segment.start_s, segment.end_s),
                            )
                        })
                        .filter(|(_, overlap)| *overlap > 0.0)
                        .fold(None, |best: Option<(usize, f64)>, (g, o)| match best {
                            Some((_, best_o)) if best_o >= o => best,
                            _ => Some((g, o)),
                        })
                        .map_or(group, |(g, _)| g)
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
    fn the_same_scene_groups_across_clips_whatever_the_exposure_and_rotation() {
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
        assert!((SAME_SCENE..=2.0).contains(&d), "{d}");
        assert!(normalise(&[7; 64]).is_none(), "flat is blank");
    }
}
