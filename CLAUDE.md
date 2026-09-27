# clipscribe — agent guide

This file is what an agent needs to build, test and ship clipscribe without a human in the loop.

## Layout

- `src/lib.rs` — the library: `Options`, `Description`, `Cue`, errors; re-exports `describe`.
- `src/describe.rs` — models and prices (`MODELS`), languages, the request (prompt), the answer,
  the estimate, `describe` itself.
- `src/frames.rs` — GStreamer frame sampling (feature `frames`).
- `src/anthropic.rs`, `src/provider.rs` — the Anthropic Messages API client, `AiError`, `AiUsage`.
- `src/srt.rs` — subtitles. `src/main.rs` — the `clipscribe` command line (feature `cli`).
- `tests/clips/` — test clips (list in `tests/clips.txt`); the frame tests decode them on Linux.
- `scripts/install-gstreamer.ps1` — the Windows GStreamer install CI uses.
- `.claude/skills/` — project skills (nightly, review-gate, readme, create-release-version).
- `docs/design/` — design notes for features that needed them (created with the first one).
- `version.md` — release notes; its first line `# X.Y.Z` is the version and must equal
  `version` in `Cargo.toml` (`release.yml` checks). A change to it on `main` publishes the crate
  to crates.io and a GitHub release `vX.Y.Z` with Windows, Linux and macOS builds
  (`.github/workflows/release.yml`). **A crates.io version can never be replaced.** To pause a
  release PR, convert it to a draft.

Features: `cli` (default; clap, ctrlc; implies `frames`), `frames` (default; GStreamer and
`describe`), none (models, request/answer types, estimate, `srt`, client; no GStreamer).

## Commands

What CI runs (`.github/workflows/ci.yml`), plus the release build:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --locked --no-default-features --features frames -- -D warnings
cargo clippy --locked --no-default-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

`release.yml` also runs `cargo test --release --locked` and `cargo publish --dry-run --locked`.

`CLIPSCRIBE_LIVE_API_KEY` set → `cargo test` sends one real request (Claude Haiku 4.5, about
$0.01); unset → that test is skipped.

The Rust toolchain is pinned in `rust-toolchain.toml` so a new stable release cannot turn CI red
overnight. Updating it is its own PR (new lints get fixed there).

Linux needs GStreamer development packages:

```sh
sudo apt-get install -y libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-libav
```

## Autonomous work

Features are GitHub issues in `Zelenov/clipscribe`. The unattended pipeline — picking an issue,
implementing it, independent review, CI, merge and release — is `.claude/skills/nightly/SKILL.md`.
Reviewers follow `.claude/skills/review-gate/SKILL.md`. User-facing text follows
`.claude/skills/readme/SKILL.md`.

The issue is the request. Do exactly what the issue asks (the agent's design notes only fill in
details), nothing beyond it. Ideas of your own become new issues labelled `idea`, never extra code
in the current PR.

## Consumers

frename (`Zelenov/frename`) uses clipscribe as a library, pinned by commit (`rev`), with
`default-features = false, features = ["frames"]`.

- A breaking library API change (a removed or renamed public item, a changed signature, a new
  field in a public struct callers build (`Options`), a changed `Error` variant) bumps the
  minor version and is described in `version.md` (`## Changed`, what callers must change) so
  frename can follow.
- The library with `frames` and without default features must keep building: CI checks both.
- After a release that frename must follow (a breaking change, or a fix frename needs), open an
  issue in `Zelenov/frename` labelled `feature`, body `🤖 agent:` with the version, what changed and
  what frename must do (frename's pipeline takes it once the owner labels it `approved`); link
  it from the PR. If frename need not change, write "frename: no change
  needed" in the PR body instead. The pin itself is moved in frename, never from here.

## Labels

| Label | Set by | Meaning |
|---|---|---|
| `P1` `P2` `P3` | owner/agent | Priority (lower number first). |
| `regression` | owner | A release broke something; picked before everything else. |
| `feature`, `process` | owner/agent | Kind of work. |
| `needs-design` | owner/agent | The agent writes its own design notes in `docs/design/` on the feature branch before coding. Not a gate; the owner does not approve designs. |
| `approved` | owner | Makes an `idea` or a non-owner issue implementable. |
| `idea` | agent | Agent's own proposal; not implemented until `approved`. |
| `in-progress` | agent | An agent session is working on it (see heartbeat lock). |
| `awaiting-owner` | agent | Legacy, no longer set: the pipeline never waits for design answers. |
| `needs-owner` | agent | On an issue: agent cannot proceed at all (guarded file, failed release); owner answers and removes it to let the agent retry. |
| `owner-review` | agent | Code review or CI did not converge: the feature is built on its PR but not merged or released. Owner merges it, or removes the label from the PR to hand it back. |
| `hold` | owner | Do not work on / merge this. |
| `blocked`, `rejected` | owner | Not now / never. |
| `agent` | agent | PR opened by the agent pipeline. |
| `release-failed` | agent | A release run failed twice; blocks version bumps until fixed. |

## Owner setup (one-time, GitHub settings)

Settings → Rules → Rulesets → new branch ruleset for `main`, enforcement Active, **no bypass list
(administrators included)**:
- require a pull request before merging (0 approvals: the agent uses the owner's account);
- require status checks `ci-linux` and `ci-windows`, and branches up to date before merging;
- block force pushes; restrict deletions.

Settings → General → Pull Requests: allow squash merging only.

Settings → Secrets and variables → Actions: `CLIPSCRIBE_LIVE_API_KEY` (live test) and
`CARGO_REGISTRY_TOKEN` (crates.io publish).

This makes the gates enforceable, not just written down: the agent merges with the owner's account,
so without the ruleset nothing stops a merge on red CI.

## Never

- Commit secrets, or print them in logs, PR bodies or sample output. The Anthropic key lives in
  the user's environment (`ANTHROPIC_API_KEY`) or `--api-key` at runtime; the live test key is
  `CLIPSCRIBE_LIVE_API_KEY`, an environment variable in agent sessions and a GitHub Actions
  secret in CI. `CARGO_REGISTRY_TOKEN` exists only in CI.
- Run `cargo publish` (other than `--dry-run`) or `cargo yank`: publishing is `release.yml`'s.
- Skip, disable or weaken a test to get CI green.
- Force-push `main` or rewrite its history.
- Merge a PR whose CI is not green on its latest commit.
