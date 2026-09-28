//! Frames and durations of a clip for AI descriptions, read with GStreamer.
//!
//! The clip is opened paused (`uridecodebin ! videoscale ! videoconvert ! appsink`, other streams
//! sent nowhere) and, for each sample time, the pipeline seeks there and takes the one frame it
//! prerolls. Scaling comes first, so a 4K frame is never converted at full size. A seek decodes
//! about one GOP, not the whole clip.
//!
//! The orientation tag is applied here, on the small frame, rather than with `videoflip`: that
//! element is not in the GStreamer bundled for Windows, so a phone clip stored sideways comes
//! out upright on every system.

use std::io::Cursor;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::describe::{
    candidate_times, frame_count, frame_size, sample_times, select_key_frames, Frame,
    FrameSampling, FRAME_LONG_SIDE,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

/// How long opening a clip may take before it counts as unreadable.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one seek may take to deliver its frame.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const JPEG_QUALITY: u8 = 80;

/// The frames [`Clip::sample_with_fingerprints`] read: what is sent to the model, and each one's
/// fingerprint (same order, same times), made from the same decoded picture.
pub(crate) struct Sampled {
    pub(crate) frames: Vec<Frame>,
    pub(crate) fingerprints: Vec<crate::FrameFingerprint>,
}

/// A clip opened paused for sampling. Stopped when dropped.
pub struct Clip {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
}

impl Drop for Clip {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Clip {
    /// Open the clip at `path` and wait up to `timeout` for its first frame.
    pub fn open(path: &Path, timeout: Duration) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let absolute = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let uri = url::Url::from_file_path(&absolute)
            .map_err(|()| format!("not a file path: {}", absolute.display()))?;

        let make = |factory: &str| {
            gst::ElementFactory::make(factory)
                .build()
                .map_err(|e| format!("{factory}: {e}"))
        };
        let pipeline = gst::Pipeline::new();
        let decode = gst::ElementFactory::make("uridecodebin")
            .property("uri", uri.as_str())
            .build()
            .map_err(|e| format!("uridecodebin: {e}"))?;
        let scale = make("videoscale")?;
        // At most 512 px on each side with square pixels; videoscale keeps the aspect ratio
        // within that, so the long side becomes 512.
        let scaled = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", gst::IntRange::new(1, FRAME_LONG_SIDE as i32))
                    .field("height", gst::IntRange::new(1, FRAME_LONG_SIDE as i32))
                    .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                    .build(),
            )
            .build()
            .map_err(|e| format!("capsfilter: {e}"))?;
        let convert = make("videoconvert")?;
        let sink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGB")
                    .build(),
            )
            .sync(false)
            .max_buffers(1)
            .build();
        pipeline
            .add_many([&decode, &scale, &scaled, &convert, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;
        gst::Element::link_many([&scale, &scaled, &convert, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;

        // The first video stream goes to the scaler; everything else (sound, a second video
        // stream) to a sink of its own that drops it, so no stream stalls the others.
        let video_in = scale
            .static_pad("sink")
            .ok_or("videoscale has no sink pad")?;
        // Weak: the pipeline owns the decoder that owns this closure.
        let bin = pipeline.downgrade();
        decode.connect_pad_added(move |_, pad| {
            let is_video = pad
                .current_caps()
                .or_else(|| Some(pad.query_caps(None)))
                .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("video/")))
                .unwrap_or(false);
            if is_video && !video_in.is_linked() && pad.link(&video_in).is_ok() {
                return;
            }
            let Ok(drop) = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
            else {
                return;
            };
            let Some(bin) = bin.upgrade() else {
                return;
            };
            if bin.add(&drop).is_ok() {
                let _ = drop.sync_state_with_parent();
                if let Some(sink_pad) = drop.static_pad("sink") {
                    let _ = pad.link(&sink_pad);
                }
            }
        });

        let clip = Self { pipeline, sink };
        clip.pipeline.set_state(gst::State::Paused).map_err(|_| {
            clip.bus_error()
                .unwrap_or_else(|| "cannot open".to_string())
        })?;
        let (result, state, _) = clip
            .pipeline
            .state(gst::ClockTime::from_mseconds(timeout.as_millis() as u64));
        if result.is_err() || state != gst::State::Paused {
            return Err(clip
                .bus_error()
                .unwrap_or_else(|| "no frame in time".to_string()));
        }
        // Prerolled without a video frame: no video stream, or no decoder for it.
        if clip
            .sink
            .static_pad("sink")
            .and_then(|pad| pad.current_caps())
            .is_none()
        {
            return Err("no video stream".to_string());
        }
        Ok(clip)
    }

    fn bus_error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        std::iter::from_fn(|| bus.pop()).find_map(|message| match message.view() {
            gst::MessageView::Error(e) => Some(e.error().to_string()),
            _ => None,
        })
    }

    /// The clip's length in seconds.
    pub fn duration_s(&self) -> Option<f64> {
        self.pipeline
            .query_duration::<gst::ClockTime>()
            .map(|d| d.nseconds() as f64 / 1e9)
            .filter(|d| *d > 0.0)
    }

    /// The clip's `image-orientation` tag (`rotate-90`, `flip-rotate-0`, …), from the tag
    /// events that reached the sink.
    fn orientation(&self) -> Option<String> {
        let pad = self.sink.static_pad("sink")?;
        (0..8)
            .map_while(|i| pad.sticky_event::<gst::event::Tag>(i))
            .find_map(|event| {
                event
                    .tag()
                    .get::<gst::tags::ImageOrientation>()
                    .map(|value| value.get().to_string())
            })
    }

    /// Seek to `time_s` and take the frame there: its real time and its pixels.
    fn frame_at(
        &self,
        time_s: f64,
        flags: gst::SeekFlags,
    ) -> Result<(f64, image::RgbImage), String> {
        let position = gst::ClockTime::from_nseconds((time_s * 1e9) as u64);
        self.pipeline
            .seek_simple(gst::SeekFlags::FLUSH | flags, position)
            .map_err(|_| "seek failed".to_string())?;
        let sample = self
            .sink
            .try_pull_preroll(gst::ClockTime::from_mseconds(
                FRAME_TIMEOUT.as_millis() as u64
            ))
            .ok_or_else(|| {
                self.bus_error()
                    .unwrap_or_else(|| "no frame after a seek".to_string())
            })?;
        let pts = sample
            .buffer()
            .and_then(|b| b.pts())
            .map_or(time_s, |t| t.nseconds() as f64 / 1e9);
        Ok((pts, to_image(&sample)?))
    }

    /// The frames of the whole clip, as JPEG: [`sample_times`] with `Interval`, or the frames
    /// [`select_key_frames`] keeps out of [`candidate_times`] with `KeyFrames` (see
    /// `describe::FrameSampling`) — skipped in favour of `sample_times` when the frame budget
    /// ([`frame_count`]) is 0 or 1, since there is then no window to choose a frame within.
    /// Stops with `Ok(None)` when `cancel` is set between two frames. `on_frame(done, total)`
    /// follows along, `total` always the number of frames that will actually be sent (as before
    /// key frames existed) even though `KeyFrames` decodes more candidates than that to choose
    /// from.
    pub fn sample(
        &self,
        duration_s: f64,
        sampling: FrameSampling,
        cancel: &AtomicBool,
        on_frame: impl FnMut(usize, usize),
    ) -> Result<Option<Vec<Frame>>, String> {
        Ok(self
            .sample_with_fingerprints(duration_s, sampling, cancel, on_frame)?
            .map(|sampled| sampled.frames))
    }

    /// [`Self::sample`], with each frame's fingerprint (an 8×8 grid of average luma of the upright
    /// frame, see [`crate::FrameFingerprint`]) for grouping similar footage.
    pub(crate) fn sample_with_fingerprints(
        &self,
        duration_s: f64,
        sampling: FrameSampling,
        cancel: &AtomicBool,
        mut on_frame: impl FnMut(usize, usize),
    ) -> Result<Option<Sampled>, String> {
        // Below two frames' worth of budget there is no window to choose a frame within, so
        // `KeyFrames` has nothing to add over `Interval`.
        let key_frames = sampling == FrameSampling::KeyFrames && frame_count(duration_s) > 1;
        let times = if key_frames {
            candidate_times(duration_s)
        } else {
            sample_times(duration_s)
        };
        let orientation = self.orientation();
        let interval = match times.as_slice() {
            [a, b, ..] => b - a,
            _ => duration_s,
        };
        // The frame budget `on_frame`'s `total` reports: same meaning as before key frames
        // existed, even though `times` (the candidates to decode) can be denser than that. `done`
        // is `i` scaled down proportionally from the candidate loop's range into the budget's,
        // so it still counts up smoothly to `total` over the (denser) loop instead of jumping.
        let frame_budget = frame_count(duration_s).max(1);
        let candidate_total = times.len().max(1);
        let mut candidates: Vec<(f64, image::RgbImage)> = Vec::new();
        for (i, time_s) in times.into_iter().enumerate() {
            let done = i * frame_budget / candidate_total;
            debug_assert!(done < frame_budget, "{done} of {frame_budget}");
            on_frame(done, frame_budget);
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let fast = gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_NEAREST;
            let (mut pts, mut image) = match self.frame_at(time_s, fast) {
                Ok(frame) => frame,
                // Past the end of the picture (the sound runs longer): the frames so far are
                // the whole picture.
                Err(_) if !candidates.is_empty() => break,
                Err(e) => return Err(e),
            };
            let last = candidates.last().map(|(t, _)| *t);
            if needs_exact(pts, time_s, last, interval) {
                (pts, image) = match self.frame_at(time_s, gst::SeekFlags::ACCURATE) {
                    Ok(frame) => frame,
                    Err(_) if !candidates.is_empty() => break,
                    Err(e) => return Err(e),
                };
                if last.is_some_and(|last| pts <= last + 1e-3) {
                    continue;
                }
            }
            candidates.push((pts, image));
        }
        let chosen: Vec<usize> = if key_frames {
            let fingerprints: Vec<(f64, Vec<u8>)> = candidates
                .iter()
                .map(|(t, image)| (*t, fingerprint(image)))
                .collect();
            select_key_frames(&fingerprints, frame_budget)
        } else {
            (0..candidates.len()).collect()
        };
        let mut frames = Vec::with_capacity(chosen.len());
        let mut fingerprints = Vec::with_capacity(chosen.len());
        for i in chosen {
            let (time_s, image) = candidates[i].clone();
            let upright = orient(image, orientation.as_deref());
            fingerprints.push(crate::FrameFingerprint {
                time_s,
                fingerprint: fingerprint(&upright),
            });
            frames.push(Frame {
                time_s,
                jpeg: to_jpeg(upright)?,
            });
        }
        Ok(Some(Sampled {
            frames,
            fingerprints,
        }))
    }

    /// The frame at `at_s`, plus one on each side `window_s` away — for [`describe_moment`],
    /// never more than three frames, so always exact seeks: nothing like `sample`'s care to keep
    /// a whole clip's worth of seeks cheap is needed here. `at_s` and the window are clamped to
    /// the clip's own duration, so a caller need not know it. `Ok(None)` when `cancel` is set
    /// between two frames.
    ///
    /// [`describe_moment`]: crate::describe_moment
    pub fn sample_moment(
        &self,
        at_s: f64,
        window_s: f64,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<Frame>>, String> {
        let duration_s = self.duration_s().unwrap_or(at_s).max(0.0);
        let at_s = at_s.clamp(0.0, duration_s);
        let window_s = window_s.max(0.0);
        let mut times = vec![
            (at_s - window_s).max(0.0),
            at_s,
            (at_s + window_s).min(duration_s),
        ];
        times.sort_by(f64::total_cmp);
        times.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
        let orientation = self.orientation();
        let mut frames = Vec::with_capacity(times.len());
        for time_s in times {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let (pts, image) = match self.frame_at(time_s, gst::SeekFlags::ACCURATE) {
                Ok(frame) => frame,
                // A seek right at the clip's own duration can land past the last decodable
                // frame; skip it once at least one frame (closer to `at_s`) is already in hand,
                // the same tolerance `sample` gives a whole clip's last window.
                Err(_) if !frames.is_empty() => continue,
                Err(e) => return Err(e),
            };
            frames.push(Frame {
                time_s: pts,
                jpeg: to_jpeg(orient(image, orientation.as_deref()))?,
            });
        }
        Ok(Some(frames))
    }
}

/// A small grid of average luma values (Rec. 601 weights), cheap to compare between frames: how
/// much the picture changed between two candidates is [`describe::select_key_frames`]'s job, on
/// the byte differences of what this returns.
const FINGERPRINT_GRID: (u32, u32) = (8, 8);

fn fingerprint(image: &image::RgbImage) -> Vec<u8> {
    let (gw, gh) = FINGERPRINT_GRID;
    let (w, h) = image.dimensions();
    (0..gh)
        .flat_map(|gy| (0..gw).map(move |gx| (gx, gy)))
        .map(|(gx, gy)| {
            let x0 = gx * w / gw;
            let x1 = ((gx + 1) * w / gw).max(x0 + 1).min(w);
            let y0 = gy * h / gh;
            let y1 = ((gy + 1) * h / gh).max(y0 + 1).min(h);
            let mut sum: u64 = 0;
            let mut count: u64 = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = image.get_pixel(x, y);
                    sum += u64::from(
                        u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114,
                    );
                    count += 1;
                }
            }
            (sum / count.max(1) / 1000) as u8
        })
        .collect()
}

/// Whether a keyframe seek to `target` that landed on `snapped` must be redone exactly:
/// keyframes further apart than the sampling interval snap to a frame already taken, or to one
/// far from the time asked for. Seeking to the exact time instead keeps the frames in order,
/// labelled with times near their samples, and bills no frame twice.
fn needs_exact(snapped: f64, target: f64, last: Option<f64>, interval: f64) -> bool {
    last.is_some_and(|last| snapped <= last + 1e-3) || (snapped - target).abs() > interval / 2.0
}

/// `image` turned upright as the clip's orientation tag says: `rotate-N` turns it N° clockwise,
/// `flip-rotate-N` mirrors it left to right first.
fn orient(image: image::RgbImage, orientation: Option<&str>) -> image::RgbImage {
    use image::imageops::{flip_horizontal, rotate180, rotate270, rotate90};
    let Some(tag) = orientation else {
        return image;
    };
    let (flip, rotation) = match tag.strip_prefix("flip-") {
        Some(rest) => (true, rest),
        None => (false, tag),
    };
    let image = if flip { flip_horizontal(&image) } else { image };
    match rotation {
        "rotate-90" => rotate90(&image),
        "rotate-180" => rotate180(&image),
        "rotate-270" => rotate270(&image),
        _ => image,
    }
}

/// The RGB pixels of a sample, rows packed.
fn to_image(sample: &gst::Sample) -> Result<image::RgbImage, String> {
    let caps = sample.caps().ok_or("frame without caps")?;
    let info = gst_video::VideoInfo::from_caps(caps).map_err(|e| e.to_string())?;
    let buffer = sample.buffer().ok_or("frame without data")?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|_| "unreadable frame".to_string())?;
    let (width, height) = (info.width(), info.height());
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).map_err(|e| e.to_string())?;
    let row = width as usize * 3;
    let mut pixels = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = y * stride;
        pixels.extend_from_slice(data.get(start..start + row).ok_or("short frame")?);
    }
    image::RgbImage::from_raw(width, height, pixels).ok_or_else(|| "bad frame size".to_string())
}

/// Encode a frame as JPEG, first scaling it down if the pipeline could not (its long side is
/// still over 512 px).
fn to_jpeg(image: image::RgbImage) -> Result<Vec<u8>, String> {
    let (w, h) = frame_size(image.width(), image.height());
    let image = if (w, h) == image.dimensions() {
        image
    } else {
        image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle)
    };
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), JPEG_QUALITY)
        .encode_image(&image)
        .map_err(|e| e.to_string())?;
    Ok(jpeg)
}

/// The length in seconds of the clip at `path`, for an estimate before describing it; `None`
/// when it could not be read within [`OPEN_TIMEOUT`].
pub fn clip_duration_s(path: &Path) -> Option<f64> {
    Clip::open(path, OPEN_TIMEOUT)
        .map_err(|e| log::info!("clipscribe: cannot read {}: {e}", path.display()))
        .ok()
        .and_then(|clip| clip.duration_s())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;

    #[cfg(target_os = "linux")]
    fn repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The clips of `tests/clips.txt`, which CI decodes on Linux.
    #[cfg(target_os = "linux")]
    fn ci_clips() -> Vec<PathBuf> {
        std::fs::read_to_string(repo().join("tests/clips.txt"))
            .expect("clip list")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| repo().join(l))
            .collect()
    }

    fn jpeg_size(jpeg: &[u8]) -> (u32, u32) {
        image::load_from_memory(jpeg)
            .expect("jpeg")
            .to_rgb8()
            .dimensions()
    }

    /// Linux only, like the self-test: the Windows CI job has a build-only GStreamer.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_ci_clip_gives_its_frames_fast_and_within_512_px() {
        for path in ci_clips() {
            let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
            let duration = clip.duration_s().expect("duration");
            let started = std::time::Instant::now();
            let frames = clip
                .sample(
                    duration,
                    FrameSampling::KeyFrames,
                    &AtomicBool::new(false),
                    |_, _| {},
                )
                .expect("frames")
                .expect("not cancelled");
            let expected = sample_times(duration).len();
            assert!(
                !frames.is_empty() && frames.len() <= expected,
                "{}: {} of {expected}",
                path.display(),
                frames.len()
            );
            // Logged, with only a generous bound, so a loaded CI runner does not flake.
            let per_frame = started.elapsed() / frames.len() as u32;
            eprintln!(
                "{}: {} frames, {per_frame:?} each",
                path.display(),
                frames.len()
            );
            assert!(
                per_frame < Duration::from_secs(5),
                "{}: {per_frame:?}",
                path.display()
            );
            for frame in &frames {
                let (w, h) = jpeg_size(&frame.jpeg);
                assert_eq!(w.max(h), FRAME_LONG_SIDE.min(w.max(h)));
                assert!(frame.time_s >= 0.0 && frame.time_s <= duration + 0.1);
            }
            let times: Vec<f64> = frames.iter().map(|f| f.time_s).collect();
            assert!(
                times.windows(2).all(|w| w[0] < w[1]),
                "no duplicates: {times:?}"
            );
        }
    }

    /// A phone clip stored landscape with a 90° rotation tag comes out portrait.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_rotated_clip_comes_out_upright() {
        let path = repo().join("tests/clips/rotated-90.mp4");
        let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
        let frames = clip
            .sample(
                clip.duration_s().expect("duration"),
                FrameSampling::KeyFrames,
                &AtomicBool::new(false),
                |_, _| {},
            )
            .expect("frames")
            .expect("not cancelled");
        let (w, h) = jpeg_size(&frames[0].jpeg);
        assert!(h > w, "portrait: {w}×{h}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_cancel_stops_between_frames() {
        let clip = Clip::open(&ci_clips()[0], Duration::from_secs(20)).expect("opens");
        let duration = clip.duration_s().expect("duration");
        assert!(clip
            .sample(
                duration,
                FrameSampling::KeyFrames,
                &AtomicBool::new(true),
                |_, _| {}
            )
            .expect("ok")
            .is_none());
    }

    /// `Interval` reproduces today's fixed-interval timestamps exactly, frame for frame.
    #[cfg(target_os = "linux")]
    #[test]
    fn interval_sampling_matches_sample_times() {
        for path in ci_clips() {
            let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
            let duration = clip.duration_s().expect("duration");
            let frames = clip
                .sample(
                    duration,
                    FrameSampling::Interval,
                    &AtomicBool::new(false),
                    |_, _| {},
                )
                .expect("frames")
                .expect("not cancelled");
            let times: Vec<f64> = frames.iter().map(|f| f.time_s).collect();
            let expected = sample_times(duration);
            assert_eq!(times.len(), expected.len(), "{}", path.display());
            for (got, want) in times.iter().zip(&expected) {
                assert!(
                    (got - want).abs() <= 1.0,
                    "{}: {got} vs {want}",
                    path.display()
                );
            }
        }
    }

    /// `sample_moment` on a real clip: one frame per requested time, in order, clamped to the
    /// clip's own duration, and never more than the three times asked for.
    #[cfg(target_os = "linux")]
    #[test]
    fn sample_moment_reads_a_frame_around_a_real_time() {
        for path in ci_clips() {
            let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
            let duration = clip.duration_s().expect("duration");
            let at = duration / 2.0;
            let frames = clip
                .sample_moment(at, 1.0, &AtomicBool::new(false))
                .expect("frames")
                .expect("not cancelled");
            assert!(!frames.is_empty(), "{}", path.display());
            assert!(frames.len() <= 3, "{}: {}", path.display(), frames.len());
            assert!(
                frames.windows(2).all(|w| w[0].time_s <= w[1].time_s),
                "{}: not in order",
                path.display()
            );
            for frame in &frames {
                assert!(
                    frame.time_s >= 0.0 && frame.time_s <= duration + 1e-3,
                    "{}: {} outside 0..={duration}",
                    path.display(),
                    frame.time_s
                );
                assert!(!frame.jpeg.is_empty(), "{}", path.display());
            }
        }
    }

    /// A time past the end, or a window reaching below zero, is clamped rather than failing.
    #[cfg(target_os = "linux")]
    #[test]
    fn sample_moment_clamps_a_time_and_window_outside_the_clip() {
        let path = ci_clips().into_iter().next().expect("at least one clip");
        let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
        let duration = clip.duration_s().expect("duration");
        let past_the_end = clip
            .sample_moment(duration + 100.0, 1.0, &AtomicBool::new(false))
            .expect("frames")
            .expect("not cancelled");
        assert!(!past_the_end.is_empty());
        assert!(past_the_end.iter().all(|f| f.time_s <= duration + 1e-3));

        let near_zero = clip
            .sample_moment(0.0, 1.0, &AtomicBool::new(false))
            .expect("frames")
            .expect("not cancelled");
        assert!(!near_zero.is_empty());
        assert!(near_zero.iter().all(|f| f.time_s >= 0.0));
    }

    /// A negative `window_s` (never produced by the CLI or the library's own default, but not
    /// ruled out by the type) clamps to zero instead of reading nonsensical times.
    #[cfg(target_os = "linux")]
    #[test]
    fn sample_moment_clamps_a_negative_window_to_zero() {
        let path = ci_clips().into_iter().next().expect("at least one clip");
        let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
        let duration = clip.duration_s().expect("duration");
        let at = duration / 2.0;
        let frames = clip
            .sample_moment(at, -5.0, &AtomicBool::new(false))
            .expect("frames")
            .expect("not cancelled");
        assert!(!frames.is_empty());
        for frame in &frames {
            assert!((frame.time_s - at).abs() < 1e-2, "{}", frame.time_s);
        }
    }

    /// Key frames never exceed the same budget `Interval` uses, on real clips (not just the
    /// synthetic fingerprints `describe::select_key_frames` is unit-tested with).
    #[cfg(target_os = "linux")]
    #[test]
    fn key_frames_stay_within_the_interval_budget() {
        for path in ci_clips() {
            let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
            let duration = clip.duration_s().expect("duration");
            let key_frames = clip
                .sample(
                    duration,
                    FrameSampling::KeyFrames,
                    &AtomicBool::new(false),
                    |_, _| {},
                )
                .expect("frames")
                .expect("not cancelled");
            let interval_times = sample_times(duration);
            eprintln!(
                "{}: keyframes {:.1?}\n{}: interval  {:.1?}",
                path.display(),
                key_frames.iter().map(|f| f.time_s).collect::<Vec<_>>(),
                path.display(),
                interval_times
            );
            assert!(
                key_frames.len() <= interval_times.len(),
                "{}: {} key frames vs {} interval frames",
                path.display(),
                key_frames.len(),
                interval_times.len()
            );
        }
    }

    /// `on_frame`'s `total` is always the real frame budget (`frame_count`), the same meaning it
    /// had before key frames existed, even though `KeyFrames` decodes many more candidates than
    /// that: frename shows this number and sizes its progress ETA from it, so it must still match
    /// how many frames actually get sent, not how many candidates got decoded along the way.
    #[cfg(target_os = "linux")]
    #[test]
    fn progress_total_is_the_frame_budget_not_the_candidate_count() {
        let path = ci_clips().into_iter().next().expect("a test clip");
        let clip = Clip::open(&path, Duration::from_secs(20)).expect("opens");
        let duration = clip.duration_s().expect("duration");
        let budget = frame_count(duration);
        let mut totals_seen = Vec::new();
        let frames = clip
            .sample(
                duration,
                FrameSampling::KeyFrames,
                &AtomicBool::new(false),
                |_, total| {
                    totals_seen.push(total);
                },
            )
            .expect("frames")
            .expect("not cancelled");
        assert!(!totals_seen.is_empty());
        assert!(
            totals_seen.iter().all(|&t| t == budget),
            "{totals_seen:?} vs budget {budget}"
        );
        assert_eq!(frames.len(), budget, "the budget the caller was shown");
    }

    /// A clip whose sound runs longer than its picture gives the frames of the picture.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_picture_shorter_than_the_sound_gives_its_frames() {
        let dir = std::env::temp_dir().join(format!("clipscribe-frames-av-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let clip_path = dir.join("short-picture.mkv");
        let made = std::process::Command::new("gst-launch-1.0")
            .args([
                "-q",
                "videotestsrc",
                "num-buffers=40",
                "!",
                "video/x-raw,framerate=10/1,width=64,height=48",
                "!",
                "jpegenc",
                "!",
                "matroskamux",
                "name=mux",
                "!",
                "filesink",
            ])
            .arg(format!("location={}", clip_path.display()))
            .args([
                "audiotestsrc",
                "num-buffers=430",
                "!",
                "audio/x-raw,rate=44100",
                "!",
                "mux.",
            ])
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("gst-launch-1.0 could not make the test clip: skipped");
            return;
        }
        let clip = Clip::open(&clip_path, Duration::from_secs(20)).expect("opens");
        let duration = clip.duration_s().expect("duration");
        let frames = clip
            .sample(
                duration,
                FrameSampling::KeyFrames,
                &AtomicBool::new(false),
                |_, _| {},
            )
            .expect("the picture's frames")
            .expect("not cancelled");
        assert!(!frames.is_empty());
        assert!(
            frames.iter().all(|f| f.time_s <= 4.5),
            "{:?}",
            frames.iter().map(|f| f.time_s).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Below two frames' worth of budget (`frame_count(duration_s) <= 1`), `KeyFrames` takes
    /// `sample_times`'s single midpoint frame directly, the same as `Interval` — there is no
    /// window to choose a frame within — on a real decoded clip, not just the pure-math check in
    /// `describe::tests`. Like `a_picture_shorter_than_the_sound_gives_its_frames` above, this
    /// needs `gst-launch-1.0` to build its fixture and skips (still passing) without it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_very_short_clip_gets_sample_times_midpoint_frame_with_key_frames_too() {
        let dir =
            std::env::temp_dir().join(format!("clipscribe-frames-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let clip_path = dir.join("very-short.mkv");
        let made = std::process::Command::new("gst-launch-1.0")
            .args([
                "-q",
                "videotestsrc",
                "num-buffers=10",
                "!",
                "video/x-raw,framerate=10/1,width=64,height=48",
                "!",
                "jpegenc",
                "!",
                "matroskamux",
                "!",
                "filesink",
            ])
            .arg(format!("location={}", clip_path.display()))
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("gst-launch-1.0 could not make the test clip: skipped");
            return;
        }
        let clip = Clip::open(&clip_path, Duration::from_secs(20)).expect("opens");
        let duration = clip.duration_s().expect("duration");
        assert!(duration < 2.0, "1 s clip: {duration}");
        assert_eq!(frame_count(duration), 1);
        let key_frames = clip
            .sample(
                duration,
                FrameSampling::KeyFrames,
                &AtomicBool::new(false),
                |_, _| {},
            )
            .expect("frames")
            .expect("not cancelled");
        let interval_frames = clip
            .sample(
                duration,
                FrameSampling::Interval,
                &AtomicBool::new(false),
                |_, _| {},
            )
            .expect("frames")
            .expect("not cancelled");
        assert_eq!(key_frames.len(), 1);
        assert_eq!(key_frames[0].time_s, interval_frames[0].time_s);
        assert_eq!(key_frames[0].time_s, sample_times(duration)[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_video_is_unreadable() {
        let dir = std::env::temp_dir().join(format!("clipscribe-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let fake = dir.join("fake.mp4");
        std::fs::write(&fake, b"not a movie").expect("write");
        assert_eq!(clip_duration_s(&fake), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With keyframes every 10 s and a frame asked for every 2 s, every sample that snapped to a
    /// keyframe taken already or far away is sought exactly, so times only go forward.
    #[test]
    fn sparse_keyframes_are_sought_exactly() {
        let keyframes = [0.0, 10.0, 20.0];
        let snap = |t: f64| {
            *keyframes
                .iter()
                .min_by(|a, b| (*a - t).abs().total_cmp(&(*b - t).abs()))
                .expect("keyframes")
        };
        let mut taken: Vec<f64> = Vec::new();
        for i in 0..10 {
            let target = f64::from(i) * 2.0;
            let snapped = snap(target);
            let time = if needs_exact(snapped, target, taken.last().copied(), 2.0) {
                target
            } else {
                snapped
            };
            taken.push(time);
        }
        assert!(taken.windows(2).all(|w| w[0] < w[1]), "{taken:?}");
        assert!(taken
            .iter()
            .enumerate()
            .all(|(i, t)| (t - i as f64 * 2.0).abs() <= 1.0));
    }

    #[test]
    fn the_orientation_tag_turns_frames_upright() {
        // A 2×1 frame: red on the left, blue on the right.
        let mut frame = image::RgbImage::new(2, 1);
        frame.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        frame.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let red = image::Rgb([255, 0, 0]);

        assert_eq!(orient(frame.clone(), None), frame);
        assert_eq!(orient(frame.clone(), Some("rotate-0")), frame);
        let turned = orient(frame.clone(), Some("rotate-90"));
        assert_eq!(turned.dimensions(), (1, 2));
        assert_eq!(
            *turned.get_pixel(0, 0),
            red,
            "clockwise: the left edge goes up"
        );
        let turned = orient(frame.clone(), Some("rotate-270"));
        assert_eq!(*turned.get_pixel(0, 1), red);
        assert_eq!(
            *orient(frame.clone(), Some("rotate-180")).get_pixel(1, 0),
            red
        );
        assert_eq!(
            *orient(frame.clone(), Some("flip-rotate-0")).get_pixel(1, 0),
            red
        );
        let flipped = orient(frame, Some("flip-rotate-90"));
        assert_eq!(
            (flipped.dimensions(), *flipped.get_pixel(0, 1)),
            ((1, 2), red)
        );
    }

    #[test]
    fn large_frames_are_scaled_down_on_encode() {
        let jpeg = to_jpeg(image::RgbImage::new(1080, 1920)).expect("jpeg");
        assert_eq!(jpeg_size(&jpeg), (288, 512));
    }

    #[test]
    fn a_solid_frame_fingerprints_to_one_flat_value() {
        let mut white = image::RgbImage::new(16, 16);
        white
            .pixels_mut()
            .for_each(|p| *p = image::Rgb([255, 255, 255]));
        let fp = fingerprint(&white);
        assert_eq!(fp.len(), 64);
        assert!(fp.iter().all(|&v| v == 255), "{fp:?}");

        let black = image::RgbImage::new(16, 16);
        assert!(fingerprint(&black).iter().all(|&v| v == 0));
    }

    #[test]
    fn a_split_frame_fingerprints_differently_on_each_side() {
        let mut split = image::RgbImage::new(16, 16);
        for (x, _y, p) in split.enumerate_pixels_mut() {
            *p = if x < 8 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            };
        }
        let fp = fingerprint(&split);
        // The 8x8 grid's left half comes from the black side, the right half from the white one.
        for gy in 0..8 {
            for gx in 0..4 {
                assert_eq!(fp[gy * 8 + gx], 0);
            }
            for gx in 4..8 {
                assert_eq!(fp[gy * 8 + gx], 255);
            }
        }
    }
}
