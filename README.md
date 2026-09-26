# clipscribe

Describe what happens in video clips, and when, with Claude.

A video file and its subtitles go in; a one-sentence summary and time-ranged key moments come
out, even for clips with no speech. Use it from the command line, or as a Rust crate
([frename](https://github.com/Zelenov/frename) does). For example:

```
$ clipscribe market.mp4
market.mp4  1:24 · 42 frames · $0.0112
  A guide leads two tourists through a spice market and explains the prices.
  0:00–0:14  Walking in through the market entrance, crowded, handheld camera.
  0:14–0:41  Close-ups of spice sacks while the guide names each one.
  0:41–1:24  The tourists buy saffron; the vendor weighs it on a scale.
```

## Download

Get the build for your system from [Releases](https://github.com/Zelenov/clipscribe/releases/latest):

| System | File |
|---|---|
| Windows x64 | `clipscribe-windows-x64-vX.Y.Z.zip` |
| Linux x64 | `clipscribe-linux-x64-vX.Y.Z.tar.gz` |
| macOS (Apple Silicon) | `clipscribe-macos-arm64-vX.Y.Z.tar.gz` |

Unpack it and put `clipscribe` / `clipscribe.exe` anywhere on `PATH`. GStreamer is not inside:
install its runtime (see [Building](#building); on Windows the official MSVC *runtime* package,
with its `bin` on `PATH`). Or build it with `cargo install clipscribe`.

## How it works

- Frames are read with GStreamer: one every 2 s, at most 60 per clip (a longer clip is sampled
  evenly), 512 px on the long side, turned upright if the clip has a rotation tag, and encoded
  as JPEG in memory. Nothing is written to disk.
- The `.srt` next to the video (`clip.mp4` → `clip.srt`), if there is one, goes along, so the
  description knows what is said.
- One request goes to the Anthropic Messages API; the answer is structured JSON, and moments
  outside the clip are dropped. Clips over 30 minutes are refused.

## Command line

You need an [Anthropic API key](https://console.anthropic.com/) and GStreamer (see
[Building](#building)).

```sh
export ANTHROPIC_API_KEY=sk-ant-...
clipscribe clip.mp4 other.mov          # describe
clipscribe footage/ --json > out.json  # every video in a folder, as JSON
clipscribe footage/ --estimate         # what it would cost; nothing is sent
```

| Option | |
|---|---|
| `--model haiku\|sonnet\|opus` | Claude Haiku 4.5 (default; about $10 per 1000 one-minute clips), Sonnet 5 or Opus 5 (notice more, cost more). |
| `--language` | `subtitles` (default: the subtitles' language, English if none), `en`, `ru`, `uk`, `de`, `es`, `fr`. |
| `--no-subtitles` | Do not send the `.srt`. |
| `--json` | One JSON array: `file`, `duration_s`, `frames`, `summary`, `moments[{start_s, end_s, description}]`, `model`, `usage`, `cost_usd`. |
| `--estimate` | Price the videos from their lengths only. |
| `--api-key` | Instead of `ANTHROPIC_API_KEY`. |

Progress goes to stderr, results to stdout, and the tokens and cost of the run to stderr at the
end. Ctrl+C stops the video in work. A rejected key or an empty balance stops the batch. The exit
code is 1 when a video failed.

## As a library

```toml
[dependencies]
clipscribe = { version = "0.1", default-features = false, features = ["frames"] }
```

```rust
use std::sync::atomic::AtomicBool;
use clipscribe::{describe, srt, Options, SummaryLanguage, MODELS};

let video = std::path::Path::new("clip.mp4");
let options = Options {
    api_key: std::env::var("ANTHROPIC_API_KEY")?,
    model: MODELS[0],
    language: SummaryLanguage::English,
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

### Features

| Feature | |
|---|---|
| `cli` (default) | The `clipscribe` binary (clap, ctrlc). Implies `frames`. |
| `frames` (default) | GStreamer frame reading and `describe`. |
| none | Models, languages, request and answer types, the estimate, `srt` and the Anthropic client, with no GStreamer. |

## Building

GStreamer's runtime and development files are needed for `frames` (and so for the command line).

- **Linux:** `sudo apt-get install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav`
- **Windows:** the official MSVC package from gstreamer.freedesktop.org, or
  `scripts/install-gstreamer.ps1` (what CI uses); `PKG_CONFIG_PATH` must point at its
  `lib\pkgconfig` and its `bin` must be on `PATH`.
- **macOS:** `brew install gstreamer`.

```sh
cargo build --release   # target/release/clipscribe
cargo test              # the frame tests decode tests/clips on Linux
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
