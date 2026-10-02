//! The command line's progress display on a real run, without a request: a file that is not a
//! video fails before anything is sent.

use std::path::PathBuf;
use std::process::{Command, Output};

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

/// A folder with a file named like a video that is not one.
fn not_a_video(name: &str) -> (PathBuf, PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("clipscribe-progress-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("\u{43f}\u{43b}\u{43e}\u{445}\u{43e}\u{435}.mp4");
    std::fs::write(&file, "this is not a video").expect("file");
    (dir, file)
}

#[test]
fn json_on_stdout_stays_parseable_and_stderr_has_only_the_result_line_without_a_terminal() {
    let (dir, file) = not_a_video("json");
    let out = run(&[
        "--api-key",
        "unused",
        "--json",
        file.to_str().expect("utf8"),
    ]);
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(parsed, serde_json::json!([]));
    assert!(
        !stderr.contains('\u{1b}'),
        "no escape codes without a terminal: {stderr:?}"
    );
    // Without a terminal: no bars, no plan, no summary; only the result line, whole.
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.starts_with("\u{2717} "), "{stderr}");
    assert!(
        stderr.contains("\u{43f}\u{43b}\u{43e}\u{445}\u{43e}\u{435}.mp4"),
        "{stderr}"
    );
    assert!(
        stderr.contains("could not be read ("),
        "the error is whole: {stderr}"
    );
    assert!(
        !stderr.contains("video: ") && !stderr.contains("done: "),
        "{stderr}"
    );
    assert_eq!(out.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quiet_gives_the_same_result_line_and_the_same_json() {
    let (dir, file) = not_a_video("quiet");
    let path = file.to_str().expect("utf8");
    let loud = run(&["--api-key", "unused", "--json", path]);
    let quiet = run(&["--api-key", "unused", "--quiet", "--json", path]);
    assert_eq!(text(&quiet.stdout), text(&loud.stdout));
    assert_eq!(text(&quiet.stderr), text(&loud.stderr));
    assert_eq!(text(&quiet.stderr).lines().count(), 1);
    assert_eq!(quiet.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}
