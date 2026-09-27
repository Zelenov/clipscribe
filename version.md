# NEXT
## Added
- Key frames: by default, frames are chosen where the picture changes the most in each window of
  the clip's frame budget, instead of a blind fixed interval — a static shot no longer spends the
  same budget as a clip that keeps cutting to something new. `--frames keyframes|interval` picks
  between the new default and the old fixed-interval spacing.

## Changed
- Library: `Options` has a new field `frame_sampling: FrameSampling` (`KeyFrames` or `Interval`);
  set it to `FrameSampling::KeyFrames` for the new default or `FrameSampling::Interval` to keep
  today's behaviour exactly.

# 0.1.0
## Added
- Describe what happens in a video clip, and when, with Claude: frames (one every 2 s, at most 60) and the clip's `.srt` go in; a one-sentence summary and time-ranged key moments come out, with the tokens billed.
- Models Claude Haiku 4.5 (default), Sonnet 5 and Opus 5, with their prices, a cost estimate before sending, and the language of the descriptions.
- `clipscribe` command line (the `cli` feature, on by default): videos or folders in, text or JSON out, `--estimate` to price a batch without sending anything, Ctrl+C to stop.
- Usable as a library: `default-features = false` drops the command line; the `frames` feature holds GStreamer and `describe`.
