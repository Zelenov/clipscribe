# Shot detection: find the cuts, describe each shot

Requested by the owner (2026-10-11). An edited video, such as a montage or an ending, is many shots
joined by cuts. If clipscribe knows exactly where the picture changes, it can do two things it
cannot do today:

1. **Split an edit into its shots.** It can list each shot with its start and end, so frename can
   put a marker at every cut or describe a montage shot by shot.
2. **Pick frames by shot instead of by window.** At least one frame per shot, taken from the
   middle of the shot rather than from a transition, and no frame budget spent twice on the same
   static picture.

This document compares how FFmpeg and other tools detect shot changes, measures them against
today's key frames on a test edit, and proposes a design for clipscribe. The prototype used for
the measurements is `shot-detection/prototype.py`.

## What clipscribe does today

Key frames (`docs/design/key-frames.md`) decode candidates every 0.5 s (up to 240) and reduce each
one to an 8×8 grid of mean luma. In each of `max_frames` windows, they keep the candidate that
differs most from the one before it. This finds frames *near* changes, but:
- it never says where a cut is, only that some candidate in a window changed;
- a cut is known to 0.5 s at best;
- the budget is spread by time, not by shot, so a long static shot can still take several frames;
- a frame can land in the middle of a dissolve, where two pictures are mixed.

## How FFmpeg does it

FFmpeg has two filters with the same score: `select='gt(scene,X)'` (`libavfilter/f_select.c`,
`get_scene_score`) and `scdet` (`libavfilter/vf_scdet.c`, since FFmpeg 4.4). For each decoded
frame, they compute the following.

1. `MAFD`: the mean absolute difference from the previous frame, over the luma plane for YUV input
   (all planes for RGB), as a percentage of full scale.
2. `diff = |MAFD − MAFD of the previous frame|`.
3. `score = min(MAFD, diff)`, clipped to 0–100 (`scdet`) or divided by 100 (`select`).

Taking the minimum is the clever part. A cut gives a large `MAFD` that is also a jump from the
frame before, so both terms are large. Steady motion, such as a pan, gives a large `MAFD` on every
frame but a small `diff`, so it is suppressed. `scdet` reports frames whose score is at least
`threshold`, which defaults to 10. Usage:

```sh
ffmpeg -i in.mp4 -vf "scdet=t=10" -an -f null -               # logs lavfi.scd.score and .time
ffmpeg -i in.mp4 -vf "select='gt(scene,0.3)',showinfo" -f null -
```

FFmpeg's own detection has these limits:
- the threshold is fixed, so a cut between two similar shots scores under it;
- it has no notion of a flash: a camera flash gives two high scores, one in and one out;
- it cannot see gradual transitions, because every step of a dissolve or fade is small.

FFmpeg has separate filters for some of the rest: `blackdetect` for black frames and `freezedetect`
for frozen video.

## Other tools

[PySceneDetect](https://github.com/Breakthrough/PySceneDetect), `scenedetect/detectors/`, has four
detectors:
- **`ContentDetector`:** the mean change of hue, saturation and luma between frames (edges
  optional). Threshold 27, minimum shot 15 frames.
- **`AdaptiveDetector`:** the content score divided by the average of its neighbours (window ±2
  frames). Threshold ratio 3.0, minimum score 15. A cut must stand out from its surroundings
  rather than pass a fixed bar, so camera motion and pans give fewer false cuts.
- **`ThresholdDetector`:** fades to and from black, by mean brightness under 12.
- **`HashDetector`:** a perceptual hash (8×8 DCT), threshold 0.35.

Neural detectors, such as TransNet V2 (Souček and Lokoč, 2020), are the most accurate on
dissolves. They need a model file and a tensor runtime. clipscribe has neither, and frename would
inherit them. This was rejected for the same reason as in `key-frames.md`.

## Measurement

Neither repository has a real edit to test on. The owner's `ending.mp4` could not be downloaded
here because the session's network policy blocks its host. So a test edit was built from the
repository's test clips with FFmpeg, 30.3 s at 30 fps, with every event known to the frame:

| Time (s) | Event | What a detector should do |
|---|---|---|
| 4.00, 7.33, 14.33, 17.33, 20.33, 23.33, 25.33 | hard cuts between different pictures | find each, to the frame |
| 28.33 | hard cut between two moments of the *same* Earth shot | find it (hardest) |
| 9.83–10.83 | 1 s dissolve | find it as a gradual transition, not a cut |
| 22.33–23.33 | 1 s fade to black | find it as a gradual transition |
| 15.83 | a 2-frame white flash | ignore |
| 4.0–7.3, 17.3–20.3 | a slow zoom, a fast pan | ignore |

Results:

| Method | Cuts found (of 8) | Same-scene cut | Flash | Pan, zoom | Dissolve, fade | Time |
|---|---|---|---|---|---|---|
| FFmpeg `scdet`, threshold 10 | 7, each on the exact frame | missed (score 0.76) | 2 false cuts (42, 43) | ignored (scores 3 to 4) | not seen (max 0.06) | 0.26 s |
| clipscribe key frames today | no cut list; frames land 0.07 s after 7 cuts | frame landed after it | missed by luck | ignored | one of the 16 frames is mid-dissolve (a mix of two pictures) | about 1 s |
| Prototype (below) | 8, each on the exact frame | found | ignored | ignored | both found as gradual | 0.27 s (Python, 64×36) |

Today's 16 key frames on the test edit (`shot-detection/keyframes-today.jpg`):
- frame 5 is the dissolve, a mix of two pictures;
- frames 0–1 are the same static Earth;
- frames 13–15 are three near-identical Earth frames;
- frames 2–3 are the slow zoom.

That is 6 of 16 frames spent on near-repeats or a mix, while every one of the 11 shots still got a
frame.

## Proposed design

### 1. One cheap, continuous decode

Today's candidates are separate accurate seeks. Finding cuts to the frame needs every frame, so
add a pass that decodes the clip once, front to back, scaled to 64×36 grey (GStreamer:
`decodebin ! videoconvert ! videoscale ! video/x-raw,format=GRAY8,width=64,height=36 ! appsink`),
and keeps only the 2 KB per frame. Then:
- the frames sent to the model are still decoded at 512 px by the existing seek path, but only at
  the chosen times;
- the cost is one full decode of the clip at low output size, and decoding dominates. On long 4K
  footage that is the main cost, so offer `avdec_*`'s `skip-frame=nonref` (skip B-frames) as a
  fast mode. It costs ±1–2 frames of cut precision.

### 2. Detect cuts, flashes and gradual transitions

Per frame `i`, with `d[i]` the MAFD of luma to frame `i−1`:
- **score** = FFmpeg's `min(d[i], |d[i] − d[i−1]|)`, so steady motion is suppressed;
- **cut:** score ≥ 0.5 and score ≥ 4 × the highest other score within ±1 s, ignoring `i±1`.
  This is an isolated spike, the adaptive idea of PySceneDetect, measured against the neighbours'
  maximum. It finds the same-scene cut (0.76 against about 0.01 around it) and rejects a pan's
  jittery scores;
- **flash:** a frame 1 to 4 frames after a spike is back within 35 % of the frame before it.
  Ignore the flash and its return. On the test edit, the isolated-spike rule already rejected the
  2-frame flash, because its in and out spikes are each other's neighbours. This rule is the
  backstop for a flash that lasts longer;
- **gradual transition:** frames 0.5 s apart differ by at least 5 while every single step is under
  35 % of that, and the middle frame is close to a weighted mix of the two ends (blend error under
  25 % of the span). The blend test is what separates a dissolve from a pan. It is a fade when one
  end is near black;
- **minimum shot:** 15 frames (0.5 s), as PySceneDetect does. A cut within a gradual transition
  merges into it. The prototype still reports one extra cut at a dissolve's first frame; that is
  the merge rule's job.

All thresholds are provisional. Tune them on real edits, starting with the owner's `ending.mp4`,
and keep a few short real samples as test fixtures.

### 3. Shots as output

The library gets `detect_shots(video, &options, cancel) -> Vec<Shot>`, with
`Shot { start_s, end_s, enters_by: Cut | Dissolve | Fade }` and the gradual transitions as their
own ranges. The CLI gets `--shots`, which prints them, or adds `shots` to `--json`. frename can
then:
- put a marker at every cut;
- show the shots on the timeline;
- offer "split this edit into shots" as ranges, or as an EDL later.

### 4. Frames chosen by shot

`FrameSampling::Shots` would become the new default once tuned:
1. every shot of at least 0.5 s gets one frame, from its middle. Never from a transition: a
   shot's range excludes the dissolve and fade ranges;
2. the rest of the budget goes to the longest shots and the shots with the most change inside
   them (the sum of `d`), with extra frames spread within those shots;
3. shots that look the same as an earlier shot share a frame. Use the 8×8 fingerprint distance
   that grouping already uses (`group_clips`), so three takes of the same Earth cost one frame;
4. with more shots than budget, keep the longest and most different shots, and say so in the
   result (`Description` notes "N short shots not sampled").

The frames sent are labelled with their shot (`shot 3 of 11, 0:07.3–0:09.8`) in the request, so
the model's moments can follow real shot boundaries. `important`-mode moments and the main range
can then snap to cuts.

## Risks

- Real footage has camera flashes, strobes, fast whip-pans and grain. The isolated-spike rule and
  the flash rule cover the common cases, and tuning on real edits must check the rest.
- Raw camera clips (the main frename use) mostly have no cuts. Shot detection then adds one cheap
  decode and finds one shot, and frames fall back to today's spread inside it. This must not cost
  more than a few seconds per clip of a normal length, so measure on 4K.
- A full decode reads the whole file once more. Keep it cancellable, and cache shots per file in
  the folder cache (whole folders, #8).

## Plan

1. Library: `detect_shots` with the continuous grey decode, cut, flash and gradual rules, and
   tests on generated edits (the FFmpeg recipe below) plus short real fixtures.
2. CLI `--shots` and `shots` in `--json`.
3. `FrameSampling::Shots`, behind a flag first and the default after tuning on real edits.
4. frename: a marker at each cut, and "describe shot by shot" for edits. These are frename issues.

## Test edit recipe

Each segment is rendered to 480×270, 30 fps, H.264 from `tests/clips` and the test images, then
joined with the FFmpeg concat demuxer:
- an Earth clip;
- a JPG with `zoompan`;
- a PNG with an `xfade=fade` into `smptehdbars`;
- `testsrc2` with a 2-frame white `drawbox`;
- a JPG with a moving `crop` (pan);
- a JPG with `fade=t=out`;
- a TIFF;
- two moments of the Earth clip.

`shot-detection/make-test-edit.sh` has the exact commands and prints how to run the prototype.
