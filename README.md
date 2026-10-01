# clipscribe

Describe what happens in video clips, and when, with Claude or ChatGPT — a Rust crate
([frename](https://github.com/Zelenov/frename) uses it).

A video file and its subtitles go in; a one-sentence summary and time-ranged key moments come
out, even for clips with no speech.

## How it works

- Frames are read with GStreamer, at most 60 per clip, 512 px on the long side, turned upright if
  the clip has a rotation tag, and encoded as JPEG in memory. Nothing is written to disk unless you ask (see *Seeing the frames*). By
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
  always covering the whole clip in consecutive stretches. In `important` mode, ranges that
  together cover almost the whole clip are dropped unless the model says the clip is made of
  clearly different parts, a range that only repeats the summary is dropped, and so are ranges
  that only fill in around the main range or cut it into parts.
- A clip with a lead-in or lead-out gets a **main range** (`Description::main`, `"main"` in
  `--json`, `Main:` in the text output): the part an editor would keep, as a suggested In/Out.
  It is absent when the whole clip is usable, and never set in `full` mode.

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

### Writing files

`export` turns a description into files, so an editor or a website can use it without another
request. `Export::new(video, &described, model)` (or `Export::with_tags` for `describe_with_tags`)
wraps a result; `export::render(&export, Format::Srt)` gives one format as a `String`, and
`export::write_all(&export, &[Format::Json, Format::Srt], out_dir, force)` writes several, as
`<video>.clipscribe.<ext>` next to the video or in `out_dir`, leaving an existing file alone
unless `force`. The formats are `Json`, `Markdown`, `Text`, `Srt` and `Vtt` (moments as cues),
`Csv` (one row per moment), `Chapters` (YouTube-style `0:00 Title` lines) and `Xmp` (a Premiere Pro
sidecar, `<video>.xmp`: moments as Comment markers, the main range as an InOut marker). Nothing
is sent anywhere and the output is deterministic.

### Seeing the frames

To check which images the model got, call `set_debug_frames_dir(Some(dir))` (or set
`CLIPSCRIBE_DEBUG_FRAMES=<dir>`). `describe`, `describe_with_tags` and `describe_moment` then write
`<dir>/<video name>/`: every frame as the same JPEG bytes that were sent, named by time
(`0012.40s.jpg`), and a `frames.json` with each frame's time, how it was picked (`key_frame`,
`interval` or `moment`) and its size. A setter and a variable, not an `Options` field, so nothing
that builds `Options` changes. A write error is only logged. `dump_clip_frames` writes the frames
without sending anything.

### Features

| Feature | |
|---|---|
| `cli` (default) | The `clipscribe` binary (clap, ctrlc). Implies `frames`. |
| `frames` (default) | GStreamer frame reading and `describe`. |
| none | Models, languages, request and answer types, the estimate, `srt` and both providers' clients, with no GStreamer. |

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
