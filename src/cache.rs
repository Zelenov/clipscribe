//! The cache of a folder run: described clips kept in a JSON Lines file, one line per clip, looked
//! up by the file's identity (size, modification time and a hash of a sample of its content) and
//! how it was described. See [`Cache`] and the design notes in `docs/design/whole-folders.md`.
//!
//! No GStreamer dependency: a program can read a cache, and group what is in it, without the
//! `frames` feature.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::Engine;
use serde_json::{json, Value};

use crate::describe::{Description, FrameSampling, MomentsMode, Segment};
use crate::folder::{DescribedClip, FrameFingerprint};
use crate::provider::AiUsage;
use crate::tags::{Tag, TagRange, TagSuggestion, TagSuggestions};
use crate::Options;

/// The cache's file name, in a video's folder or in the directory given instead; see
/// [`cache_path`].
pub const CACHE_FILE_NAME: &str = ".clipscribe-cache.jsonl";
/// The format of the lines this build writes. Lines of a newer format are kept but not read.
const FORMAT_VERSION: u64 = 1;
/// The content sample of [`FileIdentity`]: this many bytes at the start, the middle and the end.
const SAMPLE_CHUNK: u64 = 64 * 1024;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The cache file for the videos of `folder`: in `cache_dir` when given, else in `folder` itself.
pub fn cache_path(folder: &Path, cache_dir: Option<&Path>) -> PathBuf {
    cache_dir.unwrap_or(folder).join(CACHE_FILE_NAME)
}

/// What makes a file the same file for the cache, without reading all of it: its size, its
/// modification time, and a hash of a sample of its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// The file's length, in bytes.
    pub size: u64,
    /// Nanoseconds since the Unix epoch; 0 when the file system cannot tell (or before 1970).
    pub modified_ns: u64,
    /// FNV-1a 64 of the first, middle and last 64 KiB of the file (all of it when it is 192 KiB or
    /// smaller). Fixed forever, unlike `std`'s hasher, since the cache outlives the build.
    pub sample_hash: u64,
}

impl FileIdentity {
    /// The identity of the file at `path` now: at most three 64 KiB reads.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len();
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let mut file = File::open(path)?;
        let starts = if size <= 3 * SAMPLE_CHUNK {
            vec![(0, size)]
        } else {
            vec![
                (0, SAMPLE_CHUNK),
                (size / 2 - SAMPLE_CHUNK / 2, SAMPLE_CHUNK),
                (size - SAMPLE_CHUNK, SAMPLE_CHUNK),
            ]
        };
        let mut hash = FNV_OFFSET;
        let mut buffer = Vec::new();
        for (start, length) in starts {
            file.seek(SeekFrom::Start(start))?;
            buffer.clear();
            (&mut file).take(length).read_to_end(&mut buffer)?;
            hash = fnv1a(hash, &buffer);
        }
        Ok(Self {
            size,
            modified_ns,
            sample_hash: hash,
        })
    }
}

/// FNV-1a 64 of `bytes`, continuing from `hash`.
fn fnv1a(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
    })
}

/// How the cache finds a clip: the video's identity, and a line of the settings its description
/// depends on (the model, the language, the frame sampling, the moments mode, the vocabulary and
/// the subtitles). An entry is used only when both match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// Which file it is: see [`FileIdentity`].
    pub identity: FileIdentity,
    /// e.g. `model=claude-haiku-4-5 language=en frames=keyframes moments=important tags=none
    /// subtitles=none`.
    pub settings: String,
}

impl CacheKey {
    /// The key of `video` described with `options` (and tag suggestions from `vocabulary`, when
    /// given), with the `.srt` next to it when `subtitles` is set: the subtitles' own identity is
    /// part of the settings, so editing them describes the clip again.
    pub fn new(
        video: &Path,
        options: &Options,
        vocabulary: Option<&[Tag]>,
        subtitles: bool,
    ) -> std::io::Result<Self> {
        let identity = FileIdentity::of(video)?;
        let subtitles = if !subtitles {
            "off".to_string()
        } else {
            match FileIdentity::of(&crate::srt::subtitle_path(video)) {
                Ok(srt) => format!("{}-{}-{:016x}", srt.size, srt.modified_ns, srt.sample_hash),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => "none".to_string(),
                Err(_) => "unreadable".to_string(),
            }
        };
        let tags = match vocabulary {
            None => "none".to_string(),
            Some(vocabulary) => {
                let hash = vocabulary.iter().fold(FNV_OFFSET, |hash, tag| {
                    let hash = fnv1a(hash, tag.name.as_bytes());
                    let hash = fnv1a(hash, &[0x1f]);
                    let hash = fnv1a(hash, tag.hint.as_deref().unwrap_or_default().as_bytes());
                    fnv1a(hash, &[0x1e])
                });
                format!("{hash:016x}")
            }
        };
        let frames = match options.frame_sampling {
            FrameSampling::KeyFrames => "keyframes",
            FrameSampling::Interval => "interval",
        };
        let moments = match options.moments {
            MomentsMode::Important => "important",
            MomentsMode::Full => "full",
        };
        Ok(Self {
            identity,
            settings: format!(
                "model={} language={} frames={frames} moments={moments} tags={tags} \
                 subtitles={subtitles}",
                options.model.id,
                options.language.as_str(),
            ),
        })
    }
}

/// A described clip as the cache keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipRecord {
    /// The video's path when it was described; only for reading the cache by eye (entries are
    /// found by identity, so a renamed file still finds its own).
    pub file: PathBuf,
    /// How the cache finds it: the file's identity and the settings it was described with.
    pub key: CacheKey,
    /// The model id it was described with, to price [`DescribedClip::usage`].
    pub model: String,
    pub clip: DescribedClip,
}

/// The described clips of a folder, in a JSON Lines file (see [`cache_path`]); shared by the
/// workers of a run (every method takes `&self`).
///
/// Each described clip is appended as one complete line and synced to disk before it counts, so a
/// crash or a cancel never loses a finished clip and never leaves half an entry that reads as
/// valid. Opening the file drops lines that do not read (a torn last line), entries replaced by a
/// later one for the same file, and rewrites the file without them (to a temporary file, renamed
/// over it). Lines of a newer format than this build knows are kept as they are. Two processes
/// writing one cache at once is not supported.
pub struct Cache {
    path: PathBuf,
    state: Mutex<State>,
}

struct State {
    entries: HashMap<FileIdentity, ClipRecord>,
    file: File,
    /// A write failed part way and the fragment could not be cut off again: the next line starts
    /// with a newline of its own, so it is not glued to the fragment.
    torn: bool,
}

impl Cache {
    /// Open (or create) the cache at `path`, creating its directory if needed.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let text = match std::fs::read(path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let torn = !text.is_empty() && !text.ends_with('\n');
        let mut lines = 0;
        // Each kept line's position, so a rewrite keeps the file's order.
        let mut entries: HashMap<FileIdentity, (usize, ClipRecord)> = HashMap::new();
        let mut newer: Vec<(usize, String)> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            lines += 1;
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match value["v"].as_u64() {
                Some(FORMAT_VERSION) => {
                    if let Some(record) = record_from_json(&value) {
                        entries.insert(record.key.identity, (n, record));
                    }
                }
                Some(v) if v > FORMAT_VERSION => newer.push((n, line.to_string())),
                _ => {}
            }
        }
        if torn || entries.len() + newer.len() != lines {
            let mut kept: Vec<(usize, String)> = entries
                .values()
                .map(|(n, record)| (*n, record_to_json(record).to_string()))
                .chain(newer)
                .collect();
            kept.sort_by_key(|(n, _)| *n);
            let mut contents = String::new();
            for (_, line) in kept {
                contents.push_str(&line);
                contents.push('\n');
            }
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(".tmp");
            let temporary = path.with_file_name(name);
            {
                let mut file = File::create(&temporary)?;
                file.write_all(contents.as_bytes())?;
                file.sync_all()?;
            }
            std::fs::rename(&temporary, path)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(State {
                entries: entries
                    .into_iter()
                    .map(|(identity, (_, record))| (identity, record))
                    .collect(),
                file,
                torn: false,
            }),
        })
    }

    /// Where the cache file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The clip described with `key`, if the cache has it: same file identity *and* same settings.
    pub fn get(&self, key: &CacheKey) -> Option<ClipRecord> {
        let state = self.lock();
        state
            .entries
            .get(&key.identity)
            .filter(|record| record.key.settings == key.settings)
            .cloned()
    }

    /// Add `record`, replacing any earlier entry for the same file: appended as one line and synced
    /// to disk before this returns. A write that fails part way (a full disk) is cut off again, so
    /// the next line is not glued to the fragment and lost with it on the next open.
    pub fn put(&self, record: &ClipRecord) -> std::io::Result<()> {
        let mut line = record_to_json(record).to_string();
        line.push('\n');
        let mut state = self.lock();
        if state.torn {
            line.insert(0, '\n');
        }
        let before = state.file.metadata().map(|m| m.len());
        let written = state
            .file
            .write_all(line.as_bytes())
            .and_then(|()| state.file.sync_data());
        if let Err(e) = written {
            let cut = before.and_then(|len| state.file.set_len(len));
            state.torn = state.torn || cut.is_err();
            return Err(e);
        }
        state.torn = false;
        state.entries.insert(record.key.identity, record.clone());
        Ok(())
    }

    /// How many clips the cache has.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the cache has no clips at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn record_to_json(record: &ClipRecord) -> Value {
    let clip = &record.clip;
    let identity = &record.key.identity;
    json!({
        "v": FORMAT_VERSION,
        "size": identity.size,
        "modified_ns": identity.modified_ns,
        "sample_hash": format!("{:016x}", identity.sample_hash),
        "settings": record.key.settings,
        "file": record.file.to_string_lossy(),
        "model": record.model,
        "duration_s": clip.duration_s,
        "usage": {
            "input_tokens": clip.usage.input_tokens,
            "output_tokens": clip.usage.output_tokens,
        },
        "summary": clip.description.summary,
        "segments": clip.description.segments.iter().map(|s| json!({
            "start_s": s.start_s,
            "end_s": s.end_s,
            "description": s.description,
        })).collect::<Vec<_>>(),
        "tags": clip.tags.as_ref().map(|tags| json!({
            "tags": tags.tags.iter().map(|t| json!({
                "name": t.name,
                "confidence": t.confidence,
                "ranges": t.ranges.iter().map(|r| json!({
                    "start_s": r.start_s,
                    "end_s": r.end_s,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "new_tag_ideas": tags.new_tag_ideas,
        })),
        "frames": clip.frames.iter().map(|f| json!({
            "t": f.time_s,
            "fp": base64::engine::general_purpose::STANDARD.encode(&f.fingerprint),
        })).collect::<Vec<_>>(),
    })
}

/// A record read back from [`record_to_json`]'s shape; `None` when anything is missing.
fn record_from_json(value: &Value) -> Option<ClipRecord> {
    let f64_of = |v: &Value| v.as_f64();
    let string = |v: &Value| v.as_str().map(str::to_string);
    let identity = FileIdentity {
        size: value["size"].as_u64()?,
        modified_ns: value["modified_ns"].as_u64()?,
        sample_hash: u64::from_str_radix(value["sample_hash"].as_str()?, 16).ok()?,
    };
    let segments = value["segments"]
        .as_array()?
        .iter()
        .map(|s| {
            Some(Segment {
                start_s: f64_of(&s["start_s"])?,
                end_s: f64_of(&s["end_s"])?,
                description: string(&s["description"])?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let tags = match &value["tags"] {
        Value::Null => None,
        tags => Some(TagSuggestions {
            tags: tags["tags"]
                .as_array()?
                .iter()
                .map(|t| {
                    Some(TagSuggestion {
                        name: string(&t["name"])?,
                        confidence: f64_of(&t["confidence"])?,
                        ranges: t["ranges"]
                            .as_array()?
                            .iter()
                            .map(|r| {
                                Some(TagRange {
                                    start_s: f64_of(&r["start_s"])?,
                                    end_s: f64_of(&r["end_s"])?,
                                })
                            })
                            .collect::<Option<Vec<_>>>()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            new_tag_ideas: tags["new_tag_ideas"]
                .as_array()?
                .iter()
                .map(string)
                .collect::<Option<Vec<_>>>()?,
        }),
    };
    let frames = value["frames"]
        .as_array()?
        .iter()
        .map(|f| {
            Some(FrameFingerprint {
                time_s: f64_of(&f["t"])?,
                fingerprint: base64::engine::general_purpose::STANDARD
                    .decode(f["fp"].as_str()?)
                    .ok()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ClipRecord {
        file: PathBuf::from(value["file"].as_str()?),
        key: CacheKey {
            identity,
            settings: string(&value["settings"])?,
        },
        model: string(&value["model"])?,
        clip: DescribedClip {
            description: Description {
                summary: string(&value["summary"])?,
                segments,
            },
            tags,
            usage: AiUsage {
                input_tokens: value["usage"]["input_tokens"].as_u64()?,
                output_tokens: value["usage"]["output_tokens"].as_u64()?,
            },
            duration_s: f64_of(&value["duration_s"])?,
            frames,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Model, SummaryLanguage};

    /// A fresh directory under the system's temp dir, removed by the caller.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clipscribe-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn options() -> Options {
        Options {
            api_key: "k".to_string(),
            model: Model::default(),
            language: SummaryLanguage::English,
            frame_sampling: FrameSampling::KeyFrames,
            moments: MomentsMode::Important,
        }
    }

    fn record(identity: FileIdentity, summary: &str) -> ClipRecord {
        ClipRecord {
            file: PathBuf::from("footage/goat.mp4"),
            key: CacheKey {
                identity,
                settings: "model=claude-haiku-4-5 language=en".to_string(),
            },
            model: "claude-haiku-4-5".to_string(),
            clip: DescribedClip {
                description: Description {
                    summary: summary.to_string(),
                    segments: vec![Segment {
                        start_s: 1.0,
                        end_s: 2.5,
                        description: "Привет — a goat looks up.".to_string(),
                    }],
                },
                tags: Some(TagSuggestions {
                    tags: vec![TagSuggestion {
                        name: "Goat".to_string(),
                        confidence: 0.9,
                        ranges: vec![TagRange {
                            start_s: 1.0,
                            end_s: 2.0,
                        }],
                    }],
                    new_tag_ideas: vec!["Fence".to_string()],
                }),
                usage: AiUsage {
                    input_tokens: 4000,
                    output_tokens: 120,
                },
                duration_s: 12.5,
                frames: vec![FrameFingerprint {
                    time_s: 0.25,
                    fingerprint: (0..64).collect(),
                }],
            },
        }
    }

    fn identity(n: u64) -> FileIdentity {
        FileIdentity {
            size: n,
            modified_ns: 1_700_000_000_000_000_000 + n,
            sample_hash: u64::MAX - n,
        }
    }

    #[test]
    fn the_identity_changes_with_the_content_the_size_or_the_time_but_not_the_name() {
        let dir = temp_dir("identity");
        let path = dir.join("a.mp4");
        std::fs::write(&path, vec![7u8; 1000]).expect("write");
        let first = FileIdentity::of(&path).expect("identity");
        assert_eq!(first, FileIdentity::of(&path).expect("again"), "stable");
        assert_eq!(first.size, 1000);

        let renamed = dir.join("b.mp4");
        std::fs::rename(&path, &renamed).expect("rename");
        assert_eq!(FileIdentity::of(&renamed).expect("renamed"), first);

        // Same size, same time, different bytes: only the sample hash tells them apart.
        let modified = std::fs::metadata(&renamed)
            .and_then(|m| m.modified())
            .expect("mtime");
        std::fs::write(&renamed, vec![8u8; 1000]).expect("rewrite");
        File::options()
            .write(true)
            .open(&renamed)
            .and_then(|f| f.set_modified(modified))
            .expect("set mtime");
        let rewritten = FileIdentity::of(&renamed).expect("rewritten");
        assert_eq!(
            (rewritten.size, rewritten.modified_ns),
            (first.size, first.modified_ns)
        );
        assert_ne!(rewritten.sample_hash, first.sample_hash);

        std::fs::write(&renamed, vec![8u8; 1001]).expect("grow");
        assert_ne!(FileIdentity::of(&renamed).expect("grown").size, first.size);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A large file is sampled at its start, middle and end: a change in each changes the hash,
    /// a change between the samples does not (that is the price of not reading gigabytes).
    #[test]
    fn a_large_file_is_hashed_from_its_start_middle_and_end() {
        let dir = temp_dir("sample");
        let path = dir.join("big.mov");
        let size = 1_000_000usize;
        let hash_with = |at: Option<usize>| {
            let mut bytes = vec![0u8; size];
            if let Some(at) = at {
                bytes[at] = 1;
            }
            std::fs::write(&path, &bytes).expect("write");
            FileIdentity::of(&path).expect("identity").sample_hash
        };
        let plain = hash_with(None);
        assert_ne!(hash_with(Some(10)), plain, "start");
        assert_ne!(hash_with(Some(size / 2)), plain, "middle");
        assert_ne!(hash_with(Some(size - 10)), plain, "end");
        assert_eq!(hash_with(Some(size / 4)), plain, "between the samples");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fnv1a_matches_its_published_test_vectors() {
        // From the FNV reference test suite (isthe.com/chongo/src/fnv/test_fnv.c).
        assert_eq!(fnv1a(FNV_OFFSET, b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(FNV_OFFSET, b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(FNV_OFFSET, b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn the_settings_change_with_what_the_description_depends_on() {
        let dir = temp_dir("settings");
        let video = dir.join("clip.mp4");
        std::fs::write(&video, b"video").expect("write");
        let base = CacheKey::new(&video, &options(), None, true).expect("key");
        assert_eq!(
            base.settings,
            "model=claude-haiku-4-5 language=en frames=keyframes moments=important tags=none \
             subtitles=none"
        );
        let other_model = Options {
            model: Model::from_id("claude-sonnet-5"),
            ..options()
        };
        let variants = [
            CacheKey::new(&video, &other_model, None, true),
            CacheKey::new(&video, &options(), None, false),
            CacheKey::new(
                &video,
                &options(),
                Some(&[Tag {
                    name: "Goat".to_string(),
                    hint: None,
                }]),
                true,
            ),
        ];
        for variant in variants {
            let variant = variant.expect("key");
            assert_eq!(variant.identity, base.identity);
            assert_ne!(variant.settings, base.settings);
        }
        std::fs::write(
            dir.join("clip.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nHi\n",
        )
        .expect("srt");
        let with_srt = CacheKey::new(&video, &options(), None, true).expect("key");
        assert_ne!(
            with_srt.settings, base.settings,
            "the subtitles are part of it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_round_trips_through_a_line() {
        let record = record(identity(1), "A goat in a field.");
        let line = record_to_json(&record).to_string();
        assert!(!line.contains('\n'), "one line");
        let back = record_from_json(&serde_json::from_str(&line).expect("json")).expect("record");
        assert_eq!(back, record);
        let no_tags = ClipRecord {
            clip: DescribedClip {
                tags: None,
                ..record.clip.clone()
            },
            ..record
        };
        let back = record_from_json(&record_to_json(&no_tags)).expect("record");
        assert_eq!(back, no_tags);
    }

    #[test]
    fn a_hit_needs_the_same_identity_and_settings_and_survives_reopening() {
        let dir = temp_dir("hit");
        let path = cache_path(&dir, None);
        let cache = Cache::open(&path).expect("open");
        assert!(cache.is_empty());
        let stored = record(identity(1), "A goat in a field.");
        cache.put(&stored).expect("put");
        assert_eq!(cache.get(&stored.key), Some(stored.clone()));
        let other_settings = CacheKey {
            settings: "model=claude-opus-5 language=en".to_string(),
            ..stored.key.clone()
        };
        assert_eq!(
            cache.get(&other_settings),
            None,
            "same file, other settings"
        );
        let other_file = CacheKey {
            identity: identity(2),
            ..stored.key.clone()
        };
        assert_eq!(cache.get(&other_file), None);
        drop(cache);

        let reopened = Cache::open(&path).expect("reopen");
        assert_eq!(reopened.get(&stored.key), Some(stored));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a crash in the middle of a write leaves behind — the last line cut off — is skipped,
    /// the file is rewritten without it, and the next entry lands on a line of its own instead of
    /// being glued to the torn one.
    #[test]
    fn a_torn_last_line_is_dropped_and_the_next_entry_is_not_glued_to_it() {
        let dir = temp_dir("torn");
        let path = dir.join(CACHE_FILE_NAME);
        let first = record(identity(1), "First.");
        let torn = record_to_json(&record(identity(2), "Second.")).to_string();
        std::fs::write(
            &path,
            format!(
                "{}\nnot json at all\n{}",
                record_to_json(&first),
                &torn[..torn.len() / 2]
            ),
        )
        .expect("write");

        let cache = Cache::open(&path).expect("open");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&first.key), Some(first.clone()));
        let third = record(identity(3), "Third.");
        cache.put(&third).expect("put");
        drop(cache);

        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.ends_with('\n'));
        let reopened = Cache::open(&path).expect("reopen");
        assert_eq!(reopened.get(&first.key), Some(first));
        assert_eq!(reopened.get(&third.key), Some(third));
        assert!(!dir.join(format!("{CACHE_FILE_NAME}.tmp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write that failed part way and could not be cut off leaves a fragment without a newline:
    /// the next entry starts on a line of its own, so reopening loses only the fragment, never
    /// the finished clip written after it.
    #[test]
    fn an_entry_after_a_failed_write_is_not_glued_to_its_fragment() {
        let dir = temp_dir("fragment");
        let path = dir.join(CACHE_FILE_NAME);
        let cache = Cache::open(&path).expect("open");
        let first = record(identity(1), "First.");
        cache.put(&first).expect("put");
        // What a write cut short by a full disk leaves, with the cut-off failing too.
        let fragment = record_to_json(&record(identity(2), "Second.")).to_string();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(&fragment.as_bytes()[..fragment.len() / 2]))
            .expect("fragment");
        cache.lock().torn = true;
        let third = record(identity(3), "Third.");
        cache.put(&third).expect("put");
        drop(cache);
        let reopened = Cache::open(&path).expect("reopen");
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get(&first.key), Some(first));
        assert_eq!(reopened.get(&third.key), Some(third));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write that fails is cut back to where the file ended before it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_write_leaves_the_file_as_it_was() {
        // /dev/full takes the open and fails every write with ENOSPC, like a full disk.
        let Ok(file) = std::fs::OpenOptions::new().append(true).open("/dev/full") else {
            return;
        };
        let cache = Cache {
            path: PathBuf::from("/dev/full"),
            state: Mutex::new(State {
                entries: HashMap::new(),
                file,
                torn: false,
            }),
        };
        assert!(cache.put(&record(identity(1), "Lost.")).is_err());
        assert!(cache.is_empty(), "not counted as cached");
    }

    #[test]
    fn a_later_entry_replaces_an_earlier_one_and_reopening_compacts_them() {
        let dir = temp_dir("replace");
        let path = dir.join(CACHE_FILE_NAME);
        let cache = Cache::open(&path).expect("open");
        let old = record(identity(1), "Old.");
        let new = record(identity(1), "New.");
        cache.put(&old).expect("put");
        cache.put(&new).expect("put");
        assert_eq!(
            cache.get(&new.key).map(|r| r.clip.description.summary),
            Some("New.".to_string())
        );
        drop(cache);
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("read")
                .lines()
                .count(),
            2
        );

        let reopened = Cache::open(&path).expect("reopen");
        assert_eq!(reopened.get(&new.key), Some(new));
        drop(reopened);
        assert_eq!(
            std::fs::read_to_string(&path)
                .expect("read")
                .lines()
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_of_a_newer_format_are_kept_but_not_read() {
        let dir = temp_dir("newer");
        let path = dir.join(CACHE_FILE_NAME);
        let newer = r#"{"v":2,"something":"new"}"#;
        std::fs::write(&path, format!("{newer}\ngarbage\n")).expect("write");
        let cache = Cache::open(&path).expect("open");
        assert!(cache.is_empty());
        drop(cache);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{newer}\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_sits_next_to_the_videos_unless_a_directory_is_given() {
        assert_eq!(
            cache_path(Path::new("footage"), None),
            Path::new("footage").join(CACHE_FILE_NAME)
        );
        assert_eq!(
            cache_path(Path::new("footage"), Some(Path::new("/tmp/caches"))),
            Path::new("/tmp/caches").join(CACHE_FILE_NAME)
        );
        let dir = temp_dir("create");
        let nested = dir.join("a").join("b").join(CACHE_FILE_NAME);
        Cache::open(&nested).expect("creates its directory");
        assert!(nested.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
