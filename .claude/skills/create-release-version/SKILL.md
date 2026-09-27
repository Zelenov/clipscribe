---
name: create-release-version
description: Create or update a release version entry in version.md. Use when drafting a new version, cutting a release, or when the user asks to create or update release notes.
---

# Create release version

## Release note structure

Every version entry in **version.md** must include **at least one** of `## Added` or `## Changed`.

```markdown
# X.Y.Z
## Added
- short description

## Changed
- short description
```

- **`# X.Y.Z`** — Version as H1 (semver, e.g. `0.2.0`; release tags are `vX.Y.Z`). Must be the
  very first line of the block, and the top block's version must equal `version` in `Cargo.toml`
  (`release.yml` fails otherwise).
- **`# NEXT`** — Unreleased notes on a feature branch. The agent pipeline replaces it with the
  real version, and sets `Cargo.toml` to match, right before merging (`nightly` step 7); `main`
  never carries `# NEXT`.
- **`## Added`** — New features. Omit if nothing added.
- **`## Changed`** — Behaviour changes, fixes, and breaking library API changes (say what callers
  must change). Omit if nothing changed.

**Which number:** third number for fixes; second number (third back to 0) for new features and
any breaking library API change while the version is 0.x. The version must not exist on
crates.io yet: a published crate version can never be replaced.

**Content guidelines:** User-facing changes only (command line, output, library API, models,
prices). Skip internal refactors unless they affect behaviour. One bullet = one line. Keep it short.

## Instructions

1. Add a new H1 block at the **top** of version.md (above existing versions).
2. Every version needs at least one of `## Added` or `## Changed`. No empty sections.
3. Never edit a published block's version; fix mistakes in the next version's notes.

## Example

```markdown
# 0.2.0
## Added
- `--frames-every` sets the seconds between frames (default 2).

## Changed
- Library: `Options` has a new field `frame_interval_s`; set it to `2.0` to keep the old behaviour.
```

## File

- **version.md** — Single file at repo root. Prepend new version blocks; keep older versions below.
- A change to it on `main` runs `.github/workflows/release.yml`: crates.io, then a GitHub release
  `vX.Y.Z` whose notes are the top block.
