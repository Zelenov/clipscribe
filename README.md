# clipscribe

Describe what happens in video clips, and when, with Claude or ChatGPT — a Rust crate
([frename](https://github.com/Zelenov/frename) uses it).

A video file and its subtitles go in; a one-sentence summary and time-ranged key moments come
out, even for clips with no speech.

## How it works

- Frames are read with GStreamer, at most 60 per clip, 512 px on the long side, turned upright if
  the clip has a rotation tag, and encoded as JPEG in memory. Nothing is written to disk. By
  default they are **key frames** (`FrameSampling::KeyFrames`): the clip is split into as many
  equal windows as the frame budget, and the one frame kept from each is wherever the picture
  changes the most in it, so a static shot spends no more of the budget than a clip that keeps
  cutting to something new. `FrameSampling::Interval` goes back to one frame every 2 s, spread
  evenly on a longer clip.
- The `.srt` next to the video (`clip.mp4` → `clip.srt`), if there is one, goes along, so the
  description knows what is said.
- One request goes to Claude (Anthropic) or ChatGPT (OpenAI); the answer is structured JSON, and
  moments outside the clip are dropped. Clips over 30 minutes are refused.
- Moments (segments) default to **important** (`MomentsMode::Important`): none at all for a
  static or uniform clip (the summary already covers it), one per clearly different or standout
  part otherwise — never a moment that just tiles the timeline. `MomentsMode::Full` goes back to
  always covering the whole clip in consecutive stretches.

## As a library

```toml
[dependencies]
clipscribe = { version = "0.1", default-features = false, features = ["frames"] }
```

```rust
use std::sync::atomic::AtomicBool;
use clipscribe::{describe, srt, FrameSampling, MomentsMode, Options, SummaryLanguage, MODELS};

let video = std::path::Path::new("clip.mp4");
let options = Options {
    api_key: std::env::var("ANTHROPIC_API_KEY")?,
    model: MODELS[0],
    language: SummaryLanguage::English,
    frame_sampling: FrameSampling::KeyFrames,
    moments: MomentsMode::Important,
};
let subtitles = srt::load_for(video)?;
let described = describe(video, &subtitles, &options, &AtomicBool::new(false), |stage| {
    eprintln!("{stage:?}"); // Frame { done, total }, then Asking
})?;
println!("{}", described.description.summary);
for moment in &described.description.segments {
    println!("{:.0}–{:.0} s: {}", moment.start_s, moment.end_s, moment.description);
}
println!("${:.4}", options.model.cost_usd(described.usage));
```

Everything blocks: call it from a worker thread. `describe` fails with `Error::Cancelled`,
`Unreadable`, `TooLong`, `Ai(AiError)` (rejected key, no credit, limits, network, timeout) or
`BadAnswer` (billed, but not usable). `estimate_usage` and `Model::cost_usd` price a clip
before sending it; `frames::clip_duration_s` reads its length.

Every entry of `MODELS` carries its `Provider` (`Anthropic` or `OpenAi`); `options.api_key` is
read against whichever provider `options.model` belongs to, so switching to a GPT model is just
picking a different `MODELS` entry and an OpenAI key — nothing else about the call changes.

### Tag suggestions

`describe_with_tags(video, subtitles, vocabulary, &options, ...)` describes a clip and suggests
tags from a `&[Tag]` vocabulary in one request, returning a `DescribedWithTags`. A vocabulary is a
closed list — one tag per line in a `name — hint` file, read with `parse_vocabulary` — instead of
the model inventing tags freely: each suggestion gets a confidence and, when it does not apply to
the whole clip, the time ranges where it does; a tag is never suggested outside the vocabulary,
but the model may add short, unscored `new_tag_ideas` for anything worth tagging that the
vocabulary does not cover.

`suggest_tags(description, duration_s, subtitles, vocabulary, &options, cancel)` tags a clip
already described earlier instead, from its `Description` alone — cheaper, no video read, works
without the `frames` feature — at the cost of not seeing anything the description itself left
out. `estimate_tags_usage` prices either.

### One moment

`describe_moment(video, at_s, window_s, subtitles, &options, cancel)` names and describes the
frame at `at_s`, for a marker there, instead of the whole clip: a fast, cheap, single request
reading only the frame plus `window_s` on each side of it and any subtitle lines that overlap that
same window. `MOMENT_WINDOW_S` (1 s) is a reasonable default for `window_s`. Returns a
`DescribedMoment { moment: Moment { name, description }, usage }` — `moment.name` is a few words,
fit for a marker label; `usage` prices with `Model::cost_usd` like any other call.

### Whole folders

`describe_folder` describes many clips in one call, several at once, and can resume a stopped
run and stop at a budget:

```rust
use clipscribe::{
    cache_path, describe_folder, find_videos, group_clips, Budget, Cache, DescribedClip, RunOptions,
};

let videos = find_videos(&["footage".into()])?; // the videos in the folder, sorted
let cache = Cache::open(&cache_path("footage".as_ref(), None))?;
let budget = Budget::new(Some(5.0)); // US dollars
let run = describe_folder(&videos, Some(&cache), &RunOptions::default(), &options, &budget,
    &AtomicBool::new(false), |event| eprintln!("{event:?}"));
// Only the described (or cached) clips can be grouped: keep each one's index into `videos`.
let described: Vec<(usize, &DescribedClip)> = run.clips.iter().enumerate()
    .filter_map(|(i, c)| c.record().map(|r| (i, &r.clip)))
    .collect();
let grouping = group_clips(&described.iter().map(|(_, clip)| *clip).collect::<Vec<_>>());
for ((i, _), groups) in described.iter().zip(&grouping.clips) {
    println!("{}: group {}", videos[*i].display(), groups.group);
}
```

`grouping.clips[n]` belongs to the `n`th clip passed to `group_clips`, which is not `videos[n]`
as soon as one video failed or was not started: keep the index, as above.

- **Resume.** The `Cache` is a `.clipscribe-cache.jsonl` file next to the videos (or in a
  directory you pass to `cache_path`). A clip is found by its `FileIdentity` (size, modification
  time and a hash of its first, middle and last 64 KiB) and the settings it was described with,
  so a renamed clip is still found and a different model or `.srt` describes it again. Each clip
  is written as soon as it is done: a crash or a cancel loses only the clips in work.
  `RunOptions::force` describes everything again.
- **Several at once.** `RunOptions::jobs` clips (default 4) are in flight; a rate limit on one
  pauses all of them.
- **Budget.** Before each request, `Budget` sets aside the most it can cost (the answer counted at
  its longest, so several times what it usually costs). A request that does not fit only because
  of the others in flight waits for them to settle; one that could pass the cap even alone is not
  sent, the clip comes back `ClipOutcome::OverBudget`, and no new clip is described — the clips
  left are still served from the cache when it has them (`serve_after_stop`, also for a loop over
  several folders), and `Budget::refused_usd` says what the refused clip could have cost. A
  timeout or an unreadable answer counts as its whole bound, since it may have been billed. The
  cap holds as long as each request is billed at most once and within its bound; OpenAI's image
  billing is taken from its published formulas, and a request retried after a lost connection
  may, rarely, have been billed twice.
- **Outcomes.** `FolderRun::clips` has a `ClipOutcome` per video, in order (`Described`, `Cached`,
  `Failed`, `OverBudget`, `NotStarted`), with `usage` (what this run spent) and `stopped` (why it
  stopped early, if it did). `FolderEvent`s arrive from the worker threads, for a progress display.
  A `Cached` clip's `usage` is what it cost when it was first described, not part of this run's.
- **Grouping.** `group_clips` puts clips, and stretches within them, in one `Group` when they look
  like the same shot or their descriptions say much the same thing, with a label taken from the
  descriptions. The pictures (8×8 brightness grids of the frames already sent, whatever the
  exposure, turned sideways too) find duplicates, re-exports, a clip stored sideways and a
  camera that did not move. The descriptions' words (shared words over all words, common words
  dropped) find the same subject or activity filmed again from elsewhere or zoomed, as long as
  the descriptions say it in similar words — and they also join the same place with something
  else happening, or different things described with the same everyday words ("a woman in a
  bright kitchen", "a woman in a bright office"). On a large folder from one shoot described in
  similar words, those links can chain several groups into a few large ones: check groups before
  relying on them. Both cost nothing, run offline and give the same groups every time. Both
  thresholds are provisional: measured on near-duplicates, synthetic patterns and descriptions
  written for the tests, not yet on real retakes and real descriptions.

To build your own loop instead, the parts are public: `CacheKey`, `Cache::get`/`put`,
`cached_clip`, `describe_clip` (one clip, within a `Budget`), `Budget::reserve_or_wait` and
`request_cost_bound`.

### Features

| Feature | |
|---|---|
| `cli` (default) | The `clipscribe` binary (clap, ctrlc). Implies `frames`. |
| `frames` (default) | GStreamer frame reading, `describe` and `describe_folder`. |
| none | Models, languages, request and answer types, the estimate, `srt`, both providers' clients, the cache and `group_clips`, with no GStreamer. |

## Building

GStreamer's runtime and development files are needed for the `frames` feature.

- **Linux:** `sudo apt-get install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav`
- **Windows:** the official MSVC package from gstreamer.freedesktop.org, or
  `scripts/install-gstreamer.ps1` (what CI uses); `PKG_CONFIG_PATH` must point at its
  `lib\pkgconfig` and its `bin` must be on `PATH`.
- **macOS:** `brew install gstreamer`.

```sh
cargo build --release
cargo test    # the frame tests decode tests/clips on Linux
```

`CLIPSCRIBE_LIVE_API_KEY` makes `cargo test` send one real request (about $0.01).

## Releasing

Bump the version in `Cargo.toml` and put it as the first heading of `version.md`, with the
changes under it. A push that changes `version.md` runs `.github/workflows/release.yml`: on
`main` it publishes the crate to crates.io (the `CARGO_REGISTRY_TOKEN` secret) and a GitHub
release `vX.Y.Z` with the three builds; on any other branch, a draft release
`vX.Y.Z-<branch>` and a crates.io dry run.

## License

MIT
