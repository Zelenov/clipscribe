//! The `--format` command line, without a request: `--estimate` and the refusals.

use std::path::PathBuf;
use std::process::{Command, Output};

fn clip() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/clips/rotated-90.mp4")
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clipscribe"))
        .args(args)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run clipscribe")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("clipscribe-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

// `--estimate` reads the clip with GStreamer; like the other decoding tests it runs on Linux, where
// CI decodes the test clips. The file listing itself is covered by the unit tests in main.rs.
#[cfg(target_os = "linux")]
#[test]
fn estimate_lists_the_files_and_notes_existing_ones() {
    let dir = scratch("estimate");
    let video = dir.join("clip.mp4");
    std::fs::copy(clip(), &video).expect("copy");
    let video = video.to_str().expect("utf8");
    let out = run(&[video, "--estimate", "--format", "json,srt,txt,chapters"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("would write") && stdout.contains("clip.clipscribe.json"));
    assert!(stdout.contains("clip.clipscribe.srt"));
    assert!(stdout.contains("clip.txt.txt") && stdout.contains("clip.chapters.txt"));
    assert!(!stdout.contains("exists"));
    assert!(
        dir.read_dir().expect("dir").count() == 1,
        "nothing is written"
    );

    for name in [
        "clip.clipscribe.json",
        "clip.clipscribe.srt",
        "clip.txt.txt",
        "clip.chapters.txt",
    ] {
        std::fs::write(dir.join(name), "").expect("existing");
    }
    let stdout = text(&run(&[video, "--estimate", "--format", "json,srt,txt,chapters"]).stdout);
    assert!(stdout.contains("not described again"), "{stdout}");
    assert!(stdout.contains("(exists, would be skipped)"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn existing_files_are_skipped_without_a_key_or_a_request() {
    let dir = scratch("skip");
    let video = dir.join("clip.mp4");
    std::fs::copy(clip(), &video).expect("copy");
    std::fs::write(dir.join("clip.clipscribe.json"), "mine").expect("existing");
    let out = run(&[video.to_str().expect("utf8"), "--format", "json"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("skipped"));
    assert_eq!(
        std::fs::read_to_string(dir.join("clip.clipscribe.json")).expect("read"),
        "mine"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn colliding_outputs_and_an_unwritable_folder_fail_before_any_request() {
    let dir = scratch("refuse");
    let a = dir.join("a");
    let b = dir.join("b");
    std::fs::create_dir_all(&a).expect("a");
    std::fs::create_dir_all(&b).expect("b");
    std::fs::copy(clip(), a.join("clip.mp4")).expect("copy");
    std::fs::copy(clip(), b.join("clip.mp4")).expect("copy");
    let out_dir = dir.join("out");
    let args = [
        a.join("clip.mp4").to_str().expect("utf8").to_string(),
        b.join("clip.mp4").to_str().expect("utf8").to_string(),
        "--format".into(),
        "json".into(),
        "--out-dir".into(),
        out_dir.to_str().expect("utf8").into(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run(&args);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("would both write"),
        "{}",
        text(&out.stderr)
    );

    // The folder is a file: it can neither be created nor written to.
    std::fs::write(&out_dir, "").expect("a file in the way");
    let out = run(&[
        a.join("clip.mp4").to_str().expect("utf8"),
        "--api-key",
        "unused",
        "--format",
        "json",
        "--out-dir",
        out_dir.to_str().expect("utf8"),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("cannot be written to"),
        "{}",
        text(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
