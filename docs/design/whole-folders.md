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
- **A failed write** (a full disk part way through a line) is cut back to where the file ended
  before it (`set_len`), so the next line does not start glued to a fragment; if even that fails,
  the next line starts with a newline of its own. Either way a later, finished clip is never lost
  with the fragment when the file is next opened.
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

### Recommendation: fingerprints and the descriptions' words, together

Chosen: two free signals, either of which joins two stretches.

- **Perceptual fingerprints** find the same *shot*: the same clip exported or re-muxed twice,
  stored sideways, brighter or darker, and — by construction, though not yet measured on real
  footage — a camera that did not move (a tripod left in place, retakes from the same position).
  They do not find the same place once the framing changes: see "Measured" below.
- **The descriptions' words** find the same subject or activity said in much the same words,
  whatever the camera did: every clip grouping sees has already been described, so comparing its
  summary and moments costs nothing — no request, no model, no dependency, a few string
  operations per stretch. This is the option the first version of this design left out; the
  review pointed out it is the cheapest of all, and it covers exactly what the fingerprints
  cannot (on the test footage, a zoom or a pan of the same shot is joined by its words, not its
  pictures). It cannot replace the fingerprints: two exports of one clip can be described in
  different words, and a blank or wordless description says nothing.

Both are deterministic (a resumed run groups exactly as a full one would: the fingerprints and
descriptions are both in the cache) and extend what the crate already has instead of adding the
heaviest dependency it would ever carry: CLIP would bring a tensor runtime, a 90–340 MB model
download and new build steps on three platforms into a crate that frename embeds. Asking Claude
to group needs nothing new either, but it is the only option that costs money again on each run,
is not reproducible, and stops fitting one request around 1,500 clips.

What it cannot do, stated plainly:

- The word signal knows only what the descriptions say. It joins two clips when their
  descriptions share enough words, which also happens for **the same place or person with
  something else happening** (a hiker walking a ridge and a man biking the same ridge at sunset
  share most of their words) and for **different things described with the same generic words**
  (on the test patterns, a black-and-white pinwheel and black-and-white rings "around the
  centre": a real false positive, below). It misses the same activity described in different
  words ("chops onions" and "slicing an onion" barely make it). Whether a group is the same
  "scene or activity" is only as good as the descriptions' wording.
- Its stemming is crude (the first four letters of each word) and its stop words are English
  only; in the other description languages only the length rule drops function words.
- Groups are connected sets (single linkage): with the word signal, a chain of clips each close in
  words to the next can join clips that have little in common end to end. On a large folder of
  one shoot described in similar words (every clip a "hiker on a trail"), expect some large
  groups.
- Neither signal groups the same activity filmed in visibly different places *and* described in
  different words, or follows a small subject moving over an empty background. That is where
  CLIP would win; if the owner wants it later, it fits as an optional cargo feature behind the
  same `group_clips` output shape.

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

The words are compared as sets (the **Jaccard index**: shared words over all words), after
lower-casing, splitting at anything that is not a letter or digit, dropping words under four
letters (articles and prepositions in all six description languages) and a short English list
of longer function words and words about the footage itself ("camera", "shot", "scene", …), and
cutting each word to its first four letters — truncation stemming, language-blind: "goat" and
"goats", "hiker" and "hikes", "красной" and "красная" come out the same. The segment-merging
check of `MomentsMode::Important` (`merge_same_description`) compares whole descriptions for
equality after trimming and lower-casing; that is a different question (is this the same
sentence?), so it is left as it is rather than shared.

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
5. **Stretch words.** The words of the description segments that overlap this stretch more than
   any other, plus the clip's summary when the stretch is the whole clip or no segment falls in
   it. Fewer than 3 distinct words: nothing to compare.
6. **Groups.** Two stretches are joined when their signatures are closer than 0.2 (the same
   shot), or when they are in different clips and their words' Jaccard index is at least 0.25.
   Two stretches of one clip are never joined by words (they often carry the same summary). A
   stretch without a signature (blank) is a group of its own, whatever its words. Groups are the
   connected sets (union-find, i.e. single linkage). Group ids count from 1 in order of first
   appearance (videos in input order, stretches in time order), so the same inputs always get the
   same ids.
7. **Labels.** The group's medoid (the stretch with the smallest total distance to the other
   members, each pair's distance being the closer of its two signals, each over its threshold;
   the first on a tie) names it: the clip's description segment covering at least half of that
   stretch, else the clip's summary. No extra request.
8. **Per clip and per segment.** A clip's group is the one covering most of its duration; each
   description segment gets the group of the stretch overlapping it most.

Pairwise comparison is O(n²) over stretches: 256 multiply-adds for the pictures (four rotations ×
64) and a merge of two sorted lists of a dozen or two word ids for the words. 10,000 stretches ≈
50 M pairs, seconds in a release build. Beyond that a nearest-neighbour index would be the next
step; not needed for "thousands of clips".

### Measured on the test clips (provisional)

The thresholds are **provisional**: the pictures' is calibrated only on near-duplicates of one real
video and on synthetic patterns, the words' only on descriptions written for the tests in the
style the model writes them — **not on real model output**: no live key was available where this
was developed, and the mock server answers every clip with the same sentence. There is no real
hard positive (the same place re-shot, a moved tripod, a retake) or hard negative (two different
real scenes from one shoot) among the fixtures, so neither threshold is validated on either;
retuning them is a later issue with real footage and its real descriptions in hand.

The four clips in `tests/clips/` are the same footage (a rotating Earth at night): the MP4, the
MOV and the WebM are one video in three containers, `rotated-90.mp4` its first 6 s with a 90°
rotation tag. The closest thing to a hard positive the fixtures allow is that footage *re-framed*:
its first 6 s cropped and scaled back up, as a zoomed or panned camera would see it. Different
scenes are `videotestsrc` patterns. All of these are made in-process with GStreamer by the test
`grouping_distances_on_the_test_clips` (`src/folder.rs`), which gives each a description in
different words (as two requests would) and prints this table:

```sh
cargo test --lib grouping_distances -- --nocapture
```

The pictures' distance is the one `group_clips` joins by: `1 − r` between stretch signatures,
best of the four rotations, closest pair of stretches. The words' is the Jaccard index.

| From the MP4 to | pictures | words | grouped by |
|---|---|---|---|
| MOV / WebM (same video, other container) | 0.000–0.001 | 0.31–0.38 | both |
| `rotated-90.mp4` | 0.022 | 0.64 | both |
| itself zoomed 1.25× (centre crop) | 0.355 | 0.58 | **words** |
| itself panned 20 % (left crop) | 0.598 | 0.73 | **words** |
| itself panned 40 % | 0.702 | 0.50 | **words** |
| `smpte`, `gradient`, `circular` | 0.84–0.89 | 0.00–0.07 | no |
| `ball`, `pinwheel` | 0.95–0.98 | 0.00 | no |
| **Between patterns** | | | |
| `smpte` vs `gradient` (the closest pictures) | 0.359 | 0.00 | no |
| `pinwheel` vs `circular` ("black and white … the centre") | 0.991 | 0.33 | **words: a false positive** |
| every other pair | 0.59–1.01 | 0.00–0.22 | no |

So the pictures' 0.2 sits between "the same shot" (≤ 0.02) and "anything else" (≥ 0.36), and
re-framing the same scene counts as "anything else" there; the words bring the re-framed footage
back. Raw mean absolute difference, as key frames use it, could not have told the shots apart:
Earth vs `ball` is 0.067 there, Earth vs its own rotation 0.032.

The words' threshold comes from `text_similarity_of_descriptions` (`src/groups.rs`, `cargo test
--lib text_similarity -- --nocapture`): 19 descriptions of a day's shoot — five subjects each
described twice as a re-shoot would be (another position or zoom), one pair in Russian, and seven
other clips of the same shoot, some at the same place as a subject.

| Pairs | Jaccard | at 0.25 |
|---|---|---|
| The same subject filmed again (6 pairs) | 0.27–0.50 (onion 0.27, tent 0.29, waves 0.30, hiker 0.31, goats 0.43, Russian hiker 0.50) | all joined |
| The same place or person, something else happening (5 pairs) | 0.21–0.50 (hiker walking vs biking the same ridge 0.50) | 3 of 5 joined |
| Different subjects (160 pairs) | 0.00–0.18 (the closest: the same hiker drinking from a stream) | none joined |

0.25 sits in the gap between 0.18 and 0.27, nearer the re-shoots, to keep joins of different
subjects rare; the margin is thin on both sides, which is why it is provisional.
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
the request's **upper bound** is reserved: input tokens from the actual frames (their JPEG sizes;
Anthropic's 28 px tiles, or for OpenAI the larger of its two published schemes, 85 + 170 per 512 px
tile and 1.62 per 32 px patch) and the prompt text (bytes / 3.5, an overestimate for English and
for Cyrillic alike), plus 10 %, and output at the request's `max_tokens`, the most it can be
billed for. When an answer comes back, the reservation is replaced by its real cost; a failed
request releases it; a timeout, or an answer that came back but could not be read at all
(`AiError::BadAnswer`: the two failures the provider may have billed without the error saying how
much) keeps the whole bound as spent — a conservative estimate, since `AiError` carries no usage
and changing that would change a public type.

**Waiting for the others.** The bound is typically four or five times what a clip really costs
(the answer is counted at 4,000 or 16,000 tokens and is usually a few hundred). So with several
clips in flight, a clip's bound often does not fit next to the other clips' *reservations* even
though it fits easily next to what they will really cost. Such a clip waits
(`Budget::reserve_or_wait`: a condition variable signalled whenever a reservation settles or is
released, checking the cancel flag every 50 ms) and tries again. Only when spent + its bound
would pass the cap *with nothing else in flight* is it `OverBudget`: nothing is sent, nothing is
cached for it, the run stops describing new clips, and the clips already in flight finish (their
results are cached). No clip ever waits while holding a reservation of its own, so a waiting
clip always has someone to wait for.

What that guarantees: the reservations in flight never add up past the cap, so what the run is
billed for stays under it as long as every request costs at most its bound and is billed once.
The run stops early by at most one bound: the last clip is refused when what is spent plus its
bound would pass the cap, whatever `--jobs` is. (In review of the first version, which refused instead of waiting, with `--jobs 4`
the first clip's open reservation turned the other three away at once: `--max-cost 0.03` on
`tests/clips/`, estimated at $0.025 in all, described one video of four.)

What it does not guarantee:

- **OpenAI's image billing is taken from its published formulas, not measured** (OpenAI is not
  reachable from where this crate is developed; see `docs/design/openai-provider.md`). The
  answer's slack covers a small error there, but it is not a proven bound.
- **A retried attempt may have been billed.** A lost connection *after* the request was sent, or
  a 5xx, is retried (as it always was for a single `describe`); if the provider billed that first
  attempt, the run never learns of it, and only the last attempt's usage is counted. A timeout,
  the common case, and an unreadable answer are not retried and are counted at their bound.

The cap counts only what the run itself spends: cached clips are free. A resumed run with the same
`--max-cost` gets the full amount again.

### Stopping

Ctrl+C (the `cancel` flag) stops at the next frame, the next wait (for a rate limit, a retry or
the budget), or just before a request is sent — the last moment the money can still be saved. A
request already sent is not interrupted: its answer is waited for, cached and counted, since it
was billed. Nothing new is taken up after a cancel.

`OverBudget`, and an error that would fail every clip the same way (`AiError::stops_job`: rejected
key, no credit, a spend limit), stop describing new clips. The clips left are still looked up in
the cache (`cached_clip`) and served from it when it has them — they cost nothing and are already
on disk, so a `--resume --max-cost` run that hits the cap still prints and groups every clip
described before; only the clips that would need a request are `NotStarted`. `FolderRun::stopped`
says why the run stopped. That policy lives in one function, `serve_after_stop`, which
`describe_folder` calls for the clips it did not take up and the CLI calls for the folders after
the one that stopped.

## 4. API

### Library

Always built (no GStreamer):

- `find_videos(inputs)`, `VIDEO_EXTENSIONS` — moved from the CLI: files as given, folders as the
  videos in them, sorted.
- `FileIdentity::of(path)`, `CacheKey::new(video, &options, vocabulary, subtitles)`,
  `cache_path(folder, cache_dir)`, `CACHE_FILE_NAME`, `Cache::open(path)` → `get(&key)`,
  `put(&record)`; `ClipRecord { file, key, model, clip: DescribedClip }`.
- `Budget::new(max_usd)` → `reserve(usd)` (never waits) or `reserve_or_wait(usd, cancel)` →
  `Reserved::{Yes(Reservation), OverBudget, Cancelled}`; `Reservation::settle(actual_usd)`;
  `spent_usd()`, `max_usd()`; `request_cost_bound(model, &request)`.
- `cached_clip(video, &cache, &run, &options)`: what the cache has for a video, nothing sent.
- `serve_after_stop(videos, cache, &run, &options, &stop, on_event)`: the clips a stopped run did
  not take up, served from the cache (or `NotStarted`; nothing at all after a cancel).
- `Budget::refused_usd()`: the bound of the last request found over budget, for a message.
- `group_clips(&[&DescribedClip])` → `Grouping { groups: Vec<Group { id, label, stretches }>,
  clips: Vec<ClipGroups { group, stretches: Vec<Stretch { start_s, end_s, group }>, segments }> }`.

With `frames`:

- `describe_clip(video, subtitles, vocabulary, &options, &budget, cancel, on_stage)` →
  `Ok(Some(DescribedClip))`, `Ok(None)` when the budget would be exceeded (nothing sent), or the
  same `Error`s as `describe`. `DescribedClip { description, tags, usage, duration_s, frames }`.
  `describe` and `describe_with_tags` are `describe_clip` with a budget without a cap: there is
  one place that builds a clip's request, sends it and reads the answer.
- `describe_folder(videos, cache, &run, &options, &budget, cancel, on_event)` → `FolderRun { clips:
  Vec<ClipOutcome>, usage, stopped }`, with `ClipOutcome::{Described, Cached, Failed, OverBudget,
  NotStarted}` in input order and `FolderEvent::{Started, Stage, Finished, Warning}` for a progress
  display (called from worker threads, hence `Fn + Sync`). After a budget or job stop, `Finished`
  comes for every clip left too (`Cached` or `NotStarted`), in order.

frename can call `describe_folder` and show `FolderEvent`s, or build its own loop from
`CacheKey`/`Cache`/`describe_clip`/`Budget` and call `group_clips` on whatever it has.

The crate root re-exports these by name (not `pub use module::*`), so a new public item in these
modules does not become part of the API unnoticed.

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
like described ones (JSON: `"cached": true`); their `usage` and `cost_usd` are what they cost when
they were described, not what this run spent — a script summing `cost_usd` over a resumed run's
JSON overstates the run. The usage line on stderr counts only what this run spent, and says how
many clips came from the cache.

- With `--resume`/`--force` and no `--cache-dir`, inputs from several folders run as one
  `describe_folder` per folder (each stretch of consecutive videos in one folder, with that
  folder's cache), one after the other in input order, sharing one `Budget`. A budget or job stop
  in one leaves the rest to their caches: cached clips are served, the others `NotStarted`; a
  cancel leaves them all `NotStarted`. Without a cache, or with `--cache-dir`, it is a single
  `describe_folder`. Every cache is opened before anything is sent.
- `--resume` and `--force` exclude each other; `--cache-dir` needs one of them. `--at` and
  `--estimate` take none of the folder-run options.
- A budget stop says the cap, what was spent, why it stopped short of the cap, and what to do:
  `Stopped at --max-cost $0.03: 2 of 4 videos not described. $0.0143 spent; the next video could
  cost up to $0.0231 (its answer counted at full length, though it usually costs a fraction of
  that), which would pass the cap. Each video needs that much room before it is sent, so a cap
  close to --estimate's total can stop a video or two early. Raise --max-cost and run again with
  --resume to continue.` Without `--resume` or `--force` nothing was saved, and the message says
  so: the videos described in this run were not saved, so running again pays for them again
  too. `--max-cost`'s help says the same about a cap close to `--estimate`'s total.
- The exit code is 1 whenever a video was not described (failed, over budget, not started,
  cancelled), as a failure was before.

## Tests

- Cache: identity changes with size, mtime or the sampled content, and not with a rename; the
  sample of a small file is the whole file; round trip of a record through a line; settings
  mismatch is a miss; a torn last line and a corrupt line are skipped and compacted away, and the
  next append lands on its own line; an entry written after a failed write is not glued to its
  fragment; newer-version lines survive compaction.
- Budget: reserve/settle/release arithmetic, refusal at the cap, timeouts kept as spent; a
  reservation waits for another in flight and goes ahead when it settles, is over budget when it
  could not fit even alone, and stops waiting on a cancel; the bound covers a real request's
  parts, and OpenAI's image tokens.
- Rate gate: a 429 on one thread holds back another thread's next attempt (synchronised on the
  gate's state, not a sleep).
- Grouping: synthetic grids (a scene, its rotation, another scene, a blank clip, a clip with a
  cut); clips whose pictures differ joined by their descriptions, never two stretches of one clip
  or a blank one; the word table above; and, on Linux, the real test clips (one group) plus
  `videotestsrc` clips made in-process (their own groups — this half always runs, no
  `gst-launch-1.0` needed), and the distance table above.
- An unreadable answer counts its bound as spent; after a stop, the clips left are finished in
  order (none after a cancel).
- Folder run, on Linux with a mock HTTP server (the `TcpListener` pattern of the other tests):
  an interrupted run (cancelled after its first clip) resumes and sends only the rest; a third run
  sends nothing; `--force` sends everything again; a budget below one clip's bound sends nothing and
  caches nothing, and one with room for exactly one clip (derived from the real bounds, not
  hand-computed) describes one; with `jobs = 3` and room for only one bound at a time, every clip
  waits its turn and all are described within the cap; a run stopped by the budget still serves
  what the cache has; an answer in flight when Ctrl+C comes is kept; two clips are really in
  flight at once with `jobs = 2`.

## Decisions made without the owner

- **Perceptual fingerprints and the descriptions' words for grouping**, either one joining,
  labels from the existing descriptions — see "Recommendation" above. CLIP stays a possible
  optional feature later, not a default dependency.
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
- **The budget reserves an upper bound**, so the cap is not passed (within the limits under "The
  budget cap"), at the price of stopping up to one clip's bound early. A clip that does not fit
  only because of other reservations in flight **waits** for them rather than stopping the run.
  It stops at the first clip that does not fit even alone rather than hunting for smaller clips
  that might, so what is left is a clean tail for a resumed run.
- **After a stop, cached clips are still served** (not after a cancel: Ctrl+C means stop now).
- **`--jobs` defaults to 4**, and applies to every whole-clip run, not only cached ones; `--jobs 1`
  is the old behaviour.
- **One shared pause on a 429** across workers, not per-worker only; no pre-emptive pacing from the
  `anthropic-ratelimit-*` headers (OpenAI's differ, and the pause already stops the burst).
- **Stretches are visual** (cuts in the fingerprints), not the description's segments: in the
  default `Important` mode a clip can have no segments at all, and grouping must still work. Each
  segment is then mapped to the stretch it overlaps most.
- **Every stretch gets a group**, a singleton when nothing matches, so every clip and segment has
  an id; blank stretches are always singletons.
- **Single linkage** (connected components) rather than average linkage: simple and deterministic.
  The known risk — a chain of gradually changing shots, or of clips each described much like the
  next, merging — is bounded for the pictures by the strict 0.2 threshold; for the words it is
  real on a large folder described in similar words (see "What it cannot do").
- **JSON stays an array** of per-clip objects; group data is added to each clip, not a new top-level
  object.
- **Thresholds are fixed constants** (0.2 same shot, 0.25 shared words with at least 3 words,
  0.5 + 0.05 cut, 4 blank), not options, and provisional: measured above on near-duplicates,
  re-framings, synthetic patterns and descriptions written for the tests only; retuning is a
  later issue with real footage and real descriptions in hand.
- **Words never join two stretches of one clip, nor a blank stretch**: stretches of one clip
  often carry the same summary, and a blank stretch's words ("a black screen") would gather every
  black leader of a folder into one group.

[defaulthasher]: https://doc.rust-lang.org/std/collections/hash_map/struct.DefaultHasher.html
[fnv]: http://www.isthe.com/chongo/tech/comp/fnv/
[rename]: https://doc.rust-lang.org/std/fs/fn.rename.html
[ort]: https://ort.pyke.io/setup/cargo-features
[clipcard]: https://huggingface.co/openai/clip-vit-base-patch32
[clip]: https://arxiv.org/abs/2103.00020
[zauner]: https://www.phash.org/docs/pubs/thesis_zauner.pdf
[limits]: https://platform.claude.com/docs/en/api/rate-limits
