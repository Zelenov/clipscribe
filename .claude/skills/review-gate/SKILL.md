---
name: review-gate
description: >
  Independent multi-agent review that decides whether a clipscribe change (code PR or design doc)
  may merge. Use before opening or merging any agent-made PR, after every fix round, and when
  asked to "review", "gate" or "check if this is ready for main".
---

# Review gate

The author of a change never judges it. Reviewers are subagents started with fresh context: they get
the issue, the design doc (if any), the commit SHA under review, the PR body (for the sample
output), and the diff against `main` — not the author's reasoning. Run them in parallel. A verdict
applies to that SHA only.

## Reviewers

Code PR — all three, every round:

1. **Correctness** — tries to break the change. Reads the diff and the code around it, looks for
   logic errors, panics/`unwrap` in production paths, and in particular:
   - cost and billing: prices in `MODELS` against Anthropic's published prices (cite the page),
     token counting, the estimate vs. real usage, `cost_usd` units (per million tokens), a retry
     or loop that sends a paid request twice, a batch that goes on after a rejected key or an
     empty balance;
   - API key handling: the key never reaches logs, `Debug` output, error messages, panics, JSON
     output or files; `--api-key` / `ANTHROPIC_API_KEY` precedence unchanged unless asked;
   - HTTP: timeouts on every request, retries only where safe and bounded, HTTP status and
     Anthropic error types mapped to the right `AiError`, cancellation (Ctrl+C, the `AtomicBool`)
     honoured between frames and before the request;
   - GStreamer: pipeline errors and EOS handled (no hang, no panic on an unreadable or odd
     file), state set back to `Null`, rotation, clips with no video stream, very short and very
     long clips (the 30 min limit, the 60-frame cap);
   - the answer: malformed or partial JSON from Claude, moments outside the clip, `BadAnswer`;
   - all three feature combinations build and are tested where they apply (`cli`, `frames`
     only, none), Windows paths, regressions in untouched callers (frename uses the library with
     `frames` only).
   Runs the local gate (`nightly` step 5). Where it suspects a bug it writes a failing test to
   prove it. Checks
   `git diff origin/main... -- '*.rs' '*.toml' 'rust-toolchain*' '.cargo/**' '.github/**' 'scripts/**'`
   for weakened gates: removed or loosened asserts, new `#[ignore]`, deleted tests, `cfg` that
   hides a test on the CI platforms, the live test made to pass without checking anything, CI
   steps or flags removed or relaxed. Each is a blocker unless the issue requires it. Any change to
   a guarded file (list in `nightly` → Trust) that the issue does not explicitly ask for is a
   blocker.
2. **Design and quality** — could this be simpler, smaller, more in line with the codebase? Checks
   module boundaries (frames in `frames.rs`, HTTP in `anthropic.rs`/`provider.rs`, the command line
   only in `main.rs`, nothing GStreamer outside `feature = "frames"`), naming, duplication, dead
   code, new dependencies (needed? optional behind the right feature? default features off where
   possible?), scope creep beyond the issue, missing tests for new behaviour, doc comments on new
   public items.
3. **Product** — does it do what the issue and design ask, from the user's point of view?
   - Command line: option names and defaults consistent with the existing ones, `--help` text,
     stdout for results and stderr for progress, exit codes, text and `--json` output stable
     (a changed JSON field is a breaking change), error messages that say what to do.
   - Library API: easy to call right and hard to call wrong, consistent with `Options`/`describe`,
     no needless breaking change; a breaking change is named in `version.md` with what callers
     must change, and the PR says whether frename must follow.
   - README and `version.md` text (short, user language, per `readme` skill; the README's option
     table and library example match the code).
   - The PR body has `## Sample output` with the exact command and its output for every visible
     change, or "No visible change." when there is none (a missing or wrong sample is a `major`
     finding).

Design doc (advisory only, one round, never blocks; see `nightly` step 3) — reviewers 2 and 3,
judging the design: missing flows, edge cases, cost, simpler alternatives, feasibility (is every
API/format/price claim sourced?).

## Verdict format

Each reviewer returns:

```
VERDICT: APPROVE | CHANGES_REQUIRED
FINDINGS:
- [blocker|major|minor] file:line — problem — concrete failure scenario — suggested fix
```

APPROVE is allowed with only `minor` findings. Any `blocker` or `major` means CHANGES_REQUIRED.
A finding without a concrete failure scenario or a concrete improvement is dropped.

## Loop

1. Fix every blocker/major (and minors that are cheap and clearly right).
2. Re-run the local gate.
3. Start a **new** round with fresh reviewers (never reuse a reviewer that saw an earlier round —
   it anchors on its old findings). Give them the full current diff.
4. Repeat until all three approve in the same round. Four rounds without that → the change is
   finished as far as possible and left unmerged for the owner (`nightly` → "Owner review").

Record every round in the PR description: SHA, each reviewer's verdict, findings count, what was fixed.
