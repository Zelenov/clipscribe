use super::*;
use crate::{MainRange, TagRange, TagSuggestion, MODELS};

/// A golden file as checked out: a Windows checkout may have turned its line ends into CRLF.
fn golden(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn seg(start_s: f64, end_s: f64, description: &str) -> Segment {
    Segment {
        start_s,
        end_s,
        description: description.to_string(),
    }
}

fn fixture() -> Description {
    Description {
        summary: "A cook plates a dish.".to_string(),
        segments: vec![
            seg(4.0, 51.5, "Он режет лук, быстро. Потом солит."),
            seg(
                3725.25,
                3800.0,
                "Says \"hello\", then\nwaves & leaves <fast>",
            ),
        ],
        main: Some(MainRange {
            start_s: 4.0,
            end_s: 3800.0,
        }),
    }
}

fn tags() -> TagSuggestions {
    TagSuggestions {
        tags: vec![
            TagSuggestion {
                name: "kitchen".to_string(),
                confidence: 0.9,
                ranges: Vec::new(),
            },
            TagSuggestion {
                name: "waving".to_string(),
                confidence: 0.55,
                ranges: vec![TagRange {
                    start_s: 3730.0,
                    end_s: 3740.0,
                }],
            },
        ],
        new_tag_ideas: vec!["onion".to_string()],
    }
}

fn export<'a>(
    video: &'a Path,
    description: &'a Description,
    tags: Option<&'a TagSuggestions>,
) -> Export<'a> {
    Export {
        video,
        description,
        tags,
        duration_s: 3800.0,
        frames: 60,
        usage: AiUsage {
            input_tokens: 1000,
            output_tokens: 200,
        },
        model: MODELS[0],
    }
}

fn static_clip() -> Description {
    Description {
        summary: "A locked-off shot of a tree.".to_string(),
        segments: Vec::new(),
        main: None,
    }
}

#[test]
fn format_names_and_extensions_are_distinct_per_format() {
    let names: Vec<&str> = Format::ALL.iter().map(|f| f.name()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len());
    assert_eq!(Format::Chapters.extension(), "txt");
    assert_eq!(Format::Markdown.extension(), "md");
}

#[test]
fn srt_has_numbered_cues_with_comma_milliseconds_and_hours() {
    let d = fixture();
    let out = render(&export(Path::new("clip.mp4"), &d, None), Format::Srt);
    assert_eq!(
        out,
        "1\n00:00:04,000 --> 00:00:51,500\nОн режет лук, быстро. Потом солит.\n\n\
         2\n01:02:05,250 --> 01:03:20,000\nSays \"hello\", then\nwaves & leaves <fast>\n\n"
    );
}

#[test]
fn vtt_uses_dots_and_a_header() {
    let d = fixture();
    let out = render(&export(Path::new("clip.mp4"), &d, None), Format::Vtt);
    assert_eq!(
        out,
        "WEBVTT\n\n00:00:04.000 --> 00:00:51.500\nОн режет лук, быстро. Потом солит.\n\n\
         01:02:05.250 --> 01:03:20.000\nSays \"hello\", then\nwaves &amp; leaves &lt;fast>\n"
    );
}

#[test]
fn a_static_clip_has_no_cues_and_no_rows() {
    let d = static_clip();
    let e = export(Path::new("tree.mp4"), &d, None);
    assert_eq!(render(&e, Format::Srt), "");
    assert_eq!(render(&e, Format::Vtt), "WEBVTT\n");
    assert_eq!(render(&e, Format::Chapters), "");
    assert_eq!(
        render(&e, Format::Csv),
        "kind,start_s,end_s,start,end,description,tags\n"
    );
    let md = render(&e, Format::Markdown);
    assert_eq!(md, "# tree.mp4\n\nA locked-off shot of a tree.\n");
    assert!(!render(&e, Format::Xmp).contains("Tracks"));
    assert!(render(&e, Format::Json).contains("\"main\": null"));
}

#[test]
fn csv_quotes_commas_quotes_and_line_breaks_and_lists_tags_by_overlap() {
    let d = fixture();
    let t = tags();
    let out = render(&export(Path::new("clip.mp4"), &d, Some(&t)), Format::Csv);
    assert_eq!(
        out,
        "kind,start_s,end_s,start,end,description,tags\n\
         main,4.000,3800.000,0:04,1:03:20,A cook plates a dish.,kitchen;waving\n\
         moment,4.000,51.500,0:04,0:51,\"Он режет лук, быстро. Потом солит.\",kitchen\n\
         moment,3725.250,3800.000,1:02:05,1:03:20,\"Says \"\"hello\"\", then\nwaves & leaves <fast>\",kitchen;waving\n"
    );
}

#[test]
fn chapters_start_at_zero_and_use_the_first_sentence() {
    let d = fixture();
    let out = render(&export(Path::new("clip.mp4"), &d, None), Format::Chapters);
    assert_eq!(
        out,
        "0:00 clip\n0:04 Он режет лук, быстро\n1:02:05 Says \"hello\", then waves & leaves <fast>\n"
    );
}

#[test]
fn chapters_do_not_add_a_line_when_the_first_moment_is_at_zero() {
    let d = Description {
        summary: String::new(),
        segments: vec![seg(0.0, 5.0, "Intro"), seg(5.0, 9.0, "Outro")],
        main: None,
    };
    let out = render(&export(Path::new("a.mp4"), &d, None), Format::Chapters);
    assert_eq!(out, "0:00 Intro\n0:05 Outro\n");
}

#[test]
fn a_long_title_is_cut_with_an_ellipsis() {
    let long = "word ".repeat(40);
    let t = title(&long, 20);
    assert_eq!(t.chars().count(), 20);
    assert!(t.ends_with('\u{2026}'));
    assert_eq!(title("One. Two.", 50), "One");
}

#[test]
fn markdown_lists_the_main_range_moments_and_tags() {
    let d = fixture();
    let t = tags();
    let out = render(
        &export(Path::new("clip.mp4"), &d, Some(&t)),
        Format::Markdown,
    );
    assert_eq!(
        out,
        "# clip.mp4\n\nA cook plates a dish.\n\n**Main range:** 0:04\u{2013}1:03:20\n\n## Moments\n\n\
         - **0:04\u{2013}0:51** Он режет лук, быстро. Потом солит.\n\
         - **1:02:05\u{2013}1:03:20** Says \"hello\", then waves & leaves <fast>\n\n\
         ## Tags\n\n- kitchen 90%\n- waving 55% (1:02:10\u{2013}1:02:20)\n\nNew tag ideas: onion\n"
    );
}

#[test]
fn text_matches_the_golden_file() {
    let d = fixture();
    let t = tags();
    let out = render(&export(Path::new("clip.mp4"), &d, Some(&t)), Format::Text);
    assert_eq!(out, golden(include_str!("golden/clip.txt")));
}

#[test]
fn json_matches_the_golden_file() {
    let d = fixture();
    let t = tags();
    let out = render(&export(Path::new("clip.mp4"), &d, Some(&t)), Format::Json);
    assert_eq!(out, golden(include_str!("golden/clip.json")));
}

#[test]
fn csv_neutralises_formulas() {
    assert_eq!(csv_field("=1+1"), "'=1+1");
    assert_eq!(csv_field("@SUM(A1)"), "'@SUM(A1)");
    assert_eq!(csv_field("-5, ok"), "\"'-5, ok\"");
    assert_eq!(csv_field("plain"), "plain");
}

#[test]
fn vtt_text_cannot_inject_timing_or_markup_and_blank_cues_are_dropped() {
    let d = Description {
        summary: String::new(),
        segments: vec![
            seg(1.0, 2.0, "a --> b <i>&"),
            seg(2.0, 3.0, "   \n "),
            seg(3.0, 4.0, "last"),
        ],
        main: None,
    };
    let e = export(Path::new("a.mp4"), &d, None);
    assert_eq!(
        render(&e, Format::Vtt),
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\na --&gt; b &lt;i>&amp;\n\n00:00:03.000 --> 00:00:04.000\nlast\n"
    );
    assert_eq!(
        render(&e, Format::Srt),
        "1\n00:00:01,000 --> 00:00:02,000\na --> b <i>&\n\n2\n00:00:03,000 --> 00:00:04,000\nlast\n\n",
        "SRT numbers the cues that are written"
    );
    assert_eq!(
        render(&e, Format::Chapters),
        "0:00 a\n0:01 a --> b <i>&\n0:03 last\n"
    );
}

#[test]
fn a_title_of_only_punctuation_is_kept_and_xml_drops_forbidden_characters() {
    assert_eq!(title("...", 50), "...");
    assert_eq!(xml_escape("a\u{fffe}b\u{1}c\u{1F600}"), "abc\u{1F600}");
}

#[test]
fn plan_names_each_format_once_and_removes_nothing_from_disk() {
    let video = Path::new("d/clip.mp4");
    let planned = plan(video, &[Format::Json, Format::Json, Format::Xmp], None);
    assert_eq!(
        planned,
        vec![
            (Format::Json, PathBuf::from("d/clip.clipscribe.json")),
            (Format::Xmp, PathBuf::from("d/clip.xmp")),
        ]
    );
}

#[test]
fn json_carries_main_moments_tags_and_usage() {
    let d = fixture();
    let t = tags();
    let value = export(Path::new("clip.mp4"), &d, Some(&t)).to_json();
    assert_eq!(value["main"]["start_s"], 4.0);
    assert_eq!(value["moments"].as_array().map(Vec::len), Some(2));
    assert_eq!(value["tags"][1]["ranges"][0]["end_s"], 3740.0);
    assert_eq!(value["new_tag_ideas"][0], "onion");
    assert_eq!(value["usage"]["input_tokens"], 1000);
    let plain = export(Path::new("clip.mp4"), &d, None).to_json();
    assert!(plain.get("tags").is_none());
    let text = render(&export(Path::new("clip.mp4"), &d, None), Format::Json);
    assert!(text.ends_with("}\n"));
    assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
}

#[test]
fn xmp_matches_the_golden_file_and_is_deterministic() {
    let d = fixture();
    let e = export(Path::new("clip.mp4"), &d, None);
    let out = render(&e, Format::Xmp);
    assert_eq!(out, render(&e, Format::Xmp));
    assert_eq!(out, golden(include_str!("golden/clip.xmp")));
}

#[test]
fn xmp_escapes_markup_and_uses_cr_for_line_breaks() {
    let d = fixture();
    let out = render(&export(Path::new("clip.mp4"), &d, None), Format::Xmp);
    assert!(out.contains("waves &amp; leaves &lt;fast&gt;"));
    assert!(out.contains("then&#xD;waves"));
    assert!(out.contains("&quot;hello&quot;"));
    assert!(
        out.contains("xmpDM:startTime=\"4000\" xmpDM:duration=\"3796000\" xmpDM:name=\"in-out\"")
    );
}

#[test]
fn guids_are_stable_and_shaped_like_uuids() {
    let a = guid("clip|0|4000|x");
    assert_eq!(a, guid("clip|0|4000|x"));
    assert_ne!(a, guid("clip|1|4000|x"));
    let parts: Vec<usize> = a.split('-').map(str::len).collect();
    assert_eq!(parts, [8, 4, 4, 4, 12]);
}

#[test]
fn output_names_avoid_the_videos_own_subtitles() {
    let video = Path::new("/v/clip.mp4");
    let all = [Format::Srt, Format::Json];
    assert_eq!(
        output_path(video, None, Format::Srt, &all),
        PathBuf::from("/v/clip.clipscribe.srt")
    );
    assert_eq!(
        output_path(video, Some(Path::new("out")), Format::Json, &all),
        PathBuf::from("out/clip.clipscribe.json")
    );
}

#[test]
fn formats_sharing_an_extension_get_the_format_in_the_name() {
    let video = Path::new("clip.mp4");
    let both = [Format::Text, Format::Chapters];
    assert_eq!(
        output_path(video, None, Format::Text, &both),
        PathBuf::from("clip.txt.txt")
    );
    assert_eq!(
        output_path(video, None, Format::Chapters, &both),
        PathBuf::from("clip.chapters.txt")
    );
    assert_eq!(
        output_path(video, None, Format::Chapters, &[Format::Chapters]),
        PathBuf::from("clip.clipscribe.txt")
    );
}

#[test]
fn the_xmp_sidecar_is_named_after_the_video_only() {
    assert_eq!(
        output_path(
            Path::new("d/clip.mp4"),
            None,
            Format::Xmp,
            &[Format::Xmp, Format::Json]
        ),
        PathBuf::from("d/clip.xmp")
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clipscribe-export-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn write_all_writes_every_format_once_and_skips_existing_files_without_force() {
    let dir = scratch("write");
    let video = dir.join("clip.mp4");
    let d = fixture();
    let e = export(&video, &d, None);
    let formats = [Format::Json, Format::Srt, Format::Csv, Format::Json];

    let first = write_all(&e, &formats, None, false).expect("write");
    assert_eq!(first.len(), 3, "a repeated format is written once");
    assert!(first.iter().all(|w| !w.skipped));
    for w in &first {
        assert_eq!(
            std::fs::read_to_string(&w.path).expect("read"),
            render(&e, w.format)
        );
    }
    assert!(dir.join("clip.clipscribe.srt").exists());

    std::fs::write(dir.join("clip.clipscribe.srt"), "mine").expect("edit");
    let second = write_all(&e, &formats, None, false).expect("write");
    assert!(second.iter().all(|w| w.skipped));
    assert_eq!(
        std::fs::read_to_string(dir.join("clip.clipscribe.srt")).expect("read"),
        "mine"
    );

    let forced = write_all(&e, &formats, None, true).expect("write");
    assert!(forced.iter().all(|w| !w.skipped));
    assert_ne!(
        std::fs::read_to_string(dir.join("clip.clipscribe.srt")).expect("read"),
        "mine"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_all_creates_the_out_dir_and_never_touches_the_videos_own_srt() {
    let dir = scratch("outdir");
    let video = dir.join("clip.mp4");
    std::fs::write(dir.join("clip.srt"), "speech").expect("speech subtitles");
    let d = fixture();
    let out = dir.join("nested").join("out");
    let written = write_all(
        &export(&video, &d, None),
        &[Format::Srt, Format::Xmp],
        Some(&out),
        false,
    )
    .expect("write");
    assert_eq!(written[0].path, out.join("clip.clipscribe.srt"));
    assert_eq!(written[1].path, out.join("clip.xmp"));
    assert!(written[0].path.exists() && written[1].path.exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("clip.srt")).expect("read"),
        "speech"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn chapters_look_at_the_first_moment_that_has_text() {
    let d = Description {
        summary: String::new(),
        segments: vec![seg(0.0, 5.0, "  "), seg(30.0, 40.0, "Late start")],
        main: None,
    };
    let out = render(&export(Path::new("a.mp4"), &d, None), Format::Chapters);
    assert_eq!(out, "0:00 a\n0:30 Late start\n");
}
