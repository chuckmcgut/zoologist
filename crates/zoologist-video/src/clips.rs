//! Event clips, snapshots and thumbnails (plan Step 6.4).
//!
//! A clip is cut from the recording segments around an event and re-muxed into a new MP4:
//! no decoding and no re-encoding, so it takes milliseconds.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use zoologist_core::yuv::{PixelRect, RgbCropper, i420_to_rgb_full};
use zoologist_core::{BBox, Frame};

use crate::mp4w::{SampleIndexEntry, write_mp4_streamed};
use crate::recorder::read_segment_index;
use crate::snapshot::{encode_jpeg, write_atomic};

/// Errors building a clip.
#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    #[error("no recording covers {from} – {to}")]
    NoFootage {
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// A recording segment on disk (paths relative to the data directory).
#[derive(Clone, Debug)]
pub struct SegmentFile {
    pub path: String,
    pub index_path: String,
}

/// Result of [`build_clip`].
#[derive(Clone, Debug, PartialEq)]
pub struct ClipInfo {
    pub bytes: u64,
    pub started_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub frames: usize,
}

/// Cuts `[from, to]` out of `segments` (oldest first) into a new MP4 at `out`.
///
/// The clip starts at the last keyframe at or before `from` (so it plays from its first frame)
/// and ends with the last frame at or before `to`. If the stream parameters change between
/// segments, only the part containing `from` is used.
pub fn build_clip(
    data_dir: &Path,
    segments: &[SegmentFile],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    out: &Path,
) -> Result<ClipInfo, ClipError> {
    let no_footage = || ClipError::NoFootage { from, to };
    let (from_us, to_us) = (from.timestamp_micros(), to.timestamp_micros());

    // Gather (segment, entry) pairs, grouped by stream parameters.
    let mut info = None;
    let mut picked: Vec<(usize, SampleIndexEntry)> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let index = read_segment_index(&data_dir.join(&segment.index_path))?;
        let Some(segment_info) = index.info() else {
            continue;
        };
        match &info {
            None => info = Some(segment_info),
            Some(current) if *current != segment_info => {
                if picked.iter().any(|(_, e)| e.wall_us >= from_us) {
                    break; // the event is in the part already collected
                }
                picked.clear();
                info = Some(segment_info);
            }
            Some(_) => {}
        }
        picked.extend(index.samples.into_iter().map(|e| (i, e)));
    }
    let info = info.ok_or_else(no_footage)?;

    // Start at the last keyframe at or before `from`, or the first keyframe after it.
    let start = picked
        .iter()
        .rposition(|(_, e)| e.is_key && e.wall_us <= from_us)
        .or_else(|| picked.iter().position(|(_, e)| e.is_key))
        .ok_or_else(no_footage)?;
    let end = picked
        .iter()
        .rposition(|(_, e)| e.wall_us <= to_us)
        .filter(|&end| end >= start)
        .ok_or_else(no_footage)?;
    // The footage must actually reach into the window.
    let last = &picked[end].1;
    if picked[start].1.wall_us > to_us
        || last.wall_us + i64::from(last.duration_90k) * 100 / 9 < from_us
    {
        return Err(no_footage());
    }

    // The clip's layout comes from the segment indexes; the bytes are copied one frame at a
    // time, so memory use does not depend on the clip's length.
    let chosen = &picked[start..=end];
    let layout: Vec<SampleIndexEntry> = chosen.iter().map(|(_, e)| e.clone()).collect();
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = out.with_extension("mp4.tmp");
    write_mp4_streamed(
        std::io::BufWriter::new(std::fs::File::create(&tmp)?),
        &info,
        &layout,
        |w| {
            let mut buf = Vec::new();
            let mut file: Option<(usize, std::fs::File)> = None;
            for (segment, e) in chosen {
                if file.as_ref().is_none_or(|(open, _)| open != segment) {
                    let path = data_dir.join(&segments[*segment].path);
                    file = Some((*segment, std::fs::File::open(path)?));
                }
                let (_, f) = file.as_mut().expect("opened above");
                f.seek(SeekFrom::Start(e.offset))?;
                buf.resize(e.size as usize, 0);
                f.read_exact(&mut buf)?;
                w.write_all(&buf)?;
            }
            Ok(())
        },
    )?;
    std::fs::rename(&tmp, out)?;
    let first = layout.first().map_or(0, |s| s.wall_us);
    let last = layout
        .last()
        .map_or(0, |s| s.wall_us + i64::from(s.duration_90k) * 100 / 9);
    Ok(ClipInfo {
        bytes: std::fs::metadata(out)?.len(),
        started_at: DateTime::from_timestamp_micros(first).unwrap_or_default(),
        duration_ms: (last - first) / 1000,
        frames: layout.len(),
    })
}

/// Writes the whole frame as a JPEG with `bbox` outlined, for the event viewer.
pub fn write_snapshot(frame: &Frame, bbox: Option<&BBox>, path: &Path) -> std::io::Result<()> {
    let mut rgb = i420_to_rgb_full(frame);
    if let Some(b) = bbox {
        draw_box(&mut rgb, frame.width, frame.height, b, [255, 196, 0]);
    }
    let jpeg = encode_jpeg(&rgb, frame.width, frame.height, 85).map_err(std::io::Error::other)?;
    write_atomic(path, &jpeg)
}

/// Writes a square close-up of `bbox` (plus 20 % margin), 320 px wide, for event tiles.
pub fn write_thumb(frame: &Frame, bbox: &BBox, path: &Path) -> std::io::Result<()> {
    const SIZE: u32 = 320;
    let (x, y, w, h) = bbox.expand(0.2).to_pixels(frame.width, frame.height);
    let side = w.max(h).min(frame.width.min(frame.height)).max(2);
    let cx = x + w / 2;
    let cy = y + h / 2;
    let region = PixelRect {
        x: cx.saturating_sub(side / 2).min(frame.width - side),
        y: cy.saturating_sub(side / 2).min(frame.height - side),
        w: side,
        h: side,
    };
    let mut rgb = Vec::new();
    RgbCropper::new()
        .crop(frame, region, SIZE, &mut rgb)
        .map_err(std::io::Error::other)?;
    let jpeg = encode_jpeg(&rgb, SIZE, SIZE, 80).map_err(std::io::Error::other)?;
    write_atomic(path, &jpeg)
}

/// Draws a 2-pixel rectangle outline into a packed RGB image.
pub fn draw_box(rgb: &mut [u8], w: u32, h: u32, b: &BBox, colour: [u8; 3]) {
    let (x, y, bw, bh) = b.to_pixels(w, h);
    let (x2, y2) = ((x + bw).min(w) - 1, (y + bh).min(h) - 1);
    let mut put = |px: u32, py: u32| {
        let i = ((py * w + px) * 3) as usize;
        rgb[i..i + 3].copy_from_slice(&colour);
    };
    for t in 0..2 {
        for px in x..=x2 {
            put(px, (y + t).min(y2));
            put(px, y2.saturating_sub(t).max(y));
        }
        for py in y..=y2 {
            put((x + t).min(x2), py);
            put(x2.saturating_sub(t).max(x), py);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::Arc;

    use chrono::Duration;
    use image::GenericImageView;

    use super::*;
    use crate::recorder::tests::{live_items, start};
    use crate::recorder::{SegmentBuilder, write_segment};

    /// Records 30 s of the fixture into 4 s segments in `dir`.
    fn record(dir: &Path) -> Vec<SegmentFile> {
        let mut builder = SegmentBuilder::new(4);
        let mut pending = Vec::new();
        for item in live_items(start(), 3) {
            pending.extend(builder.push(item));
        }
        pending.extend(builder.flush());
        pending
            .iter()
            .map(|p| {
                let w = write_segment(dir, "cam", p).unwrap();
                SegmentFile {
                    path: w.path,
                    index_path: w.index_path,
                }
            })
            .collect()
    }

    #[test]
    fn clip_spans_segments_and_starts_on_a_keyframe() {
        let dir = tempfile::tempdir().unwrap();
        let segments = record(dir.path());
        let out = dir.path().join("clips/1.mp4");
        // 5.5 s – 14.2 s crosses three 4 s segments; keyframes are every 2 s.
        let from = start() + Duration::milliseconds(5500);
        let to = start() + Duration::milliseconds(14200);
        let clip = build_clip(dir.path(), &segments, from, to, &out).unwrap();
        assert_eq!(clip.started_at, start() + Duration::seconds(4));
        assert_eq!(clip.frames, 103); // 4.0 s .. 14.2 s at 10 fps
        assert!(
            (10_200..=10_400).contains(&clip.duration_ms),
            "{}",
            clip.duration_ms
        );
        // Copying frame by frame gives exactly the file that writing them from memory gives.
        let mut samples = Vec::new();
        for segment in &segments {
            let index = read_segment_index(&dir.path().join(&segment.index_path)).unwrap();
            samples.extend(
                crate::mp4r::read_samples(&dir.path().join(&segment.path), &index.samples).unwrap(),
            );
        }
        let first = samples
            .iter()
            .position(|s| s.wall_us == clip.started_at.timestamp_micros())
            .unwrap();
        let info = read_segment_index(&dir.path().join(&segments[0].index_path))
            .unwrap()
            .info()
            .unwrap();
        let mut in_memory = Vec::new();
        crate::mp4w::write_mp4(&mut in_memory, &info, &samples[first..first + 103]).unwrap();
        assert!(std::fs::read(&out).unwrap() == in_memory, "same bytes");
        // For checking playback in a browser by hand.
        if let Ok(keep) = std::env::var("ZOOLOGIST_KEEP_CLIP") {
            std::fs::copy(&out, keep).unwrap();
        }

        if Command::new("ffprobe").arg("-version").output().is_ok() {
            let probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-count_frames",
                    "-show_entries",
                    "stream=nb_read_frames",
                ])
                .args(["-of", "csv=p=0"])
                .arg(&out)
                .output()
                .unwrap();
            assert!(
                probe.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&probe.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "103");
        }
    }

    #[test]
    fn window_without_footage_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let segments = record(dir.path());
        let out = dir.path().join("x.mp4");
        let later = start() + Duration::hours(1);
        let err = build_clip(
            dir.path(),
            &segments,
            later,
            later + Duration::seconds(5),
            &out,
        );
        assert!(matches!(err, Err(ClipError::NoFootage { .. })));
        assert!(matches!(
            build_clip(dir.path(), &[], start(), start(), &out),
            Err(ClipError::NoFootage { .. })
        ));
    }

    #[test]
    fn snapshot_and_thumbnail_have_the_right_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let rgb: Vec<u8> = (0..640 * 360).flat_map(|_| [40u8, 90, 140]).collect();
        let frame = Frame {
            camera_id: "cam".into(),
            seq: 1,
            captured_at: start(),
            width: 640,
            height: 360,
            i420: Arc::new(zoologist_core::yuv::rgb_to_i420(&rgb, 640, 360).unwrap()),
        };
        let bbox = BBox::new(0.8, 0.7, 0.99, 0.99);
        let snap = dir.path().join("snap.jpg");
        let thumb = dir.path().join("thumb.jpg");
        write_snapshot(&frame, Some(&bbox), &snap).unwrap();
        write_thumb(&frame, &bbox, &thumb).unwrap();
        let snap = image::open(&snap).unwrap().to_rgb8();
        assert_eq!(snap.dimensions(), (640, 360));
        // The box outline is drawn in amber.
        let (x, y, _, _) = bbox.to_pixels(640, 360);
        let px = snap.get_pixel(x + 20, y).0;
        assert!(px[0] > 200 && px[2] < 80, "{px:?}");
        assert_eq!(image::open(&thumb).unwrap().dimensions(), (320, 320));
    }
}
