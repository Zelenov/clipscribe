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
   missing), all ranges are dropped: they collapse to the summary.
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
