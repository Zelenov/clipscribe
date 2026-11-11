use super::*;
use crate::describe::{MainRange, Segment};
use crate::shots::{Shot, Transition};
use crate::tags::{TagRange, TagSuggestion};

/// The example of `docs/describe-format.md`.
const EXAMPLE: &str = r#"{
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
}"#;

fn example() -> DescribeFile {
    DescribeFile::parse(EXAMPLE).expect("the example parses")
}

fn frame() -> FrameInfo {
    FrameInfo {
        width: 1920,
        height: 1080,
        fps: 25.0,
    }
}

fn segment(start_s: f64, end_s: f64, title: &str, description: &str) -> Segment {
    Segment {
        start_s,
        end_s,
        title: title.to_string(),
        description: description.to_string(),
    }
}

fn facts(moments: MomentsMode) -> RunFacts<'static> {
    RunFacts {
        duration_s: 30.0,
        frame: frame(),
        fallback_lang: "en",
        moments,
        shots: None,
        tags: None,
        min_tag_confidence: 0.5,
    }
}

fn marker(id: &str, by: By, start_s: f64, end_s: Option<f64>, title: &str) -> Marker {
    Marker {
        id: id.to_string(),
        by,
        start_s,
        end_s,
        title: Some(title.to_string()),
        description: None,
        color: None,
        extra: Extra::new(),
    }
}

fn base(duration_s: f64) -> DescribeFile {
    DescribeFile {
        version: FORMAT_VERSION,
        duration_s,
        frame: frame(),
        lang: "en".to_string(),
        summary: None,
        usable: None,
        in_out: None,
        cuts: None,
        parts: None,
        moments: None,
        people: None,
        faces: None,
        texts: None,
        tags: None,
        extra: Extra::new(),
    }
}

fn face(id: &str, start_s: f64, end_s: f64, bbox: [f64; 4]) -> Track {
    Track {
        id: id.to_string(),
        start_s,
        end_s,
        track: vec![Sample {
            t_s: start_s,
            bbox,
            extra: Extra::new(),
        }],
        text: None,
        extra: Extra::new(),
    }
}

fn person(id: &str, by: By, label: &str, faces: &[&str]) -> Person {
    Person {
        id: id.to_string(),
        by,
        label: Some(label.to_string()),
        speaker: None,
        faces: faces.iter().map(|f| f.to_string()).collect(),
        extra: Extra::new(),
    }
}

fn spans(parts: &[Marker]) -> Vec<(&str, f64, f64)> {
    parts
        .iter()
        .map(|p| {
            (
                p.title.as_deref().unwrap_or(""),
                p.start_s,
                p.end_or_start(),
            )
        })
        .collect()
}

#[test]
fn the_documented_example_is_valid_and_round_trips() {
    let file = example();
    assert_eq!(file.problems(), Vec::<String>::new());
    assert_eq!(file.frame.fps, 29.97);
    assert_eq!(file.moments.as_ref().unwrap()[0].end_s, None);
    assert_eq!(
        file.texts.as_ref().unwrap()[0].text.as_deref(),
        Some("War and Peace\nLeo Tolstoy")
    );
    let again = DescribeFile::parse(&file.to_json()).expect("written file parses");
    assert_eq!(again, file);
    assert!(!file.to_json().contains("null"), "null is never written");
}

#[test]
fn unknown_fields_are_kept_at_every_level() {
    let text = EXAMPLE
        .replacen(
            "\"version\": 1,",
            "\"version\": 1, \"future\": {\"a\": 1},",
            1,
        )
        .replacen(
            "\"title\": \"Gate\",",
            "\"title\": \"Gate\", \"premiere_guid\": \"x\",",
            1,
        )
        .replacen("\"t_s\": 16.0,", "\"t_s\": 16.0, \"angle\": 3,", 1);
    let file = DescribeFile::parse(&text).expect("parses");
    assert_eq!(file.extra["future"]["a"], 1);
    assert_eq!(
        file.moments.as_ref().unwrap()[0].extra["premiere_guid"],
        "x"
    );
    let written = file.to_json();
    for kept in ["\"future\"", "\"premiere_guid\"", "\"angle\""] {
        assert!(written.contains(kept), "{kept} is written back");
    }
}

#[test]
fn another_version_or_not_the_format_is_refused() {
    let v2 = EXAMPLE.replacen("\"version\": 1", "\"version\": 2", 1);
    assert_eq!(DescribeFile::parse(&v2), Err(ReadError::Version(2)));
    assert!(matches!(
        DescribeFile::parse("{}"),
        Err(ReadError::Invalid(_))
    ));
    assert!(matches!(
        DescribeFile::parse("not json"),
        Err(ReadError::Invalid(_))
    ));
    assert!(matches!(
        DescribeFile::parse(r#"{"version": 1, "duration_s": 3}"#),
        Err(ReadError::Invalid(_))
    ));
}

#[test]
fn absent_and_empty_analysis_stay_apart() {
    let mut file = base(10.0);
    file.faces = Some(vec![]);
    let written = file.to_json();
    assert!(written.contains("\"faces\": []"), "done, nothing found");
    assert!(!written.contains("\"texts\""), "not done");
    let again = DescribeFile::parse(&written).unwrap();
    assert_eq!(again.faces, Some(vec![]));
    assert_eq!(again.texts, None);
}

#[test]
fn problems_name_each_broken_rule() {
    let mut file = example();
    let parts = file.parts.as_mut().unwrap();
    parts[1].start_s = 15.0; // a gap after the first part
    parts[0].title = Some("A title that is far too long for a marker.".to_string());
    let moments = file.moments.as_mut().unwrap();
    moments[0].description = Some("@p9 waves.".to_string());
    moments[1].color = Some("pink".to_string());
    moments[2].id = "k3m9xa".to_string(); // the first part's id
    file.faces.as_mut().unwrap()[1].track[0].bbox = [0.9, 0.2, 0.2, 0.1];
    let problems = file.problems().join("\n");
    for expected in [
        "leaves a gap",
        "breaks the title rule",
        "names p9",
        "unknown colour",
        "used twice",
        "outside the frame",
    ] {
        assert!(problems.contains(expected), "{expected} in:\n{problems}");
    }
}

#[test]
fn a_full_run_becomes_parts_that_cover_the_clip() {
    let description = Description {
        summary: "A walk in a park.".to_string(),
        segments: vec![
            segment(12.0, 20.0, "Pond", "Ducks swim in a pond."),
            segment(1.0, 12.0, "", "Someone walks along a path."),
            segment(25.0, 31.0, "Bench.", "A person sits on a bench."),
        ],
        main: None,
        lang: Some("en".to_string()),
    };
    let file = DescribeFile::from_described(&description, facts(MomentsMode::Full));
    assert_eq!(file.problems(), Vec::<String>::new());
    let parts = file.parts.as_ref().unwrap();
    assert_eq!(
        spans(parts),
        [
            ("Someone walks along a path", 0.0, 12.0),
            ("Pond", 12.0, 25.0),
            ("Bench", 25.0, 30.0)
        ]
    );
    assert!(parts.iter().all(|p| p.by == By::Ai && p.id.len() == 6));
    assert_eq!(file.moments, None, "moments were not asked for");
    assert_eq!(file.people, None, "people were not looked for");
}

#[test]
fn an_important_run_becomes_moments_with_usable_cuts_and_tags() {
    let description = Description {
        summary: "Someone talks to the camera.".to_string(),
        segments: vec![segment(
            10.0,
            14.0,
            "Dog runs past",
            "A dog runs past behind them.",
        )],
        main: Some(MainRange {
            start_s: 2.0,
            end_s: 28.0,
        }),
        lang: None,
    };
    let shots = Shots {
        shots: vec![
            Shot {
                start_s: 0.0,
                end_s: 9.0,
                enters_by: None,
            },
            Shot {
                start_s: 9.0,
                end_s: 30.0,
                enters_by: Some(TransitionKind::Cut),
            },
        ],
        transitions: vec![Transition {
            kind: TransitionKind::Cut,
            start_s: 9.0,
            end_s: 9.0,
        }],
    };
    let tags = TagSuggestions {
        tags: vec![
            TagSuggestion {
                name: "dog".to_string(),
                confidence: 0.9,
                ranges: vec![TagRange {
                    start_s: 10.0,
                    end_s: 14.0,
                }],
            },
            TagSuggestion {
                name: "cat".to_string(),
                confidence: 0.2,
                ranges: vec![],
            },
        ],
        new_tag_ideas: vec![],
    };
    let facts = RunFacts {
        shots: Some(&shots),
        tags: Some(&tags),
        fallback_lang: "ru",
        ..facts(MomentsMode::Important)
    };
    let file = DescribeFile::from_described(&description, facts);
    assert_eq!(file.problems(), Vec::<String>::new());
    assert_eq!(
        file.lang, "ru",
        "the fallback when the answer has no language"
    );
    assert_eq!(
        file.usable,
        Some(Span {
            start_s: 2.0,
            end_s: 28.0
        })
    );
    assert_eq!(file.cuts, Some(vec![9.0]));
    assert_eq!(file.parts, None);
    let moments = file.moments.as_ref().unwrap();
    assert_eq!(spans(moments), [("Dog runs past", 10.0, 14.0)]);
    assert_eq!(
        moments[0].description.as_deref(),
        Some("A dog runs past behind them.")
    );
    let tag_names: Vec<&str> = file
        .tags
        .iter()
        .flatten()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(tag_names, ["dog"], "under the threshold is not stored");
}

#[test]
fn a_new_run_keeps_user_items_and_replaces_ai_ones() {
    let mut old = base(30.0);
    old.in_out = Some(InOut {
        start_s: Some(1.0),
        end_s: None,
        extra: Extra::new(),
    });
    old.summary = Some("Old summary.".to_string());
    old.moments = Some(vec![
        marker("aaaaaa", By::Ai, 3.0, None, "Old AI moment"),
        marker("uuuuuu", By::User, 10.0, None, "Mine"),
    ]);
    let mut new = base(30.0);
    new.summary = Some("New summary.".to_string());
    new.moments = Some(vec![
        marker("bbbbbb", By::Ai, 10.3, None, "Snapped away"),
        marker("uuuuuu", By::Ai, 20.0, Some(22.0), "New AI moment"),
    ]);
    let merged = old.merge_run(new);
    assert_eq!(merged.summary.as_deref(), Some("New summary."));
    assert_eq!(merged.in_out, old.in_out, "the In/Out a person set stays");
    let moments = merged.moments.as_ref().unwrap();
    assert_eq!(
        spans(moments),
        [("Mine", 10.0, 10.0), ("New AI moment", 20.0, 22.0)]
    );
    assert_ne!(moments[1].id, "uuuuuu", "a kept id is never reused");
    assert_eq!(merged.problems(), Vec::<String>::new());
}

#[test]
fn new_ai_parts_are_cut_around_user_parts() {
    let old = || {
        let mut old = base(30.0);
        old.parts = Some(vec![
            marker("a00000", By::Ai, 0.0, Some(10.0), "Old"),
            marker("u00000", By::User, 10.0, Some(20.0), "Mine"),
            marker("a00001", By::Ai, 20.0, Some(30.0), "Old too"),
        ]);
        old
    };
    let mut new = base(30.0);
    new.parts = Some(vec![
        marker("n00000", By::Ai, 0.0, Some(10.5), "Start"),
        marker("n00001", By::Ai, 10.5, Some(30.0), "Rest"),
    ]);
    let merged = old().merge_run(new);
    assert_eq!(
        spans(merged.parts.as_ref().unwrap()),
        [
            ("Start", 0.0, 10.0),
            ("Mine", 10.0, 20.0),
            ("Rest", 20.0, 30.0)
        ]
    );
    assert_eq!(merged.problems(), Vec::<String>::new());

    // A leftover shorter than a second goes to the part before it.
    let mut new = base(30.0);
    new.parts = Some(vec![
        marker("n00000", By::Ai, 0.0, Some(20.5), "Start"),
        marker("n00001", By::Ai, 20.5, Some(30.0), "Rest"),
    ]);
    let merged = old().merge_run(new);
    assert_eq!(
        spans(merged.parts.as_ref().unwrap()),
        [
            ("Start", 0.0, 10.0),
            ("Mine", 10.0, 20.5),
            ("Rest", 20.5, 30.0)
        ]
    );
    assert_eq!(merged.problems(), Vec::<String>::new());
}

#[test]
fn a_new_ai_person_on_a_kept_persons_face_is_that_person() {
    let mut old = base(30.0);
    old.faces = Some(vec![face("f1", 0.0, 10.0, [0.1, 0.1, 0.2, 0.2])]);
    old.people = Some(vec![person("p1", By::User, "the guide", &["f1"])]);
    let mut new = base(30.0);
    new.faces = Some(vec![
        face("f1", 0.0, 10.0, [0.5, 0.5, 0.2, 0.2]),
        face("f2", 1.0, 9.0, [0.12, 0.1, 0.2, 0.2]),
    ]);
    new.people = Some(vec![
        person("p1", By::Ai, "a woman in a hat", &["f1"]),
        person("p2", By::Ai, "a man in a blue coat", &["f2"]),
    ]);
    new.moments = Some(vec![Marker {
        description: Some("@p2 points at @p1.".to_string()),
        ..marker("m00000", By::Ai, 5.0, None, "Pointing")
    }]);
    let merged = old.merge_run(new);
    let people = merged.people.as_ref().unwrap();
    assert_eq!(people.len(), 2);
    let guide = people.iter().find(|p| p.id == "p1").unwrap();
    assert_eq!(
        guide.label.as_deref(),
        Some("the guide"),
        "the user's label stays"
    );
    assert_eq!(guide.faces, ["f2"], "the new face where theirs was");
    let other = people
        .iter()
        .find(|p| p.label.as_deref() == Some("a woman in a hat"))
        .unwrap();
    assert_eq!(
        other.id, "p2",
        "a new person gets an id after the kept ones"
    );
    let text = merged.moments.as_ref().unwrap()[0]
        .description
        .clone()
        .unwrap();
    assert_eq!(text, "@p1 points at @p2.");
    assert_eq!(merged.problems(), Vec::<String>::new());
}

#[test]
fn mentions_show_the_speaker_name_then_the_label_then_someone() {
    let file = example();
    let people = file.people.as_ref().unwrap();
    let names = |speaker: &str| (speaker == "1").then(|| "Anna".to_string());
    assert_eq!(
        render_mentions(file.summary.as_deref().unwrap(), people, names),
        "Anna walks to Kalemegdan fortress and buys corn from a vendor in a grey apron."
    );
    assert_eq!(
        render_mentions("@p7 mails \\@p1 at a@p2x", people, |_| None),
        "someone mails @p1 at a@p2x"
    );
    assert_eq!(mentions("@p1 and @p12, not \\@p3 or @px"), ["p1", "p12"]);
    assert_eq!(
        drop_unknown_mentions("@p1 meets @p9.", people),
        "@p1 meets someone."
    );
}

#[test]
fn new_ids_are_six_crockford_characters_and_never_taken() {
    let mut taken = HashSet::new();
    for _ in 0..500 {
        let id = new_id(&taken);
        assert_eq!(id.len(), 6);
        assert!(id.bytes().all(|c| ID_ALPHABET.contains(&c)), "{id}");
        assert!(taken.insert(id));
    }
}

#[test]
fn a_box_moves_in_a_straight_line_and_holds_at_the_ends() {
    let file = example();
    let f1 = &file.faces.as_ref().unwrap()[0];
    assert_eq!(box_at(f1, -1.0), [0.10, 0.20, 0.25, 0.14]);
    let mid = box_at(f1, 3.5);
    assert!((mid[0] - 0.25).abs() < 1e-9 && (mid[1] - 0.19).abs() < 1e-9);
    assert_eq!(box_at(f1, 99.0), [0.70, 0.20, 0.25, 0.14]);
}

#[test]
fn titles_follow_the_marker_rule() {
    assert_eq!(marker_title("  The gate. ", ""), "The gate");
    assert_eq!(
        marker_title("", "A long description of what happens here in the clip."),
        "A long description of what"
    );
    let long = marker_title(
        "Extraordinarily-long-compound-word-that-never-ends-at-all",
        "",
    );
    assert_eq!(long.chars().count(), MAX_TITLE_CHARS);
}

#[test]
fn the_file_sits_next_to_the_video() {
    assert_eq!(
        describe_file_path(Path::new("/d/clip.mp4")),
        Path::new("/d/clip.describe.json")
    );
}

/// `save_run` on a real clip: the frame and cuts come from the video, a second run keeps what a
/// person changed, and a file it cannot read is left alone. Printed with
/// `cargo test save_run_writes -- --nocapture`, for the PR's sample output.
#[cfg(all(feature = "frames", target_os = "linux"))]
#[test]
fn save_run_writes_next_to_the_video_and_keeps_user_edits() {
    use std::sync::atomic::AtomicBool;
    let dir = std::env::temp_dir().join(format!("clipscribe-describe-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let video = dir.join("edit.mp4");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/clips/shots-edit.mp4"),
        &video,
    )
    .unwrap();
    let clip = |title: &str| crate::DescribedClip {
        description: Description {
            summary: "An edit of three shots.".to_string(),
            segments: vec![segment(2.0, 4.0, title, "Something happens.")],
            main: None,
            lang: Some("en".to_string()),
        },
        tags: None,
        usage: crate::AiUsage::default(),
        duration_s: 10.0,
        frames: vec![],
    };
    let cancel = AtomicBool::new(false);
    let path = save_run(
        &video,
        &clip("First"),
        MomentsMode::Important,
        "en",
        &cancel,
    )
    .unwrap();
    assert_eq!(path, dir.join("edit.describe.json"));
    let mut file = DescribeFile::read(&path).unwrap().unwrap().unwrap();
    assert_eq!(file.problems(), Vec::<String>::new());
    assert_eq!((file.frame.width, file.frame.height), (320, 180));
    assert!(
        file.cuts.as_ref().is_some_and(|c| !c.is_empty()),
        "the edit's cuts"
    );

    // A person renames the AI moment and sets an In.
    let moments = file.moments.as_mut().unwrap();
    moments[0].title = Some("My name".to_string());
    moments[0].by = By::User;
    file.in_out = Some(InOut {
        start_s: Some(1.0),
        ..InOut::default()
    });
    file.write(&path).unwrap();
    save_run(
        &video,
        &clip("Second"),
        MomentsMode::Important,
        "en",
        &cancel,
    )
    .unwrap();
    println!("{}", std::fs::read_to_string(&path).unwrap());
    let file = DescribeFile::read(&path).unwrap().unwrap().unwrap();
    let titles: Vec<&str> = file
        .moments
        .iter()
        .flatten()
        .filter_map(|m| m.title.as_deref())
        .collect();
    assert_eq!(titles, ["My name", "Second"]);
    assert_eq!(file.in_out.and_then(|io| io.start_s), Some(1.0));

    std::fs::write(&path, "{\"version\": 9}").unwrap();
    assert!(matches!(
        save_run(
            &video,
            &clip("Third"),
            MomentsMode::Important,
            "en",
            &cancel
        ),
        Err(SaveError::Existing(ReadError::Version(9)))
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"version\": 9}");
    std::fs::remove_dir_all(&dir).unwrap();
}
