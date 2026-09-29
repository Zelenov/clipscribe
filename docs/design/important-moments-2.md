# Important moments, round 2

Issue #20. `important` mode (#10) drops a range covering ≥ 90 % of the clip and merges
identical neighbours, but three different-sounding ranges that tile a 37 s clip get through:
each is under 90 %, the texts differ. The owner wants: no tiling, no repeating the summary, and
a main range (In/Out).

## Rules (all in `MomentsMode::Important`; `Full` is untouched)

1. **Summary vs ranges.** Prompt: a range says only what the summary does not; the summary does
   not re-list the ranges; sometimes the whole clip is the important part and the summary says
   so. Checked in `parse_answer`: a range whose description is contained in the summary
   (trimmed, lower-cased) is dropped. Literal, like the same-description merge; no fuzzy
   matching library.
2. **Tiling is a red flag.** The answer has a required boolean `distinct_parts`: true only when
   the clip is clearly made of different parts (place, shot type, activity) and the ranges are
   those parts. After the other filters, if the union of ranges covers ≥ 90 % of the clip
   (`WHOLE_CLIP_FRACTION`, overlaps counted once) and `distinct_parts` is not true (false or
   missing), all ranges are dropped: they collapse to the summary. With a main range, tiling
   also means the ranges plus the main range cover ≥ 90 % of the clip (the rest filled in around
   it) or the ranges cover ≥ 90 % of the main range (it cut into parts); either way the ranges
   are dropped and the main range stays. A main range and `distinct_parts: true` contradict each
   other (one part worth keeping is not a clip of distinct parts), so with a main range
   `distinct_parts` is ignored. The prompt says the same: ranges do not fill in around main or
   cut it into parts.
3. **Main range.** Required answer field `main`: a list of at most one `{start_s, end_s}`
   (a list rather than a nullable object because OpenAI strict schemas need every property
   required and an empty list is the same "none" everywhere). Read as the first valid entry,
   clamped to the clip; absent when empty, backwards, outside the clip, or ≥ 90 % of the clip
   (that is what "the whole clip is usable" means). `Description::main: Option<MainRange>`;
   CLI `Main:` line and `"main"` in `--json` (`null` when absent).

## API and cost

`Description` gains a field (breaking for struct literals → minor version, noted in
`version.md`). `schema_for(MomentsMode)` and `combined_schema_for` are new; the old functions
are the `Full` shapes. The answer grows by about 30 tokens.

## Decisions made without the owner

- When tiling collapses, no range is kept "that stands out": the answer does not say which one
  stands out, and picking by length or position would be a guess. The prompt already asks for
  the standout as a range when it is not a tile; the main range survives the collapse.
- `distinct_parts` is not exposed on `Description`: it is a justification for validation, not
  output.
- No live evaluation in this session unless `CLIPSCRIBE_LIVE_API_KEY` is set; see the PR.

## Live check (owner's Kenya clips, 14 clips, Haiku 4.5)

The first version of these rules stopped whole-clip tiles, but in 7 of 9 answers with a main
range the ranges filled the rest of the clip around it or cut it into parts, and one answer
gave a main range together with `distinct_parts: true` to keep them. The main-range rules
above come from those answers. Clips like Chess (vendor display, overhead game, low angle) get
`distinct_parts: true` with no main range and keep their parts, which is what rule 2 allows.
