# Whole folders: resumable runs and grouping of similar footage

Issue #8 (stage 3 of `Zelenov/frename#17`). A folder of thousands of clips gets described (and
tagged, with `--tags`) and grouped in one run; a stopped run resumes where it left off, several
clips are in flight at once, and a budget cap stops the run cleanly. Four parts, each usable on its
own from the library so frename can drive them with its own progress UI:

1. a cache per folder, keyed by file identity (resume);
2. grouping of similar footage, per clip and per stretch of a clip;
3. a folder run: several clips in flight, one shared rate-limit pause, a budget cap;
4. the CLI on top: `clipscribe footage/ --resume --groups --json`.

## 1. The cache

### Where

One file per folder, `.clipscribe-cache.jsonl`, next to the videos (the folder a video is in), so
moving or copying the footage folder carries its descriptions along and deleting that one hidden
file is the whole "forget it" story. `--cache-dir DIR` (library: `cache_path(folder, Some(dir))`)
puts the file in `DIR` instead, for a read-only folder (a camera card, a NAS share mounted
read-only) or to keep the footage folder untouched. With `--cache-dir`, every input shares the one
file there: entries are keyed by file identity, not by path (below), so one file for many folders
is not a conflict.

The cache is only read or written when asked: `--resume` (read and write) or `--force` (write
only). A plain run leaves no file behind, as today — a command that used to have no side effects
on the footage folder does not start writing hidden files into it unasked.

### The key: file identity

`FileIdentity { size, modified_ns, sample_hash }`:

- `size`: the file length in bytes.
- `modified_ns`: the modification time, nanoseconds since the Unix epoch (0 when the platform or
  file system cannot tell, and for times before 1970).
- `sample_hash`: FNV-1a 64 over **a sample of the content: three 64 KiB chunks — the first, the
  middle and the last 64 KiB** of the file (the whole file when it is 192 KiB or smaller, which
  covers the test clips' smaller siblings and any `.srt`). The first and last chunks hold the
  container's index wherever the muxer put it (an MP4/MOV `moov` box is at one end or the other;
  Matroska/WebM cues likewise), so a re-encode or a re-mux changes them even when the size happens
  to match; the middle chunk catches an in-place edit of the media data. Three reads per file,
  at most 192 KiB: for 5,000 clips about 1 GB read, seconds on an SSD, a couple of minutes of
  seeks on a spinning disk or a NAS — small next to describing even one clip (≈10 s each).

Why each part: size + mtime alone is what `make`/`rsync` trust, and it is cheap; but a copy made
with `cp -p` or restored from a backup can land with a different file's size+mtime, and some file
systems (FAT on camera cards: 2 s resolution) make mtime coarse. The content sample closes that
gap without hashing gigabytes.

**Why FNV-1a and not `std`'s hasher:** the cache outlives one build of the program, and
`std::collections::hash_map::DefaultHasher`'s algorithm "is not specified, and so it and its hashes
should not be relied upon over releases" ([std docs][defaulthasher]). FNV-1a 64 ([Fowler–Noll–Vo][fnv])
is ten lines, fixed forever, and needs no new dependency. It is not cryptographic — nobody attacks
their own cache — and it is only one of three parts of the key.

Keyed by identity rather than path, a renamed or moved clip (same bytes, same mtime) still hits
its entry; two identical copies share one (correctly: same content, same description).

### Settings

A cached description is reused only if it was made the same way. Each entry carries a settings
string, e.g. `model=claude-haiku-4-5 language=en frames=keyframes moments=important tags=none
subtitles=none`: the model id, the language, the frame sampling, the moments mode, the vocabulary
(`none`, or an FNV hash of every `name`/`hint`), and the subtitles (`off` with `--no-subtitles`,
`none` when there is no `.srt`, else the `.srt`'s own identity, so editing the subtitles redoes the
clip). A different string is a miss, and the new result replaces the old entry. The cache holds the
latest description of each file, not one per setting.

### Format and crash safety

JSON Lines: one self-contained JSON object per described clip, `"v": 1` first, then the identity,
the settings, the file name (informational only), model, duration, usage, summary, segments, tags
(or `null`) and the frame fingerprints grouping needs (time + 64 bytes base64 per sent frame,
≈ 5.5 KB per clip; 5,000 clips ≈ 27 MB).

- **Appending is the only write during a run.** A clip's line is written once its answer is in
  and parsed, with one `write_all` of the whole line plus `\n`, then `sync_data` (an fsync), under
  a mutex shared by every worker. Nothing is written for a clip in work, a failed clip or a
  cancelled one, so Ctrl+C or a crash loses at most the clips in flight — never an already
  finished one, never a half-written entry that later parses as valid.
- **A torn last line** (the process died mid-write) is detected on the next open: a line that does
  not parse is skipped. A file that does not end in `\n` is never appended to as-is (the next line
  would glue onto the torn one): opening such a file compacts it first (below).
- **Compaction on open**: when the file holds lines that do not parse, entries superseded by a
  later line for the same identity, or a torn tail, it is rewritten — the surviving lines to
  `<name>.tmp`, fsynced, then renamed over the original. `std::fs::rename` replaces the target
  atomically on POSIX and uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on Windows
  ([std docs][rename]); a crash during compaction leaves either the old or the new file, both valid.
- **Lines with a newer `"v"`** than this build knows are kept verbatim on compaction and otherwise
  ignored, so an older clipscribe never deletes a newer one's work.
- Two processes writing one cache at the same time is not supported (no file lock); each line is
  still written with a single append, so the worst case is a duplicate entry, which compaction
  removes.

`--force` skips every lookup but still appends every new result, so the cache is fresh for the
next `--resume` rather than left stale.

## 2. Grouping similar footage

### The options

| | Local CLIP-style embeddings | Claude compares the descriptions | Perceptual fingerprints |
|---|---|---|---|
| What it groups | Same subject or activity, semantically ("people cooking" across kitchens) | Whatever the text says alike; blind to what the text left out | Same look: same place, camera set-up, composition |
| New dependencies | A tensor runtime (`ort`, which by default downloads Microsoft's prebuilt ONNX Runtime at build time ([ort features][ort]), or `candle`) plus a model file: ViT-B/32's vision tower alone is 87.8 M parameters, the full model 338 MB ([model card][clipcard], [CLIP paper][clip]) — too big for crates.io, so fetched at run time | None | None: the 8×8 luma grid key frames already compute (`frames::fingerprint`) |
| Build / binary | ONNX Runtime on three CI platforms (Windows already needs a hand-installed GStreamer), a model download in tests | — | — |
| Speed | ~4.4 GFLOP per 224 px frame: tens of ms per frame on a CPU, 1–3 s per 60-frame clip; GPU optional | One large text request at the end: 1,000 clips ≈ 100 k input tokens (summary + segments ≈ 100 tokens each) and ≈ 60 k output tokens (a group id per clip and segment); output alone is minutes of generation | Microseconds per comparison; fingerprints come free with the frames already decoded |
| Money | None | 1,000 clips ≈ $0.10 in + $0.30 out with Haiku 4.5 — cheap, but paid again on every resumed run unless cached too; past ≈ 1,500 clips it no longer fits one request, and groups split across requests must be reconciled | None |
| Privacy | Fully local | Descriptions only, which Claude already wrote | Fully local |
| Deterministic, resumable | Yes (embeddings cacheable) | No: a rerun can regroup differently | Yes: fingerprints cached with each clip, grouping a pure function of them |
| Labels | Need text from elsewhere | Natural | From the existing descriptions |

### Recommendation: perceptual fingerprints, labels from the descriptions

Chosen. The issue asks for clips "that show the same scene or activity" in a folder of one
shoot — the case where a perceptual fingerprint is strongest: the same place filmed several times,
a camera left on a tripod, retakes, the same clip exported twice. It costs nothing, runs offline,
is deterministic (so a resumed run groups exactly as a full one would), and it extends machinery
the crate already has instead of adding the heaviest dependency it would ever carry: CLIP would
bring a tensor runtime, a 90–340 MB model download and new build steps on three platforms into a
crate that frename embeds, for semantics the descriptions already carry in words. Asking Claude
needs nothing new either, but it is the only option that costs money again on each run, is not
reproducible, stops fitting one request around 1,500 clips, and can only group what one sentence
per stretch happened to mention.

What it cannot do, stated plainly: group the same activity filmed in visibly different places
(two different kitchens), or follow a small subject moving over an empty background (its 8×8 grid
changes with the subject's position). That is where CLIP would win; if the owner wants it later, it
fits as an optional cargo feature behind the same `group_clips` output shape.

The 8×8 grid is a **block-mean-value hash**, one of the four perceptual image hashes benchmarked
by Zauner ([thesis][zauner]); the key-frame design (`docs/design/key-frames.md`) already uses it for
shot changes within one clip. Grouping extends it across clips with two changes, both measured on
the test clips (below):

- **Structure, not brightness.** Key frames compare raw bytes (mean absolute difference), which is
  dominated by overall brightness: every dark clip looks alike. Grouping compares the
  z-normalised grids (subtract the mean, divide by the spread) with Pearson correlation, distance
  `1 − r`: the layout of light and dark, independent of exposure.
- **Rotation.** The distance is the smallest of the four 90° rotations of one grid, so a phone clip
  stored sideways, or a portrait re-export of the same shot, matches its landscape original. (The
  grid is square, so rotating it is exact: the cells of a rotated frame are the rotated cells.)

### The algorithm

1. **Frames.** Each described clip keeps, for every frame sent to the model, its time and its 8×8
   fingerprint, computed on the upright frame. Cached with the description, so grouping a resumed
   run needs no decoding.
2. **Blank frames.** A grid whose spread (standard deviation of the 64 cells) is under 4 (of 255)
   has no structure to compare — black, a flat wall, fine noise or a fine pattern that averages
   out at 8×8. It never matches anything.
3. **Stretches.** A clip is split between two consecutive frames when both the structure changes
   (`1 − r` > 0.5, or one side is blank) and the level changes (mean absolute difference > 0.05).
   Requiring both keeps camera motion and a moving subject in one stretch and still cuts between
   two different scenes. The boundary is halfway between the two frames; the first stretch starts
   at 0, the last ends at the clip's duration.
4. **Stretch signature.** The mean of its non-blank frames' normalised grids, normalised again. A
   stretch of only blank frames has none.
5. **Groups.** Two stretches (in any clips, or the same clip) with signatures closer than 0.2 are
   the same scene; groups are the connected sets (union-find, i.e. single linkage). A stretch
   without a signature is a group of its own. Group ids count from 1 in order of first appearance
   (videos in input order, stretches in time order), so the same inputs always get the same ids.
6. **Labels.** The group's medoid (the stretch with the smallest total distance to the other
   members, the first on a tie) names it: the clip's description segment covering at least half of
   that stretch, else the clip's summary. No extra request.
7. **Per clip and per segment.** A clip's group is the one covering most of its duration; each
   description segment gets the group of the stretch overlapping it most.

Pairwise comparison is O(n²) over stretches with 256 multiply-adds per pair (four rotations × 64):
10,000 stretches ≈ 50 M pairs ≈ 13 G multiply-adds, seconds in a release build. Beyond that a
nearest-neighbour index would be the next step; not needed for "thousands of clips".

### Measured on the test clips

The four clips in `tests/clips/` are the same footage (a rotating Earth at night): the MP4, the
MOV and the WebM are one video in three containers, `rotated-90.mp4` its first 6 s with a 90°
rotation tag. Test patterns made with `gst-launch-1.0 videotestsrc` stand in for other scenes.
Median over frames of the best-matching frame's distance `1 − r`, smallest rotation:

| | distance |
|---|---|
| MP4 vs MOV / WebM | 0.002–0.003 |
| MP4 vs `rotated-90.mp4` | 0.06 (0.44 without rotation) |
| Earth vs `ball` (a white ball on black) | 0.59 |
| Earth vs `smpte` / `gradient` | 0.85–0.87 |
| `smpte` vs `gradient` (the closest pair of different patterns) | 0.36 |
| consecutive frames of the rotating Earth | 0.00–0.11 |

0.2 sits well between "same footage" (≤ 0.11) and "different scene" (≥ 0.36). Raw mean absolute
difference, as key frames use it, could not have told them apart: Earth vs `ball` is 0.067 there,
Earth vs its own rotation 0.032.

## 3. The folder run

### Concurrency and rate limits

`RunOptions::jobs` clips are in flight at once (default 4, `--jobs`): each worker thread reads a
clip's frames with its own GStreamer pipeline and sends its own request. Anthropic's standard
limits for Haiku 4.5 start at 1,000 requests and 2 M input tokens per minute ([rate limits][limits]);
a clip is ≈ 13 k input tokens and ≈ 10 s, so four in flight use about 5 % of that. New accounts
can sit in a lower "Evaluation" tier, and limits are enforced as a token bucket, so short bursts can
hit them anyway — which is what the shared pause is for.

**One pause for everyone.** `provider::retry_loop` already waits out a 429 (the `retry-after`
header, else 30 s) without spending a retry. With several clips in flight, each would otherwise
keep sending into the limit until it got its own 429. A folder run gives all its workers' clients
one shared `RateGate`: a 429 on any worker closes the gate for its `retry-after`, and every worker's
`retry_loop` waits for the gate to open before its next attempt. It is the same loop, with the same
cancel-aware `wait`, extended by one optional argument; a lone `describe` passes none and behaves
exactly as before. Retries with backoff for 5xx and lost connections are unchanged (2 s, 8 s, 30 s).

### The budget cap

`Budget` (`--max-cost USD`) tracks what this run has spent plus what the requests in flight could
still cost. Before a clip's request is sent — after its frames are read, so its size is known —
the request's **upper bound** is reserved: input tokens from the actual frames (their JPEG sizes,
the crate's own `frame_tokens`) and the prompt text (bytes / 3.5, an overestimate for English and
for Cyrillic alike), plus 10 %, and output at the request's `max_tokens`, the most it can be
billed for. If spent + reserved + bound would pass the cap, the request is not sent: the clip is
`OverBudget`, nothing is written to the cache for it, no new clip is started, and the clips already
in flight finish (their results are cached). When an answer comes back, the reservation is
replaced by its real cost; a failed request releases it; a timeout — the one failure the provider
may have billed without saying how much — keeps the whole bound as spent. So the total never
passes the cap; the price of that guarantee is stopping up to one clip's worth of bound early.

The cap counts only what the run itself spends: cached clips are free. A resumed run with the same
`--max-cost` gets the full amount again.

### Stopping

Ctrl+C (the `cancel` flag) stops at the next frame or wait, as before; an answer that arrives
anyway is still cached, since it was billed. `OverBudget`, and an error that would fail every
clip the same way (`AiError::stops_job`: rejected key, no credit, a spend limit), stop starting new
clips. `FolderRun::stopped` says which.

## 4. API

### Library

Always built (no GStreamer):

- `find_videos(inputs)`, `VIDEO_EXTENSIONS` — moved from the CLI: files as given, folders as the
  videos in them, sorted.
- `FileIdentity::of(path)`, `CacheKey::new(video, &options, vocabulary, subtitles)`,
  `cache_path(folder, cache_dir)`, `CACHE_FILE_NAME`, `Cache::open(path)` → `get(&key)`,
  `put(&record)`; `ClipRecord { file, key, model, clip: DescribedClip }`.
- `Budget::new(max_usd)` → `reserve(usd)` → `Reservation::settle(actual_usd)`; `spent_usd()`;
  `request_cost_bound(model, &request)`.
- `group_clips(&[&DescribedClip])` → `Grouping { groups: Vec<Group { id, label, stretches }>,
  clips: Vec<ClipGroups { group, stretches: Vec<Stretch { start_s, end_s, group }>, segments }> }`.

With `frames`:

- `describe_clip(video, subtitles, vocabulary, &options, &budget, cancel, on_stage)` →
  `Ok(Some(DescribedClip))`, `Ok(None)` when the budget would be exceeded (nothing sent), or the
  same `Error`s as `describe`. `DescribedClip { description, tags, usage, duration_s, frames }`.
- `describe_folder(videos, cache, &run, &options, &budget, cancel, on_event)` → `FolderRun { clips:
  Vec<ClipOutcome>, usage, stopped }`, with `ClipOutcome::{Described, Cached, Failed, OverBudget,
  NotStarted}` in input order and `FolderEvent::{Started, Stage, Finished, Warning}` for a progress
  display (called from worker threads, hence `Fn + Sync`).

frename can call `describe_folder` and show `FolderEvent`s, or build its own loop from
`CacheKey`/`Cache`/`describe_clip`/`Budget` and call `group_clips` on whatever it has.

**No breaking change.** Everything is new; `Options`, `Error`, `describe`, `describe_with_tags` and
`RetryPolicy` keep their shape. `retry_loop` (crate-private) gains the optional gate; `Anthropic`
and `OpenAi` gain a private field.

### Command line

`clipscribe footage/ --resume --groups --json` as the issue asks, plus:

- `--resume`: skip clips already in the folder's cache, add each new one to it.
- `--force`: describe every clip again, still writing the cache.
- `--cache-dir DIR`: keep the cache in `DIR`.
- `--groups`: group similar footage. Text: a `Groups:` section at the end, each group's label and
  its clips and time ranges. JSON: each clip object gains `group`, `group_label` and `stretches`
  (`start_s`, `end_s`, `group`, `label`), each moment a `group` — the top level stays an array, so
  existing scripts keep working.
- `--jobs N` (default 4), `--max-cost USD`.

Every whole-clip run now goes through `describe_folder`: results are printed in input order as
soon as every earlier clip is done, so output reads exactly as before; `--jobs 1` is the old
one-at-a-time run. The status line shows how many clips are done and in work. Cached clips print
like described ones (JSON: `"cached": true`, with the usage they cost when described); the usage
line at the end counts only what this run spent, and says how many clips came from the cache.

## Tests

- Cache: identity changes with size, mtime or the sampled content, and not with a rename; the
  sample of a small file is the whole file; round trip of a record through a line; settings
  mismatch is a miss; a torn last line and a corrupt line are skipped and compacted away, and the
  next append lands on its own line; newer-version lines survive compaction.
- Budget: reserve/settle/release arithmetic, refusal at the cap, timeouts kept as spent; the bound
  covers a real request's parts.
- Rate gate: a 429 on one thread holds back another thread's next attempt.
- Grouping: synthetic grids (a scene, its rotation, another scene, a blank clip, a clip with a
  cut) and, on Linux, the real test clips (one group) plus `videotestsrc` clips (their own groups).
- Folder run, on Linux with a mock HTTP server (the `TcpListener` pattern of the other tests):
  an interrupted run (cancelled after its first clip) resumes and sends only the rest; a third run
  sends nothing; `--force` sends everything again; a budget below one clip's bound sends nothing and
  caches nothing; two clips are really in flight at once with `jobs = 2`.

## Decisions made without the owner

- **Perceptual fingerprints for grouping**, labels from the existing descriptions — see
  "Recommendation" above. CLIP stays a possible optional feature later, not a default dependency.
- **The cache is opt-in** (`--resume`/`--force`), not written by every run: a plain run keeps
  having no side effects on the footage folder. Consequence: pass `--resume` from the first run
  on, or the first run's results are not there to resume from.
- **`--force` still writes the cache** (the issue says "redoes everything"; leaving the old entries
  in place would make the next `--resume` serve stale results).
- **Sample = first, middle and last 64 KiB** (whole file up to 192 KiB), FNV-1a 64.
- **Identity, not path, is the key**; a rename keeps its entry.
- **Settings are part of a hit**: a different model, language, frame sampling, moments mode,
  vocabulary or `.srt` redoes the clip; the newest result replaces the entry.
- **Failures are not cached**, not even a billed bad answer: the next run retries them.
- **The budget reserves an upper bound**, so the cap is never passed, at the price of stopping up
  to one clip early; it stops at the first clip that does not fit rather than hunting for smaller
  clips that might, so what is left is a clean tail for a resumed run.
- **`--jobs` defaults to 4**, and applies to every whole-clip run, not only cached ones; `--jobs 1`
  is the old behaviour.
- **One shared pause on a 429** across workers, not per-worker only; no pre-emptive pacing from the
  `anthropic-ratelimit-*` headers (OpenAI's differ, and the pause already stops the burst).
- **Stretches are visual** (cuts in the fingerprints), not the description's segments: in the
  default `Important` mode a clip can have no segments at all, and grouping must still work. Each
  segment is then mapped to the stretch it overlaps most.
- **Every stretch gets a group**, a singleton when nothing matches, so every clip and segment has
  an id; blank stretches are always singletons.
- **Single linkage** (connected components) rather than average linkage: simple, deterministic, and
  what "the same scene" means for near-duplicates; the known risk — a chain of gradually changing
  shots merging — is bounded by the strict 0.2 threshold.
- **JSON stays an array** of per-clip objects; group data is added to each clip, not a new top-level
  object.
- **Thresholds are fixed constants** (0.2 same scene, 0.5 + 0.05 cut, 4 blank), not options:
  measured above; retuning is a later issue with real footage in hand.

[defaulthasher]: https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html
[fnv]: http://www.isthe.com/chongo/tech/comp/fnv/
[rename]: https://doc.rust-lang.org/std/fs/fn.rename.html
[ort]: https://ort.pyke.io/setup/cargo-features
[clipcard]: https://huggingface.co/openai/clip-vit-base-patch32
[clip]: https://arxiv.org/abs/2103.00020
[zauner]: https://www.phash.org/docs/pubs/thesis_zauner.pdf
[limits]: https://platform.claude.com/docs/en/api/rate-limits
