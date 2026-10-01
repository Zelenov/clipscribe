# NEXT
## Added
- Write a description into files, several formats from one request: the new `export` module has
  `Format` (`Json`, `Markdown`, `Text`, `Srt`, `Vtt`, `Csv`, `Chapters`, `Xmp`), `Export`
  (`Export::new` for a `Described`, `Export::with_tags` for a `DescribedWithTags`),
  `render(&export, format) -> String`, `plan(video, &formats, out_dir)` (the file names) and
  `write_all(&export, &formats, out_dir, force)`. Files are
  named `<video>.clipscribe.<ext>` (the Premiere sidecar is `<video>.xmp`), so the video's own
  `.srt` is never overwritten; an existing file is skipped unless `force`. The XMP sidecar has the
  moments as Comment markers and the main range as an InOut marker, the same structure frename
  writes. Pure: no network, the same input gives the same bytes.
- Command line: `--format json,srt,md,csv,vtt,txt,chapters,xmp` (repeatable or comma-separated)
  writes each video's files instead of printing it, `--out-dir DIR` puts them in one folder and
  `--force` overwrites. A video whose files all exist is not described again (no request, no
  cost), and two videos that would write the same file are refused before any request.
  `--estimate` lists the files that would be written. `--format` cannot be
  combined with `--json` or `--at`.

# 0.8.0
## Added
- Debugging: see exactly which images the model got. `set_debug_frames_dir(Some(dir))` (or the
  environment variable `CLIPSCRIBE_DEBUG_FRAMES=<dir>`) makes `describe`, `describe_with_tags` and
  `describe_moment` write the frames of each request to `<dir>/<video name>/` as the same JPEG
  bytes that were sent, named by time (`0012.40s.jpg`), with a `frames.json` (time, how the frame
  was picked, size). Off by default; a write error is only logged. `dump_clip_frames` writes the
  frames without sending a request. Nothing to change in code that builds `Options`.

# 0.7.0
## Added
- A main range: when a clip has a lead-in or lead-out (setting up, walking into position) around
  the one part worth keeping, the description carries it as the suggested In/Out —
  `Description::main` (`Option<MainRange>`), `"main": {"start_s", "end_s"}` (or `null`) in
  `--json`, and a `Main: m:ss–m:ss` line in the text output. Only in `important` moments mode.

## Changed
- Important moments, round 2: ranges that together cover almost the whole clip (90 % or more)
  are now dropped unless the model says the clip is really made of clearly different parts, and
  a range that only says what the summary already says is dropped. Ranges that only fill in
  the clip around the main range, or cut the main range into its parts, are dropped too (the
  main range stays). A clip like "pose, hold,
  walk away" now gets a summary plus at most the main range and the moment that stands out,
  not three tiles. Static and uniform clips still get no ranges.
- Library: `Description` has a new field `main: Option<MainRange>`; add `main: None` where you
  build a `Description` yourself. `describe::schema_for(MomentsMode)` and
  `tags::combined_schema_for(MomentsMode)` are new; `schema()` and `combined_schema()` are the
  `full` shapes as before. The `important` request carries two more required answer fields
  (`main`, `distinct_parts`), so code that builds its own request from `schema()` and parses the
  answer with `parse_answer(.., MomentsMode::Important)` should use `schema_for`.

# 0.6.0
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
