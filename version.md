# 0.1.0
## Added
- Describe what happens in a video clip, and when, with Claude: frames (one every 2 s, at most 60) and the clip's `.srt` go in; a one-sentence summary and time-ranged key moments come out, with the tokens billed.
- Models Claude Haiku 4.5 (default), Sonnet 5 and Opus 5, with their prices, a cost estimate before sending, and the language of the descriptions.
- `clipscribe` command line (the `cli` feature, on by default): videos or folders in, text or JSON out, `--estimate` to price a batch without sending anything, Ctrl+C to stop.
- Usable as a library: `default-features = false` drops the command line; the `frames` feature holds GStreamer and `describe`.
