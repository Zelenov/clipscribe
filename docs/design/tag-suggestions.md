# Tag suggestions from a given tag vocabulary

Issue #7 (stage 2 of `Zelenov/frename#17`). frename has its own per-folder tag library; it wants
each clip matched against that closed vocabulary — confidences, and time ranges where a tag does
not cover the whole clip — so the editor accepts or rejects suggestions instead of typing tags by
hand.

## Shape of the answer

Mirrors what `describe` already does (structured JSON, `additionalProperties: false`, validated on
the way back): for each vocabulary tag the model may suggest, a `confidence` (0.0–1.0) and,
optionally, `ranges` (empty means "the whole clip"); a separate `new_tag_ideas` list for names
*not* in the vocabulary, so they can never be confused with a real suggestion. Unknown tag names
and out-of-clip ranges are dropped on read, exactly like `describe`'s segments today.

## Input and cost: two modes, not four

The issue asks for two independent choices — frames vs. an already-computed description as input,
and one request vs. two — but they collapse into two real modes, not four: once a description is
already in hand, there is nothing left to combine it *with*; "run in the same request as describe"
only makes sense when frames are the input in the first place.

- **`describe_with_tags`** (frames in, the default): reads the clip's frames exactly as `describe`
  does and asks for a description *and* tag suggestions in one request — one frame-read, one API
  call, sharing the frame tokens between both outputs. This is the cheaper and more accurate path
  whenever a description is wanted anyway (the clipscribe CLI always wants one), so it is what
  `--tags` uses.
- **`suggest_tags`** (an existing `Description` in, no video, no frames feature needed): for a
  caller that already ran `describe` earlier (frename's real shape: describe once, tag later,
  possibly against a vocabulary that did not exist yet at describe time) and does not want to
  decode the clip again just to add tags. Text-only request: the existing summary and segments
  stand in for the frames. Cheaper per call, but blind to anything the description's one sentence
  and few segments left out — a real accuracy/cost trade-off, not a free lunch, and documented as
  such on the function.

Neither is built by extending `describe`'s own request/schema (`describe::build_request`,
`describe::parse_answer`, `Options`): those are stable, tested, and used by every existing caller
(frename included) with no vocabulary at all. Threading an optional vocabulary through them would
mean a schema that changes shape depending on a field of `Options` most calls never set, more
branches in already-covered code, and non-zero risk to a path this crate depends on the most.
Tag suggestions get their own module (`src/tags.rs`), their own request builders and validators,
and their own two entry points in `lib.rs` — additive, nothing existing changes shape.

## Module boundary

`src/tags.rs` holds everything that does not need a decoded clip: `Tag`, vocabulary parsing, the
two JSON schemas, both request builders, both answer parsers. It takes `&[describe::Frame]` when
frames are the input, exactly like `describe::build_request` does, so it needs no GStreamer or
`image` dependency and is not gated behind the `frames` feature — it also means `suggest_tags`
(the description-only mode) works in a `default-features = false` build, which never touches
GStreamer at all.

`describe_with_tags` (needs a video, so `frames` feature only) lives in `lib.rs` next to
`describe`, decoding frames the same way and calling into `tags.rs` for the request/schema/parse.

## Vocabulary format

One tag per line, `name — hint` (an em dash, as the issue's example shows) or `name - hint` (a
plain hyphen — typing an em dash is awkward on most keyboards, and the CLI file is meant to be
hand-edited); the hint is optional. Blank lines and lines starting with `#` are skipped, the same
convention `tests/clips.txt` already uses in this repo.

## Estimate

`estimate_tags_usage` extends `describe::estimate_usage`'s shape: the vocabulary's text (tag names
and hints) is counted the same way subtitle text is (`CHARS_PER_TOKEN`), and a per-tag output
allowance is added (a `confidence` and maybe a couple of ranges per tag, plus room for a handful of
new tag ideas) on top of the model's normal answer size. It is a rougher estimate than the
description-only one — actual output length depends on how many tags plausibly apply, which the
byte count of the vocabulary does not predict — so it is documented as an upper-bound guess, not a
tight one, the same honesty `estimate_usage`'s own doc comment already has about frame counts.

## Decisions made without the owner

- Tag names must match the vocabulary **exactly** (case-sensitive, after trimming). A model that
  answers with different casing or whitespace produces a suggestion `parse_tags_answer` drops as
  unknown; this is stricter than a fuzzy match, but predictable, and the prompt gives the model the
  exact vocabulary strings to copy from.
- `new_tag_ideas` are plain strings with no confidence or ranges — the issue calls them "a separate
  short list", not a scored one; scoring an idea that is not even in the vocabulary yet added
  complexity for a part of the feature explicitly marked optional.
- No language option for tag suggestions: tag names come from the vocabulary as given (never
  translated), and `new_tag_ideas` are written in English, the same default `describe` uses for a
  clip with no subtitles. A future issue can add it if frename's vocabulary needs to be
  non-English.
- The CLI (`--tags`) only exposes the combined `describe_with_tags` path, matching the only thing
  the command line already does (read a video, print a description) — `suggest_tags`'s
  description-only mode is a library-only entry point for a caller (frename) that already has a
  `Description` from an earlier `describe` call.
