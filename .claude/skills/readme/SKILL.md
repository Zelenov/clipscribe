---
name: readme
description: >
  How to write and update clipscribe's README.md and version.md entries for users. Use whenever a
  change is visible to the user (command line, output, library API, models, prices), when editing
  README.md, or when reviewing user-facing text.
---

# README for users

The README documents clipscribe as a Rust library: it is the crate's page on crates.io, read by
Rust developers deciding whether to depend on it and then calling it (frename is one). Per the
owner: a crate's README describes the code, not a CLI bundled with it — the `clipscribe` binary's
own `--help` is that tool's documentation, not the README. Never add a command-line section, an
options table, or a CLI usage example here; when a change is CLI-only (a new flag, a changed
default with no library-side equivalent), the README does not mention it at all.

## Rules

- Plain language, short sentences, second person. The library section names only public items.
- Organised by what the reader does (what it does, use it as a library, build), not by code
  modules.
- Prices and model names match `MODELS`.
- The library example must match real output and compile against the current API (`lib.rs` doc
  example too).
- A new feature gets one or two sentences, or a row in a table, where a reader would look for it —
  only if it is visible from the library (a new `Options` field, a new function, a new `MODELS`
  entry); a CLI-only addition is not documented here.
- Keep it compact: when adding, check whether an older paragraph can be shortened or removed.
  Target: README stays under ~200 lines.
- Technical details that are still worth keeping go to `docs/` or doc comments.
- English only.

## version.md

Follow `.claude/skills/create-release-version/SKILL.md`. One line per user-visible change, written
as what the user can now do or what now behaves differently. A breaking library API change says
what callers must change.
