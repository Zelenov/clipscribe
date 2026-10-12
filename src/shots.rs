//! Shots of an edited video: where the picture cuts, dissolves or fades, found from small grey
//! frames. See `docs/design/shot-detection.md`.
//!
//! [`find_shots`] works on decoded frames and needs no GStreamer; [`crate::detect_shots`] decodes a
//! file and calls it.
//!
//! Rules, on 64×36 grey frames:
//! - the score of frame `i` is FFmpeg's `min(MAFD, |ΔMAFD|)` (`MAFD`: mean absolute difference
//!   from the previous frame, in percent), so steady motion scores low;
//! - a **cut** is an isolated spike of that score: at least [`CUT_SCORE`] and
//!   [`CUT_ISOLATION`] times the highest other score within a second, not counting the frames
//!   next to it;
//! - a spike whose picture comes back within four frames is a **flash** and is ignored;
//! - a **gradual transition** is a half second over which the picture changes by at least
//!   [`GRADUAL_SPAN`] while every single step stays small and the middle frame is close to a mix
//!   of the two ends (this separates a dissolve from a pan); a **fade** is one with a black end;
//! - a shot is at least half a second; a cut inside a transition belongs to it.
//!
//! All thresholds are provisional until tuned on real edits.

/// Width of the frames [`find_shots`] expects.
pub const FRAME_WIDTH: usize = 64;
/// Height of the frames [`find_shots`] expects.
pub const FRAME_HEIGHT: usize = 36;

/// The least score (percent of full scale) of a cut.
const CUT_SCORE: f32 = 0.5;
/// How many times higher than every other score within a second a cut's score must be.
const CUT_ISOLATION: f32 = 4.0;
/// A floor for "every other score", so a perfectly still shot does not make noise a cut.
const QUIET_FLOOR: f32 = 0.05;
/// A spike is a flash when the picture comes back to within this share of the jump.
const FLASH_RETURN: f32 = 0.35;
/// The least change (percent) over half a second that makes a gradual transition.
const GRADUAL_SPAN: f32 = 5.0;
/// A step of a gradual transition is under this share of its whole change.
const GRADUAL_STEP: f32 = 0.35;
/// The middle of a gradual transition is within this share of its change from the best mix.
const BLEND_ERROR: f32 = 0.25;
/// Mean brightness (percent) under which a picture counts as black.
const BLACK: f32 = 4.0;
/// The shortest shot, in seconds.
const MIN_SHOT_S: f64 = 0.5;

/// How the picture changes from one shot to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransitionKind {
    /// From one frame to the next.
    Cut,
    /// One picture mixes into another.
    Dissolve,
    /// To or from black.
    Fade,
}

impl TransitionKind {
    /// `cut`, `dissolve` or `fade`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Cut => "cut",
            Self::Dissolve => "dissolve",
            Self::Fade => "fade",
        }
    }
}

/// A change of picture. A cut has `start_s == end_s`, the time of the first new frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transition {
    pub kind: TransitionKind,
    /// Seconds from the clip's start: where a dissolve or fade begins (found up to a third of a
    /// second late for now), the first frame of the new picture for a cut.
    pub start_s: f64,
    /// Seconds: the first frame of the new picture; equal to `start_s` for a cut.
    pub end_s: f64,
}

/// One shot: a stretch of the clip without a transition inside. Dissolves and fades belong to no
/// shot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shot {
    /// Seconds from the clip's start: the first frame (after a dissolve or fade, the end of it).
    pub start_s: f64,
    /// Seconds: where the next transition starts (for a cut, its first new frame), or the clip's
    /// end. Between this and the next shot's `start_s` lies a dissolve or fade.
    pub end_s: f64,
    /// How the shot begins; `None` for the first one.
    pub enters_by: Option<TransitionKind>,
}

/// The shots of a clip and the transitions between them, in time order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shots {
    /// In time order; empty only for a clip without frames.
    pub shots: Vec<Shot>,
    /// In time order; always one fewer than `shots`.
    pub transitions: Vec<Transition>,
}

/// Mean absolute difference of two frames, percent of full scale.
fn mad(a: &[u8], b: &[u8]) -> f32 {
    let sum: u32 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u32).sum();
    sum as f32 * 100.0 / (a.len() as f32 * 255.0)
}

/// Mean brightness, percent of full scale.
fn mean(a: &[u8]) -> f32 {
    a.iter().map(|x| *x as u32).sum::<u32>() as f32 * 100.0 / (a.len() as f32 * 255.0)
}

/// How far `middle` is from the mix of `a` and `b` that fits it best, percent.
fn blend_error(a: &[u8], middle: &[u8], b: &[u8]) -> f32 {
    (1..10)
        .map(|k| {
            let w = k as f32 / 10.0;
            let sum: f32 = a
                .iter()
                .zip(middle)
                .zip(b)
                .map(|((x, m), z)| (*m as f32 - ((1.0 - w) * *x as f32 + w * *z as f32)).abs())
                .sum();
            sum * 100.0 / (a.len() as f32 * 255.0)
        })
        .fold(99.0, f32::min)
}

/// A gradual transition found by [`gradual`]: frames `from..=to`.
struct Gradual {
    from: usize,
    to: usize,
    kind: TransitionKind,
}

/// The shots of a clip from its frames: `frames` are grey, [`FRAME_WIDTH`] × [`FRAME_HEIGHT`], in
/// order, `times_s` their times, `end_s` the clip's end. Fewer than two frames are one shot.
pub fn find_shots<F: AsRef<[u8]>>(frames: &[F], times_s: &[f64], end_s: f64) -> Shots {
    let n = frames.len().min(times_s.len());
    debug_assert!(frames
        .iter()
        .all(|f| f.as_ref().len() == FRAME_WIDTH * FRAME_HEIGHT));
    if n == 0 {
        return Shots::default();
    }
    let start_s = times_s[0];
    let end_s = end_s.max(times_s[n - 1]);
    let whole = Shots {
        shots: vec![Shot {
            start_s,
            end_s,
            enters_by: None,
        }],
        transitions: Vec::new(),
    };
    let fps = if n > 1 && times_s[n - 1] > start_s {
        (n - 1) as f64 / (times_s[n - 1] - start_s)
    } else {
        return whole;
    };
    let views: Vec<&[u8]> = frames[..n].iter().map(|x| x.as_ref()).collect();

    // d[i]: the change from frame i-1; score[i]: FFmpeg's scdet score.
    let mut d = vec![0.0f32; n];
    for (i, pair) in views.windows(2).enumerate() {
        d[i + 1] = mad(pair[0], pair[1]);
    }
    let mut score = vec![0.0f32; n];
    for (i, pair) in d.windows(2).enumerate().skip(1) {
        score[i + 1] = pair[1].min((pair[1] - pair[0]).abs());
    }

    let second = (fps.round() as usize).max(1);
    let cuts = cuts(&views, &d, &score, n, second);
    let min_frames = ((MIN_SHOT_S * fps).round() as usize).max(1);
    let gradual = gradual(&views, &d, &cuts, n, min_frames.max(2));

    // Events in frame order; a cut that touches a gradual transition belongs to it.
    enum Event {
        Cut(usize),
        Gradual(Gradual),
    }
    let mut events: Vec<(usize, Event)> = gradual
        .into_iter()
        .map(|g| (g.from, Event::Gradual(g)))
        .collect();
    for c in cuts {
        let inside = events.iter().any(|(_, e)| match e {
            Event::Gradual(g) => c + 2 >= g.from && c <= g.to + 2,
            Event::Cut(_) => false,
        });
        if !inside {
            events.push((c, Event::Cut(c)));
        }
    }
    events.sort_by_key(|(at, _)| *at);

    let mut shots = Vec::new();
    let mut transitions = Vec::new();
    let mut open = (start_s, None);
    let mut open_frame = 0usize;
    for (_, event) in events {
        let (from, to, kind) = match event {
            Event::Cut(c) => (c, c, TransitionKind::Cut),
            Event::Gradual(g) => (g.from, g.to, g.kind),
        };
        // Shots under the minimum are not split off.
        if from < open_frame + min_frames || n - to < min_frames {
            continue;
        }
        let (a, b) = (times_s[from], times_s[to]);
        shots.push(Shot {
            start_s: open.0,
            end_s: if kind == TransitionKind::Cut { b } else { a },
            enters_by: open.1,
        });
        transitions.push(Transition {
            kind,
            start_s: a,
            end_s: b,
        });
        open = (b, Some(kind));
        open_frame = to;
    }
    shots.push(Shot {
        start_s: open.0,
        end_s,
        enters_by: open.1,
    });
    Shots { shots, transitions }
}

/// The frames that start a cut; flashes are skipped.
fn cuts(f: &[&[u8]], d: &[f32], score: &[f32], n: usize, second: usize) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut skip = vec![false; n];
    for i in 2..n {
        if skip[i] || score[i] < CUT_SCORE {
            continue;
        }
        let others = (i.saturating_sub(second).max(2)..(i + second + 1).min(n))
            .filter(|j| j.abs_diff(i) > 1 && !skip[*j])
            .map(|j| score[j])
            .fold(0.0, f32::max);
        if score[i] < CUT_ISOLATION * others.max(QUIET_FLOOR) {
            continue;
        }
        let back = (i + 1..(i + 5).min(n))
            .map(|j| (mad(f[i - 1], f[j]), j))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        match back {
            Some((distance, j)) if distance < FLASH_RETURN * d[i] => {
                skip[i..=j].iter_mut().for_each(|s| *s = true);
            }
            _ => cuts.push(i),
        }
    }
    cuts
}

/// Dissolves and fades: half-second spans that change a lot in small steps by mixing two pictures.
fn gradual(f: &[&[u8]], d: &[f32], cuts: &[usize], n: usize, half_second: usize) -> Vec<Gradual> {
    let mut found = Vec::new();
    let mut i = half_second;
    while i < n {
        let (a, mut b) = (i - half_second, i);
        let span = mad(f[a], f[b]);
        let steps = d[a + 1..=b].iter().copied().fold(0.0, f32::max);
        if span >= GRADUAL_SPAN
            && steps < GRADUAL_STEP * span
            && !cuts.iter().any(|c| (a..=b).contains(c))
            && blend_error(f[a], f[a.midpoint(b)], f[b]) < BLEND_ERROR * span
        {
            // Extend while the picture keeps moving away from where it began.
            while b + 1 < n {
                let next = mad(f[a], f[b + 1]);
                if next > mad(f[a], f[b]) + 0.2 && d[b + 1] < GRADUAL_STEP * next {
                    b += 1;
                } else {
                    break;
                }
            }
            let kind = if mean(f[a]) < BLACK || mean(f[b]) < BLACK {
                TransitionKind::Fade
            } else {
                TransitionKind::Dissolve
            };
            found.push(Gradual {
                from: a,
                to: b,
                kind,
            });
            i = b + half_second;
        } else {
            i += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = 30.0;
    const PIXELS: usize = FRAME_WIDTH * FRAME_HEIGHT;

    /// A picture with a gradient and a seed-dependent pattern, so two seeds differ everywhere.
    fn picture(seed: u32) -> Vec<u8> {
        (0..PIXELS)
            .map(|p| {
                let x = (p % FRAME_WIDTH) as u32;
                let y = (p / FRAME_WIDTH) as u32;
                ((x * 7 + y * 13 + seed * 97 + (x * y * seed) % 41) % 200 + 30) as u8
            })
            .collect()
    }

    fn mix(a: &[u8], b: &[u8], w: f32) -> Vec<u8> {
        a.iter()
            .zip(b)
            .map(|(x, y)| ((1.0 - w) * *x as f32 + w * *y as f32).round() as u8)
            .collect()
    }

    /// `frames` repeated `n` times with a faint drift, as a camera's noise.
    fn hold(picture: &[u8], n: usize, start: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                picture
                    .iter()
                    .enumerate()
                    .map(|(p, v)| v.saturating_add(((p + start + i) % 3) as u8 / 2))
                    .collect()
            })
            .collect()
    }

    fn run(frames: Vec<Vec<u8>>) -> Shots {
        let times: Vec<f64> = (0..frames.len()).map(|i| i as f64 / FPS).collect();
        find_shots(&frames, &times, frames.len() as f64 / FPS)
    }

    #[test]
    fn a_still_clip_is_one_shot() {
        let shots = run(hold(&picture(1), 90, 0));
        assert_eq!(shots.shots.len(), 1);
        assert_eq!(shots.shots[0].enters_by, None);
        assert!(shots.transitions.is_empty());
        assert_eq!(shots.shots[0].end_s, 3.0);
    }

    #[test]
    fn no_frames_no_shots_and_one_frame_one_shot() {
        assert_eq!(find_shots::<Vec<u8>>(&[], &[], 0.0), Shots::default());
        let one = find_shots(&[picture(1)], &[0.0], 0.1);
        assert_eq!(one.shots.len(), 1);
    }

    #[test]
    fn cuts_are_found_to_the_frame() {
        let mut frames = hold(&picture(1), 60, 0);
        frames.extend(hold(&picture(2), 45, 0));
        frames.extend(hold(&picture(3), 60, 0));
        let shots = run(frames);
        assert_eq!(shots.shots.len(), 3, "{shots:?}");
        assert_eq!(shots.transitions.len(), 2);
        assert!(shots
            .transitions
            .iter()
            .all(|t| t.kind == TransitionKind::Cut && t.start_s == t.end_s));
        assert_eq!(shots.transitions[0].start_s, 2.0);
        assert_eq!(shots.transitions[1].start_s, 3.5);
        assert_eq!(shots.shots[1].enters_by, Some(TransitionKind::Cut));
        assert_eq!(shots.shots[0].end_s, 2.0);
        assert_eq!(shots.shots[1].start_s, 2.0);
        assert_eq!(shots.shots[2].end_s, 5.5);
    }

    #[test]
    fn a_two_frame_flash_is_not_a_cut() {
        let mut frames = hold(&picture(1), 90, 0);
        for flash in &mut frames[40..42] {
            *flash = vec![255; PIXELS];
        }
        let shots = run(frames);
        assert_eq!(shots.shots.len(), 1, "{shots:?}");
    }

    #[test]
    fn a_dissolve_is_a_transition_not_a_shot() {
        let (a, b) = (picture(1), picture(2));
        let mut frames = hold(&a, 60, 0);
        frames.extend((1..=30).map(|k| mix(&a, &b, k as f32 / 30.0)));
        frames.extend(hold(&b, 60, 0));
        let shots = run(frames);
        assert_eq!(shots.shots.len(), 2, "{shots:?}");
        assert_eq!(shots.transitions.len(), 1);
        let t = shots.transitions[0];
        assert_eq!(t.kind, TransitionKind::Dissolve);
        assert!((1.9..=2.2).contains(&t.start_s), "{t:?}");
        assert!((2.8..=3.2).contains(&t.end_s), "{t:?}");
        assert_eq!(shots.shots[0].end_s, t.start_s);
        assert_eq!(shots.shots[1].start_s, t.end_s);
        assert_eq!(shots.shots[1].enters_by, Some(TransitionKind::Dissolve));
    }

    #[test]
    fn a_fade_to_black_is_a_fade() {
        let a = picture(1);
        let black = vec![0u8; PIXELS];
        let mut frames = hold(&a, 60, 0);
        frames.extend((1..=30).map(|k| mix(&a, &black, k as f32 / 30.0)));
        frames.extend(vec![black; 60]);
        let shots = run(frames);
        assert!(
            shots
                .transitions
                .iter()
                .any(|t| t.kind == TransitionKind::Fade),
            "{shots:?}"
        );
    }

    #[test]
    fn steady_motion_is_not_a_transition() {
        // A picture scrolling a column per frame: every step the same size.
        let wide: Vec<u8> = picture(5);
        let frames: Vec<Vec<u8>> = (0..90)
            .map(|i| {
                (0..PIXELS)
                    .map(|p| {
                        wide[(p / FRAME_WIDTH) * FRAME_WIDTH + (p % FRAME_WIDTH + i) % FRAME_WIDTH]
                    })
                    .collect()
            })
            .collect();
        let shots = run(frames);
        assert_eq!(shots.shots.len(), 1, "{shots:?}");
    }

    #[test]
    fn a_cut_too_close_to_the_start_does_not_make_a_shot() {
        let mut frames = hold(&picture(1), 10, 0);
        frames.extend(hold(&picture(2), 80, 0));
        let shots = run(frames);
        assert_eq!(shots.shots.len(), 1, "{shots:?}");
    }
}
