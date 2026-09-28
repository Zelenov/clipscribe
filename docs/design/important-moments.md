# Only important moments

Issue #10. Today's prompt asks for "consecutive stretches" covering the clip and to "merge
stretches where nothing changes" — that framing tells the model to tile the whole timeline, so a
static shot (a starry sky, someone talking to camera) still gets 3+ invented ranges. The owner
wants segments (`moments` from here on, matching the CLI's own wording) to appear only where
something actually stands out; the summary already covers everything else.

## What changes, and what does not

This only touches how many moments the model is asked for and how the answer is validated — the
JSON shape (`{summary, segments}`) is unchanged; `schema()` already allows an empty `segments`
array (`additionalProperties: false`, no `minItems`), so no schema change is needed to make an
empty list valid. What changes:

1. The prompt: no "cover the whole clip" framing; explicit permission (and a clear steer) to
   return no moments when nothing stands out, plus criteria for what does.
2. Answer validation: two more filters, applied only in the new `Important` mode (see below) —
   drop a moment covering almost the whole clip (that is what the summary is for) and merge
   adjacent moments whose descriptions say the same thing (a sign the model still tiled instead
   of picking out what matters).

## `important` vs `full`: one mode, not per-request tuning

`important` (new default) uses the rewritten prompt and the extra validation above. `full` keeps
today's prompt and validation byte for byte — for anyone who already built around the old,
always-covers-the-clip behaviour and wants it back. `Options` gains `pub moments: MomentsMode`
(`Important` default, `Full`), the same shape as `FrameSampling`; `build_request` and
`parse_answer` both take a `MomentsMode` parameter now, since the same request/answer pair serves
both modes with different instructions and validation. Both existing callers of these two
functions — `describe` (`lib.rs`) and the tag-suggestion path (`tags::build_combined_request`,
`tags::parse_combined_answer`) — thread `options.moments` through, so `describe_with_tags`'s
descriptions get the same treatment as plain `describe`'s.

This is a breaking change to two existing public functions' signatures (`describe::build_request`,
`describe::parse_answer`) and to `Options` (a new required field), on top of the two breaking
changes `Options`/`Clip::sample` already picked up for key frames and none for tags (tags added no
breaking change). Noted in `version.md`; frename must add `moments: MomentsMode::Important` (or
`Full`) to its `Options` literal and, if it calls `build_request`/`parse_answer` directly (it
doesn't today — it only uses `describe`/`describe_with_tags`), pass a mode there too.

## The prompt

Replaces "consecutive stretches … merge stretches where nothing changes" with the owner's own
rules, condensed: an empty list is the right answer when nothing stands out (a static shot,
talking to camera, walking while talking with nothing changing); a small or vague change is not a
moment; a stretch that clearly stands out from the rest of the clip gets one; when the clip is
made of clearly different parts, each part gets one (the editor needs the cut points). The
`max_segments` cap (one per 30 s, 3 to 12) stays as an upper bound only — never a target to fill.

## Validation

- **Whole-clip drop:** a moment spanning ≥ 90% of `duration_s` is dropped — the issue's own
  number, and a clear enough sign the model defaulted back to "cover everything" that a stricter
  or looser threshold would not change the outcome on the clips this repo has to test against.
- **Same-description merge:** after sorting by `start_s`, adjacent moments whose descriptions are
  identical once trimmed and lower-cased are merged into one spanning both (keeping the first
  description). This is a literal-text check, not a semantic one — no embedding or similarity
  library is added for it — so it catches the model repeating itself verbatim (the concrete
  failure mode "tiling" produces) without claiming to catch every rephrasing of the same idea.
  Repeated once (a single pass over the sorted list) since a tiling model repeats short, near-
  identical sentences across adjacent tiles, not across the whole answer.
- Both run only in `Important` mode, after the existing invalid/out-of-clip filtering and before
  the existing `max_segments` truncation — `Full` mode's validation is untouched.

## Markers: not in this version

The issue gates markers behind "only if they reduce the junk in ranges" and allows keeping ranges
alone if they already work cleanly. Judging that without a live model call isn't possible from
this session (`CLIPSCRIBE_LIVE_API_KEY` is unset here), and shipping a new, permanent output field
(`markers: [{time_s, description}]`) on the strength of a prompt-engineering guess — with no
evidence it actually reduces junk, the one condition the issue sets for adding it — is the wrong
tradeoff against a field that, once shipped, this crate's own rules say can never be removed
without a breaking change. Ranges alone already satisfy the issue's "Done when": no ranges for
static/uniform clips, only standing-out or clearly-different parts otherwise. Left as a follow-up
`idea` issue for whoever can run a live evaluation of whether markers earn their place.

## Evidence

No live API key in this session, so per the `nightly` skill: the sample output uses `--estimate`
(unchanged — the request shape and frame budget are identical, only the prompt text differs) and a
unit test's `--nocapture` output showing the new prompt text and the validation behaviour
(whole-clip drop, same-description merge, empty list accepted) on constructed answers, plus the
existing test clips' shape (short, largely static or single-activity clips) discussed against the
new prompt's rules in the PR body.

## Decisions made without the owner

- Markers are not implemented this version (see above); a follow-up `idea` issue is filed for a
  session that can run a live evaluation.
- The 90% whole-clip threshold and the "adjacent, identical after trimming/lower-casing" merge
  rule are both simple, deterministic, and literal — no new dependency or fuzzy-matching library
  for a validation step that exists to catch a specific known failure mode (tiling), not to be a
  general summarization-quality filter.
- `full` mode is kept indefinitely, not deprecated — some callers may specifically want the old,
  predictable "always covers the clip" shape (e.g. for a stable diff/test fixture), and it costs
  nothing to keep since it is exactly the pre-existing code path with no code deleted.
