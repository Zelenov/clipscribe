# Key frames instead of fixed-interval sampling

Issue #5. Today [`sample_times`](../../src/describe.rs) picks one frame every 2 s (up to 60,
spread evenly on longer clips) with no regard for what is actually on screen. A static shot (a
person talking to camera, a locked-off landscape) burns the same budget as a clip that cuts
between three different scenes, and a short event between two samples (up to 2 s wide) can be
missed entirely.

## Approaches considered

1. **Container/GOP keyframes** (`GST_BUFFER_FLAG_DELTA_UNIT` on the demuxed buffer — no decode
   needed to find them, per the [GStreamer discourse
   answer](https://discourse.gstreamer.org/t/how-to-determine-if-a-buffer-in-appsink-is-a-key-frame/2175)
   and the [gstreamer-devel
   thread](https://lists.freedesktop.org/archives/gstreamer-devel/2022-June/080049.html)).
   Cheapest option (no scaling, no pixel comparison), but a GOP boundary is the **encoder's**
   choice, not the clip's: most consumer/phone encoders emit an I-frame every 1–2 s or every fixed
   N frames regardless of content, so this would not shrink the budget on a static shot or place a
   frame at a cut that happens to fall inside a GOP. Rejected: it does not answer "did the picture
   change", only "did the encoder decide to refresh".
2. **Motion vectors / optical flow** between full-resolution decoded frames. More sensitive to
   real motion than a whole-frame histogram, but needs either format-specific bitstream parsing
   (motion vectors are not exposed by GStreamer's raw-video pads) or decoding every frame at full
   rate to run optical flow, which is far more CPU than this crate spends today and adds a new
   heavy dependency. Rejected for cost.
3. **Perceptual/embedding similarity** (a small CLIP-style model). Strongest signal, but a new
   large dependency (a model file, a tensor runtime) the crate does not have today and frename
   would inherit; overkill for "is this frame different from that one". Rejected for this crate;
   worth revisiting for the similarity/grouping work in #8, which already needs an embedding
   decision.
4. **Histogram/downsampled-frame differences between decoded candidate frames.** The frame is
   already decoded and scaled to 512 px for the AI request; comparing a cheap fingerprint of that
   same decode adds no new decode path and no new dependency (`image`, already linked). This is a
   long-standing, well-documented technique for shot-boundary detection — surveyed in ["Video
   shot boundary detection method using histogram differences and local image
   descriptor"](https://ieeexplore.ieee.org/document/7060883/) and the SIFT-PDH shot-boundary
   paper ([Shafiee & Solat, IJMIR
   2016](https://link.springer.com/article/10.1007/s13735-016-0095-6)), among others. **Chosen.**

## Design

### Fingerprint and score

`frames.rs` downsamples the already-decoded, already-scaled RGB frame to an 8×8 grid of average
luma values (Rec. 601 weights), 64 bytes. The change score between two fingerprints is their mean
absolute per-cell difference, normalised to 0.0–1.0. This is cheap (64 subtractions), stable
across JPEG quality and small compression noise (block-averaged, not pixel-exact), and needs no
new dependency.

### Candidates

`describe::candidate_times` reuses the existing `sample_times` shape (fixed interval, capped
count, evenly spread when a clip is long) but 4× denser and 4× the cap: one candidate every 0.5 s
(down from 2 s), up to 240 (`MAX_FRAMES * 4`, down from unbounded only by the same "spread evenly"
rule). `frames.rs` decodes every candidate exactly as it does today's samples (same fast/accurate
reseek logic in `Clip::sample`), computing a fingerprint and the JPEG for each. Oversampling by 4
means roughly 4× the seeks of today for the same clip length — still bounded (≤240 total,
`MAX_DURATION_S` unchanged) and, on the four test clips, well under a second of extra work; there
is no separate cheap pre-pass, so the CPU cost is "today's per-frame seek cost, four times".

### Selection

`describe::select_key_frames(candidates, max_frames)` splits the candidates into `max_frames`
equal-sized index windows (the same partition `chunks`-style code uses to split a slice into N
balanced parts) and keeps, in each window, the candidate whose score against the *previous*
candidate is highest — the moment inside that window where the picture changes the most. A window
with no real change keeps its first candidate, which is exactly the timestamp fixed-interval
sampling would have picked, so a clip with nothing happening in it comes out identical in spirit
to today's behaviour (same count, same spread), and a clip with a cut or a short event inside a
window gets the frame at that cut instead of a blind timestamp.

This does not change the frame *budget*: `max_frames` is still `describe::frame_count(duration_s)`
— the same number `estimate_usage` already prices — so `--estimate` needs no change and the cost
per clip is unchanged. Coverage is bounded by construction: two chosen frames can be at most two
window-widths apart (worst case: the last candidate of one window and the first of the next both
picked), i.e. at most twice today's fixed interval — still small next to a clip's length, and
tightens automatically as `max_frames` is spent (more windows, narrower each).

Indices returned are strictly increasing by construction (each window's pick lies in its own
non-overlapping index range), so no separate de-duplication pass is needed; the one case that
could repeat a near-identical frame — a fully static clip, every score ≈ 0 — resolves to picking
each window's first candidate, i.e. the same spread as today, which was already accepted.

For a clip whose frame budget is 0 or 1 (`frame_count(duration_s) <= 1`, today's below-2-s or
empty-clip cases), the candidate/selection machinery is skipped entirely and today's
`sample_times` (a single frame at the midpoint) is used unchanged — there is no "which window"
choice to make with one frame, and this keeps that edge case byte-for-byte the same as before.

### Markers / instant events

Out of scope here (that is issue #10, "important moments": how many *segments* the summary
returns). This issue only changes which *frames* are sent to the model; the model still receives
up to the same number of frames it does today and decides the summary and segments from them.

## Options and the CLI

`Options` gains `pub frame_sampling: FrameSampling` (`KeyFrames` | `Interval`), `KeyFrames` being
the new default behaviour and `Interval` reproducing today's fixed-interval sampling exactly (for
anyone who compares runs, or hits a clip where blind spacing genuinely works better). The CLI
gets `--frames keyframes|interval` (default `keyframes`).

### Decisions made without the owner

- **`--interval` and `--max-frames` are not added.** The issue's scope list mentions keeping them
  as flags, but today they are not flags at all — `FRAME_INTERVAL_S` and `MAX_FRAMES` are
  constants with no CLI or `Options` surface, so "kept" only applies once they exist. Turning them
  into runtime parameters touches `sample_times`, `frame_count`, `estimate_usage`,
  `candidate_times` and every caller (CLI, library, both feature-off builds), which is a
  separably-useful change on its own and not needed to fix "static shots waste the budget" (the
  actual problem this issue describes). Filed as a follow-up idea rather than folded in here.
- **Fingerprint size (8×8) and oversampling factor (4×) are fixed constants, not configurable.**
  They are implementation detail of the scoring, not something a user needs to reach for; a future
  issue can retune them with real-world evidence if key frames prove not sharp enough.
- **No dedicated dedup pass beyond the window structure above** — see "Selection".
