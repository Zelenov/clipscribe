# Export formats (issue #28)

One run writes each description into several files, from the one request already paid for.

## Library

`clipscribe::export`: `Format`, `Export` (a view of a `Described` / `DescribedWithTags` plus the
video path and model), `render(&Export, Format) -> String`, `output_path`, `write_all`. No network,
no clock, no randomness (marker ids are a hash of file stem, index, start and text), so golden
tests can pin the bytes. It needs no GStreamer and is available without the `frames` feature.

## Formats

| name | file | content |
|---|---|---|
| `json` | `.json` | the object `--json` prints per video (`main`, `moments`, `tags`, `usage`, cost) |
| `md` | `.md` | summary, main range, moments, tags |
| `txt` | `.txt` | what the command line prints |
| `srt` / `vtt` | `.srt` / `.vtt` | one cue per moment |
| `csv` | `.csv` | `kind,start_s,end_s,start,end,description,tags`; a `main` row, then one row per moment; tags are the suggestions whose range overlaps the moment (or that have none) |
| `chapters` | `.txt` | `m:ss Title`, title = first sentence cut at 100 characters |
| `xmp` | `<stem>.xmp` | Premiere Pro sidecar, structure from frename's `docs/research/premiere-xmp-markers.md` |

## Names

`<stem>.clipscribe.<ext>`, so the video's own `.srt` is never overwritten. Two requested formats
that share an extension (`txt` and `chapters`) become `<stem>.<format>.<ext>`. The XMP sidecar is
always `<stem>.xmp`: Premiere only reads that name. Existing files are skipped and reported unless
`--force`.

## Decisions made without the owner

- `edl` and `fcpxml` are not included: their marker syntax needs a frame rate the clip's
  description does not carry, and the issue allows listing them as follow-ups.
- `--at` does not combine with `--format` (a single moment is not a clip description; the error
  says to use `--at --json`). Formats for one moment are filed as an `idea`. `--json` does not combine with `--format` either: with `--format` stdout only
  lists the files.
- A static clip (no moments) writes an empty `.srt`, a bare `WEBVTT`, a header-only CSV and no
  tracks in the XMP, rather than inventing a cue from the summary.
- `chapters` adds a `0:00 <video stem>` first line when the first moment starts after 1 s, since
  YouTube requires a chapter at 0:00; the stem is language-neutral.
- Marker names are the first sentence of the moment (60 characters in XMP); the full text goes in
  the marker comment, with line breaks as CR as Premiere writes them.
- The XMP is not verified in Premiere Pro by hand (frename's research notes carry the same caveat).
- A video whose files all exist is not described again without `--force` (that request would be
  paid for and thrown away). Two inputs that would write the same file (`a/clip.mp4` and
  `b/clip.mp4` with one `--out-dir`, `clip.mp4` and `clip.mov` side by side) are refused with
  exit 2 before any request.
- Files are created with `create_new` unless `--force`, so an existing file is never overwritten
  by a race either.
- CSV cells starting with `=`, `+`, `-`, `@`, tab or CR get a leading `'` so a spreadsheet does
  not run model-written text as a formula. WebVTT text has `&`, `<` and `-->` escaped. Cues with
  no text are not written.
- `Export` has private fields and two constructors, so adding a field later is not a breaking
  change; `Format` is `#[non_exhaustive]` for the same reason (edl/fcpxml may follow).
- `--out-dir` with `xmp` puts the sidecar where Premiere does not look; the help says so, as it
  does for `--force` overwriting Premiere's own sidecar.
