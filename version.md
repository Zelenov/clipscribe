# NEXT
## Added
- `describe_moment(video, at_s, window_s, subtitles, &options, cancel)`: a name and a
  one-to-two sentence description for the frame at a given time, for a marker — one small, fast
  request instead of describing the whole clip. `window_s` (`MOMENT_WINDOW_S`, 1 s, is a
  reasonable default) is how far each way to read frames and subtitles from. `--at m:ss.f` (also
  `h:mm:ss.f` or plain seconds) runs it from the command line, on exactly one video, text or
  `--json`; `--window` overrides the default window.

# 0.5.0
## Added
- ChatGPT (OpenAI) as a second provider: `--provider anthropic|openai` (default `anthropic`)
  picks the AI service; `--model` gains `gpt-4.1-mini` (the OpenAI default) and `gpt-4.1`, and
  `--api-key` falls back to `OPENAI_API_KEY` instead of `ANTHROPIC_API_KEY` when `--provider
  openai` is set.

## Changed
- Library: `Model` has a new field `provider: Provider` (`Anthropic` or `OpenAi`); every entry of
  `MODELS` already sets it, so this only matters if you build a `Model` yourself instead of
  picking one from `MODELS`.
- Library: `AiError::KeyRejected` and `AiError::OutOfCredit` changed from unit variants to
  `KeyRejected(String)` / `OutOfCredit(String)`, the provider's display name ("Anthropic" or
  "OpenAI"), so `AiError::reason()`/`stops_job()` no longer always say "Anthropic". Match them
  with a binding (`KeyRejected(provider)`) instead of the bare variant name.
- Library: `anthropic::RetryPolicy` moved to `provider::RetryPolicy` (re-exported from
  `anthropic` too, so existing code naming it still compiles).
- A request the model refused now says "The model declined to describe it" / "...suggest tags"
  instead of naming Claude specifically, since the same message covers both providers.

# 0.4.0
## Added
- Only important moments: by default, a description's moments (segments) cover only what stands
  out — none at all for a static or uniform clip, one per clearly different or standout part
  otherwise — instead of always tiling the whole clip. `--moments important|full` picks between
  the new default and the old, always-covers-the-clip behaviour.

## Changed
- Library: `Options` has a new field `moments: MomentsMode` (`Important` or `Full`); set it to
  `MomentsMode::Important` for the new default or `MomentsMode::Full` to keep today's behaviour
  exactly.
- Library: `describe::build_request` and `describe::parse_answer` each take a new `moments:
  MomentsMode` parameter; pass `MomentsMode::Full` to keep calling them the way you do today.

# 0.3.0
## Added
- Tag suggestions: `--tags tags.txt` (library: `describe_with_tags`, or `suggest_tags` for a clip
  already described) matches a clip against a closed vocabulary of tags, one per line
  (`name — hint`), with a confidence and time ranges per suggestion; the model may also propose
  short, unscored ideas for tags not in the vocabulary. `--json` gains `tags` and
  `new_tag_ideas` fields when `--tags` is used; unchanged otherwise.

# 0.2.0
## Added
- Key frames: by default, frames are chosen where the picture changes the most in each window of
  the clip's frame budget, instead of a blind fixed interval — a static shot no longer spends the
  same budget as a clip that keeps cutting to something new. `--frames keyframes|interval` picks
  between the new default and the old fixed-interval spacing.

## Changed
- Library: `Options` has a new field `frame_sampling: FrameSampling` (`KeyFrames` or `Interval`);
  set it to `FrameSampling::KeyFrames` for the new default or `FrameSampling::Interval` to keep
  today's behaviour exactly.
- Library: `frames::Clip::sample` (`frames` feature) takes a new `sampling: FrameSampling`
  parameter, right after `duration_s`; pass `FrameSampling::Interval` to keep calling it the way
  you do today.

# 0.1.0
## Added
- Describe what happens in a video clip, and when, with Claude: frames (one every 2 s, at most 60) and the clip's `.srt` go in; a one-sentence summary and time-ranged key moments come out, with the tokens billed.
- Models Claude Haiku 4.5 (default), Sonnet 5 and Opus 5, with their prices, a cost estimate before sending, and the language of the descriptions.
- `clipscribe` command line (the `cli` feature, on by default): videos or folders in, text or JSON out, `--estimate` to price a batch without sending anything, Ctrl+C to stop.
- Usable as a library: `default-features = false` drops the command line; the `frames` feature holds GStreamer and `describe`.
