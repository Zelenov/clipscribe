//! Debugging: write the frames a request sends to a folder, so you can see exactly which images
//! the model got.
//!
//! Off by default. Turn it on with [`set_debug_frames_dir`], or with the environment variable
//! `CLIPSCRIBE_DEBUG_FRAMES=<dir>` (the setter wins). It is not an `Options` field, so turning
//! it on breaks no caller that builds `Options`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::json;

use crate::Frame;

/// The environment variable that turns frame dumping on.
pub const DEBUG_FRAMES_ENV: &str = "CLIPSCRIBE_DEBUG_FRAMES";

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Write the frames of every request that follows to a subfolder of `dir` (`Some`), or stop doing
/// so (`None`, then only the environment variable applies). Each `describe`,
/// `describe_with_tags` and `describe_moment` call writes `<dir>/<video name without extension>`
/// (`<name>-at-<seconds>s` for a moment): the frames as the same JPEG bytes that were sent,
/// named by time (`0012.40s.jpg`), and a `frames.json` with each frame's time, how it was
/// picked (`key_frame`, `interval` or `moment`) and its size. A run over the same video replaces
/// the previous files. A write error is only logged: it never fails the description.
pub fn set_debug_frames_dir(dir: Option<PathBuf>) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// The folder frames are written to, if dumping is on.
pub fn debug_frames_dir() -> Option<PathBuf> {
    let set = DIR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    set.or_else(|| {
        std::env::var_os(DEBUG_FRAMES_ENV)
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    })
}

/// Write `frames` for the request about `name` when dumping is on.
#[cfg(feature = "frames")]
pub(crate) fn dump_if_on(name: &str, frames: &[Frame], picked: &str) {
    if let Some(dir) = debug_frames_dir() {
        dump_frames(&dir, name, frames, picked);
    }
}

/// The folder name for a video: its file name without the extension.
#[cfg(feature = "frames")]
pub(crate) fn name_of(video: &Path) -> String {
    video
        .file_stem()
        .map_or_else(|| "clip".to_string(), |s| s.to_string_lossy().into_owned())
}

/// Write `frames` and `frames.json` to `<dir>/<name>`; errors are logged, never returned.
pub fn dump_frames(dir: &Path, name: &str, frames: &[Frame], picked: &str) {
    if let Err(e) = try_dump(&dir.join(name), frames, picked) {
        log::warn!(
            "clipscribe: frames not saved to {}: {e}",
            dir.join(name).display()
        );
    }
}

fn try_dump(folder: &Path, frames: &[Frame], picked: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(folder)?;
    // A run over the same video replaces the last one, so old frames do not mix in.
    for entry in std::fs::read_dir(folder)?.flatten() {
        let path = entry.path();
        let ours = path.extension().is_some_and(|e| e == "jpg" || e == "json");
        if ours && path.is_file() {
            let _ = std::fs::remove_file(path);
        }
    }
    let mut names: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    for frame in frames {
        let mut file = format!("{:07.2}s.jpg", frame.time_s);
        let mut n = 1;
        while names.contains(&file) {
            n += 1;
            file = format!("{:07.2}s-{n}.jpg", frame.time_s);
        }
        std::fs::write(folder.join(&file), &frame.jpeg)?;
        entries.push(json!({
            "file": file,
            "time_s": frame.time_s,
            "picked": picked,
            "bytes": frame.jpeg.len(),
        }));
        names.push(file);
    }
    let index = serde_json::to_string_pretty(&entries).unwrap_or_default();
    std::fs::write(folder.join("frames.json"), index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AiContent;
    use crate::{build_request, Model, MomentsMode, SummaryLanguage};

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clipscribe-dump-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn frames() -> Vec<Frame> {
        vec![
            Frame {
                time_s: 0.5,
                jpeg: vec![0xff, 0xd8, 1],
            },
            Frame {
                time_s: 12.4,
                jpeg: vec![0xff, 0xd8, 2, 2],
            },
        ]
    }

    #[test]
    fn the_files_are_the_bytes_the_request_carries() {
        let dir = temp("match");
        let frames = frames();
        dump_frames(&dir, "clip", &frames, "key_frame");
        let request = build_request(
            Model::default(),
            &frames,
            &[],
            20.0,
            SummaryLanguage::English,
            MomentsMode::Important,
        );
        let sent: Vec<&Vec<u8>> = request
            .content
            .iter()
            .filter_map(|c| match c {
                AiContent::Jpeg(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            std::fs::read(dir.join("clip/0000.50s.jpg")).expect("first"),
            *sent[0]
        );
        assert_eq!(
            std::fs::read(dir.join("clip/0012.40s.jpg")).expect("second"),
            *sent[1]
        );
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("clip/frames.json")).expect("index"))
                .expect("json");
        assert_eq!(index[1]["file"], "0012.40s.jpg");
        assert_eq!(index[1]["time_s"], 12.4);
        assert_eq!(index[1]["picked"], "key_frame");
        assert_eq!(index[1]["bytes"], 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_replaces_the_first() {
        let dir = temp("replace");
        dump_frames(&dir, "clip", &frames(), "interval");
        dump_frames(&dir, "clip", &frames()[..1], "interval");
        assert!(!dir.join("clip/0012.40s.jpg").exists());
        assert!(dir.join("clip/0000.50s.jpg").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_error_is_not_an_error() {
        let dir = temp("blocked");
        std::fs::create_dir_all(&dir).expect("dir");
        // A file where the folder should be.
        let blocker = dir.join("clip");
        std::fs::write(&blocker, b"x").expect("file");
        dump_frames(&dir, "clip", &frames(), "interval");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "frames")]
    fn the_setter_turns_it_on_and_off() {
        let dir = temp("setter");
        set_debug_frames_dir(Some(dir.clone()));
        assert_eq!(debug_frames_dir(), Some(dir.clone()));
        dump_if_on("clip", &frames(), "interval");
        assert!(dir.join("clip/frames.json").exists());
        set_debug_frames_dir(None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
