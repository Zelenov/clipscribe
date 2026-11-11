# Video description format, version 1

Status: clipscribe reads, writes, checks and merges it (`clipscribe::describe_file`), and writes
it from a description run (`describe_file::save_run`, `clipscribe --describe-file`). A run of
today's clipscribe fills `summary`, `usable`, `parts` or `moments`, `cuts` and `tags`; people,
faces and texts come with the `describe` service. Schema:
[describe-format.schema.json](describe-format.schema.json).

One JSON file per video, made by the `describe` service with clipscribe. It sits next to the
video's CAPT file (the subtitles) and does not change it. It is for:

1. **Premiere and Resolve markers**, made from the same video's data as its subtitles.
2. **One name per person**: set once, used in the subtitles, the descriptions and the markers.
3. **Captions placed automatically**, so they do not cover faces or on-screen text.

## Example

```json
{
  "version": 1,
  "duration_s": 41.9,
  "frame": {"width": 1080, "height": 1920, "fps": 29.97},
  "lang": "en",
  "summary": "@p1 walks to Kalemegdan fortress and buys corn from @p2.",
  "usable": {"start_s": 2.0, "end_s": 41.5},
  "in_out": {"start_s": 1.5},
  "cuts": [14.0, 16.0],
  "parts": [
    {"id": "k3m9xa", "by": "ai", "start_s": 0.0, "end_s": 14.0, "title": "Why the fortress",
     "description": "@p1 explains why the fortress stands where the rivers meet."},
    {"id": "7hd2k0", "by": "user", "start_s": 14.0, "end_s": 41.9, "title": "At the stall",
     "description": "@p1 buys corn from @p2.", "color": "yellow"}
  ],
  "moments": [
    {"id": "p2r6kc", "by": "ai", "start_s": 14.0, "title": "Gate",
     "description": "The gate comes into view."},
    {"id": "1qc62f", "by": "user", "start_s": 30.1, "end_s": 33.4, "title": "Lion", "color": "red"},
    {"id": "c19c0922-775d-465c-917b-6a7c3a5c0902", "by": "user", "start_s": 38.0, "color": "green"}
  ],
  "people": [
    {"id": "p1", "by": "user", "label": "a woman in a red jacket", "speaker": "1",
     "faces": ["f1", "f3"]},
    {"id": "p2", "by": "ai", "label": "a vendor in a grey apron", "faces": ["f2"]},
    {"id": "p3", "by": "ai", "label": "the narrator", "speaker": "3"}
  ],
  "faces": [
    {"id": "f1", "start_s": 0.0, "end_s": 14.0, "track": [
      {"t_s": 0.0,  "box": [0.10, 0.20, 0.25, 0.14]},
      {"t_s": 7.0,  "box": [0.40, 0.18, 0.24, 0.14]},
      {"t_s": 14.0, "box": [0.70, 0.20, 0.25, 0.14]}
    ]},
    {"id": "f2", "start_s": 16.0, "end_s": 41.9, "track": [
      {"t_s": 16.0, "box": [0.55, 0.22, 0.20, 0.12]}
    ]},
    {"id": "f3", "start_s": 16.0, "end_s": 41.9, "track": [
      {"t_s": 16.0, "box": [0.20, 0.25, 0.22, 0.12]}
    ]}
  ],
  "texts": [
    {"id": "x1", "start_s": 20.0, "end_s": 26.0, "text": "War and Peace\nLeo Tolstoy", "track": [
      {"t_s": 20.0, "box": [0.30, 0.50, 0.20, 0.12]},
      {"t_s": 23.0, "box": [0.45, 0.40, 0.22, 0.14]}
    ]}
  ],
  "tags": [
    {"name": "street"},
    {"name": "history", "ranges": [{"start_s": 0.0, "end_s": 14.0}]}
  ]
}
```

## The file

UTF-8 JSON, `application/json`. Next to the video it is `<video stem>.describe.json`
(`clip.mp4` → `clip.describe.json`, like `clip.capt`); a download is named the same way.

### Top level

| Field | Always | Means |
|---|---|---|
| `version` | ✓ | `1`. It changes only when an old reader would misread the file |
| `duration_s` | ✓ | the video's length |
| `frame` | ✓ | the picture as shown: `width` and `height` in pixels after the rotation tag and the pixel aspect, letterbox bars included; `fps` is the nominal frame rate, or the average one for variable-rate phone footage. It is kept because the video is deleted after 7 days, and exports and later burn-ins still need it |
| `lang` | ✓ | the language of `summary`, `title`, `description` and `label`. It is not the speech's language |
| `summary` | | one sentence about the whole video |
| `usable` | | where the real content starts and ends, without setting up or lowering the camera. The AI's suggestion, never applied by itself. Absent when the whole video is usable |
| `in_out` | | the In and Out a person set: `start_s`, `end_s`, either one may be missing (the video's own start or end) |
| `cuts` | | times of shot changes, in seconds |
| `parts` | | stretches like chapters: they cover the video from 0 to `duration_s` without gaps or overlaps. AI makes them from a change of subject in the speech or of place or shot in the picture |
| `moments` | | notable points or stretches. They may overlap each other and parts |
| `people` | | the people the text talks about: seen, heard, or both |
| `faces` | | every face found, with no identity. A crowd gives many faces and no people |
| `texts` | | text in the picture: signs, lower thirds, logos, burned-in subtitles |
| `tags` | | tags from the user's tag list that the AI found to apply |

### Parts and moments

| Field | Means |
|---|---|
| `id` | any string, unique in the file, never changed. frename makes 6 random characters of Crockford base32 (`0-9 a-z` without `i l o u`), like CAPT block ids. A marker that came from Premiere keeps Premiere's GUID as its id |
| `by` | `ai`: made by a description run, which a new run replaces. `user`: made by a person, or an AI item a person changed (renamed, moved, resized, recoloured, described) |
| `start_s` | where it starts |
| `end_s` | where it ends. Parts always have one; a moment without one is a point in time. Never equal to `start_s` |
| `title` | the marker's name, one line. Required on `ai` items: 1–5 words, at most 40 characters, a noun phrase with no full stop, no `@pN`. On `user` items, whatever the person typed; absent means unnamed |
| `description` | the marker's comment. From the AI, one sentence saying what happens that does not just repeat the title, and it may use `@pN`; absent when the title says it all. From a person, any text, line breaks as `\n` |
| `color` | the editor's own colour code, kept exactly and never read for meaning. One of Premiere's nine (`green`, `red`, `orange`, `yellow`, `white`, `blue`, `cyan`, `lavender`, `magenta`), or `#rrggbb` / `#rrggbbaa` for any other value Premiere holds. The AI never sets it. Absent: not chosen, shown white |

### People

| Field | Means |
|---|---|
| `id` | `p` and a number, unique in the file, never changed |
| `by` | as for moments. A person becomes `user` when someone edits their label or confirms their `speaker`, or when an `@pN` in a `user` item's description refers to them |
| `label` | a noun phrase in `lang` that fits in a sentence: "a woman in a red jacket". It says what sets this person apart from the others **in this video**: clothing, role, place in the frame, obvious woman or man, obvious child or adult, and visible features (skin tone, hair, height) only when they are what sets them apart. In a video where everyone shares a feature, no label mentions it. It describes the person over the whole video: clothing only if it does not change, otherwise what stays the same ("the man with a beard and glasses", "the guide", "the person filming"); a change of clothes goes into the text of the moment ("@p1, now in a swimsuit, …"). It describes what is seen, never what is concluded from it: never a name taken from a face, ethnicity, nationality, religion, health or disability, an age in years, attractiveness or weight |
| `speaker` | the speaker id in this video's CAPT file (`speaker:1` → `"1"`). Absent unless it is certain or a person confirmed it |
| `faces` | ids of this person's face tracks |

### Faces and texts

| Field | Means |
|---|---|
| `id` | `f` (faces) or `x` (texts) and a number, unique in the file. Made again by every run |
| `start_s`, `end_s` | when it is on screen |
| `track` | where it is: `{t_s, box}` samples, in order of time, at least one |
| `box` | `[x, y, width, height]` as fractions of `frame`, from its top-left corner, all between 0 and 1, inside the frame (`x + width` and `y + height` at most 1). One box around a block of several lines of text; the upright rectangle around something tilted |
| `text` (texts) | what it says, lines as `\n`, at most 200 characters. Optional: a screen recording needs no transcription |

### Tags

| Field | Means |
|---|---|
| `name` | exactly as in the user's tag list |
| `ranges` | `{start_s, end_s}` stretches where it applies. Absent: the whole video |

Only tags above the threshold are stored, and confidence is not stored. The user's own tags stay
where they keep them (the desktop app's file names and tag store), not here.

## Rules

- **Absent or empty:** absent analysis (`summary`, `cuts`, `parts`, `moments`, `people`, `faces`,
  `texts`, `tags`) means it was not done. `[]` means it was done and nothing was found. Absent
  user data (`in_out`, `title`, `color`) means not set. `null` is never used.
- **Times:** all are seconds of the video, from 0 to `duration_s`, with `end_s` after `start_s`.
  Lists of things in time are sorted by `start_s`, then `end_s`.
- **Unknown fields** are kept by every reader that writes the file back.
- **Tracks:** a box moves in a straight line between samples. Before the first sample and after
  the last, it stays where it was, within the track's `start_s` and `end_s`. **A track never
  crosses a cut.** When something leaves the frame and comes back, it gets a new track.
- **`@pN` in text** is shown as the name of the person's speaker in the CAPT file, else as
  `label`, else as "someone". `\@` is a plain `@`. clipscribe drops an `@pN` that names no one in
  `people`.
- **A speaker merge** in the web (2 → 1) also rewrites `speaker` here.
- **One person, one entry:** two people linked to the same speaker are one person, and
  clipscribe merges them. When the model is not sure two appearances are the same person (other
  clothes, no speech), it makes two people: a wrong split is easy to fix, a wrong merge is not.
- **Merging people** (the web, like the speaker merge): merging `p4` into `p1` joins their
  `faces`, rewrites every `@p4` to `@p1`, keeps `p1`'s label and speaker, and makes `p1`
  `by: user`. `p4` is gone and its id is not reused. Splitting moves chosen face tracks to a new
  person, `by: user`.
- **Across videos** the file holds no identity: every file has its own `p1`, `p2`. What carries
  over is the speaker's name in the CAPT file, which the web can offer from the names used before.
- **A new run** keeps everything `by: user` and replaces everything `by: ai`:
  - **Moments:** a new AI moment within 0.5 s of a user point moment is dropped (one marker per
    moment, as desktop's `MARKER_SNAP_MS`).
  - **Parts:** new AI parts are cut back where a user part is, and an AI part left shorter than
    1 s is dropped, its time going to the part before it.
  - **People:** a new AI person whose faces overlap a kept person's faces in time and place is
    that person. Their faces are added and no new id is made.
  - **Faces, texts, cuts, tags, `summary`, `usable`:** all from the new run.
- **Ids are never reused** within a file, even after their item is deleted.

## Who makes it

**The `describe` service** runs clipscribe on the video while it still exists (7 days):

1. It decodes the video at 2–5 fps and finds the cuts. Then it finds face tracks (YuNet) and text
   tracks (a text detector, with recognition only where needed), all cut at the cuts. Geometry is
   measured, not asked of the model: model boxes are rough, and many faces or texts would exceed
   the model's answer limit (4 000 tokens for Haiku, 16 000 for Sonnet).
2. The model gets:
   - the key frames, with each face track's id drawn on the frame;
   - the sentences from the CAPT, with speakers;
   - the sound blocks;
   - the tag list and the tags the user already set.

   It answers with `summary`, `usable`, parts, moments, people (label and face ids) and tags. It
   gives no boxes. For labels it is asked what sets each person apart from the others in these
   frames, under the `label` rule.
3. `speaker` is filled in only when one prominent face is on screen for most of that speaker's
   turns. Otherwise the web asks.
4. People are matched across shots by the model reading the labelled frames. No face embeddings
   are made or stored.

The types live in clipscribe, which both the desktop app and the service already use.

## Who reads it

**Placing captions** (the captions crate, at render time, for each caption):

- A position a person dragged (CAPT `pos`) wins. Automatic positions are never stored.
- A caption avoids the area each face and each text sweeps while it is shown, worked out
  separately for each shot. Faces smaller than 5% of the frame height do not count.
- The order is bottom, then top, then middle. If every place covers something, text is covered
  before a face.
- ASS `PlayRes` comes from `frame`.

**Export to Premiere (FCP7 XML) and Resolve (EDL):**

| From | Becomes |
|---|---|
| part | a comment marker over the part's start to end |
| moment | a comment marker, a point or with a duration |
| `in_out` | the clip's In/Out |

- The marker's name is the `title`, its comment the `description`, and its colour the `color`.
  Without a `color`, the marker is white.
- `usable` is not exported: it is a suggestion.
- Times are rounded to frames with `frame.fps`.
- Resolve has no white or orange. The exporter maps each colour to the nearest Resolve colour.

**Desktop frename** maps the format onto what it already has:

| This format | Desktop frename |
|---|---|
| part, moment | `Marker`: `start_ms`, `duration_ms` (0 = point), `name`, `comment`, `color` |
| `id` | a cue-point parameter `frename_id`; `xmpDM:guid` and `marker_guid` are the id when it is a UUID, else a UUID v5 of it. A marker without the parameter (made by Premiere) has its GUID as its id |
| `by` | a cue-point parameter `frename_by:ai`, cleared when the editor changes the marker. A white marker without it (described before) is read as AI |
| `in_out` | the In/Out points, as an XMP subclip marker or an `In/Out:` line |
| `usable` | the `Suggested In/Out:` line, applied only by the **AI** pill |
| `summary` | the `AI: ` line of the comment |

- A Premiere marker with no colour parameter is `green`, never absent.
- The editor's own comment text is not in this file.

## To check before building

- Premiere keeps frename's own cue-point parameters (`frename_id`, `frename_by`) when it saves
  a clip's markers.
- Premiere imports clip markers from FCP7 XML and relinks the clip by name.
- Resolve's marker colours.
- Version 1 of the service can ship faces only, and add texts later.
