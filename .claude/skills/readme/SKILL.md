---
name: readme
description: >
  How to write and update clipscribe's README.md and version.md entries for users. Use whenever a
  change is visible to the user (command line, output, library API, models, prices), when editing
  README.md, or when reviewing user-facing text.
---

# README for users

The README has two readers: people running the `clipscribe` command on their clips, and Rust
developers using the crate (frename is one). It is also the crate's page on crates.io.

## Rules

- Plain language, short sentences, second person. Command-line sections use no internal types or
  implementation details; the library section names only public items.
- Organised by what the reader does (download, describe clips, price a batch, use it as a library,
  build), not by code modules.
- Every command-line option is in the options table, with its default; nothing is listed that
  the binary lacks. Prices and model names match `MODELS`.
- The opening example and the library example must match real output and compile against the
  current API (`lib.rs` doc example too).
- A new feature gets one or two sentences, or a row in a table, where a reader would look for it.
- Keep it compact: when adding, check whether an older paragraph can be shortened or removed.
  Target: README stays under ~200 lines.
- Technical details that are still worth keeping go to `docs/` or doc comments.
- English only.

## version.md

Follow `.claude/skills/create-release-version/SKILL.md`. One line per user-visible change, written
as what the user can now do or what now behaves differently. A breaking library API change says
what callers must change.
