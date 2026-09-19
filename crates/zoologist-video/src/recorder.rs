//! Continuous recording of a camera's main stream into short MP4 segments (plan Step 6.3).
//!
//! Frames are buffered in memory and written as one faststart MP4 per segment (about
//! `segment_seconds` long, always starting at a keyframe), plus a `.idx` file listing every
//! sample's byte range and wall-clock time, so clips can be cut later without decoding.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::mp4w::{Sample, SampleIndexEntry, write_mp4};
use crate::stream::{AccessUnit, Codec, StreamInfo, StreamItem};

/// A segment is written early (at the next keyframe) once its buffer exceeds this.
pub const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;
/// Frame duration used when timestamps are missing or implausible (10 fps).
const FALLBACK_DURATION_90K: u32 = 9000;
/// Re-anchor stream time to the wall clock when they drift apart by more than this.
const MAX_DRIFT_US: i64 = 500_000;

/// A finished segment waiting to be written.
#[derive(Clone, Debug)]
pub struct PendingSegment {
    pub info: StreamInfo,
    pub samples: Vec<Sample>,
}

impl PendingSegment {
    /// Wall-clock time of the first sample.
    pub fn started_at(&self) -> DateTime<Utc> {
        wall(self.samples.first().map_or(0, |s| s.wall_us))
    }

    /// Wall-clock time just after the last sample.
    pub fn ended_at(&self) -> DateTime<Utc> {
        let last = self.samples.last();
        wall(last.map_or(0, |s| s.wall_us + i64::from(s.duration_90k) * 100 / 9))
    }
}

fn wall(us: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(us).unwrap_or_default()
}

/// Groups a stream's frames into segments. Pure logic; the caller writes the segments.
pub struct SegmentBuilder {
    segment_us: i64,
    info: Option<StreamInfo>,
    /// The newest frame; its duration is known once the next one arrives.
    last: Option<AccessUnit>,
    samples: Vec<Sample>,
    bytes: usize,
    /// Wall clock and stream timestamp of the session's first frame. Frames get
    /// `anchor_wall + (ts - anchor_ts)`: smoother than arrival times and continuous across
    /// segments. Reset when the stream restarts or drifts more than [`MAX_DRIFT_US`].
    anchor: Option<(i64, i64)>,
}

impl SegmentBuilder {
    pub fn new(segment_seconds: u32) -> Self {
        Self {
            segment_us: i64::from(segment_seconds) * 1_000_000,
            info: None,
            last: None,
            samples: Vec::new(),
            bytes: 0,
            anchor: None,
        }
    }

    /// Adds one stream item. Returns a segment when one is complete.
    pub fn push(&mut self, item: StreamItem) -> Option<PendingSegment> {
        match item {
            StreamItem::Info(info) => {
                // New parameters (or a reconnect): close the current segment first.
                let done = self.flush();
                self.info = Some(info);
                self.anchor = None;
                done
            }
            StreamItem::Unit(unit) => {
                self.info.as_ref()?;
                if self.last.is_none() && self.samples.is_empty() && !unit.is_keyframe {
                    return None; // segments must start with a keyframe
                }
                let mut done = None;
                if let Some(prev) = self.last.take() {
                    let duration = match unit.ts_90k - prev.ts_90k {
                        d if d > 0 && d <= 5 * 90_000 => d as u32,
                        _ => FALLBACK_DURATION_90K,
                    };
                    self.add(prev, duration);
                    let long_enough = self.samples.last().is_some_and(|last| {
                        last.wall_us - self.samples[0].wall_us + i64::from(duration) * 100 / 9
                            >= self.segment_us
                    });
                    if unit.is_keyframe && (long_enough || self.bytes > MAX_BUFFER_BYTES) {
                        done = self.take();
                    }
                }
                self.last = Some(unit);
                done
            }
        }
    }

    /// Returns whatever is buffered as a (possibly short) segment. Use at shutdown.
    pub fn flush(&mut self) -> Option<PendingSegment> {
        if let Some(prev) = self.last.take() {
            let duration = self
                .samples
                .last()
                .map_or(FALLBACK_DURATION_90K, |s| s.duration_90k);
            self.add(prev, duration);
        }
        self.take()
    }

    fn add(&mut self, unit: AccessUnit, duration_90k: u32) {
        let arrived = unit.received_at.timestamp_micros();
        let (anchor_wall, anchor_ts) = *self.anchor.get_or_insert((arrived, unit.ts_90k));
        let mut wall_us = anchor_wall + (unit.ts_90k - anchor_ts) * 100 / 9;
        if (wall_us - arrived).abs() > MAX_DRIFT_US {
            self.anchor = Some((arrived, unit.ts_90k));
            wall_us = arrived;
        }
        self.bytes += unit.avcc.len();
        self.samples.push(Sample {
            data: unit.avcc,
            duration_90k,
            is_key: unit.is_keyframe,
            wall_us,
        });
    }

    fn take(&mut self) -> Option<PendingSegment> {
        self.bytes = 0;
        let samples = std::mem::take(&mut self.samples);
        let info = self.info.clone()?;
        (!samples.is_empty()).then_some(PendingSegment { info, samples })
    }
}

/// Contents of a segment's `.idx` file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentIndex {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub decoder_config_hex: String,
    /// Offsets are into the `.mp4`; `wall_us` are absolute (µs since the Unix epoch).
    pub samples: Vec<SampleIndexEntry>,
}

impl SegmentIndex {
    /// The stream parameters stored in the index.
    pub fn info(&self) -> Option<StreamInfo> {
        let codec = match self.codec.as_str() {
            "h264" => Codec::H264,
            "h265" => Codec::H265,
            _ => return None,
        };
        Some(StreamInfo {
            codec,
            width: self.width,
            height: self.height,
            decoder_config: bytes::Bytes::from(unhex(&self.decoder_config_hex)?),
        })
    }
}

/// A segment written to disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenSegment {
    /// Paths relative to the data directory.
    pub path: String,
    pub index_path: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub bytes: u64,
}

/// Writes `segment` under `data_dir/recordings/<camera>/<YYYYMMDD>/` as `<start>.mp4` plus
/// `<start>.idx`. Blocking; call from a blocking thread.
pub fn write_segment(
    data_dir: &Path,
    camera: &str,
    segment: &PendingSegment,
) -> std::io::Result<WrittenSegment> {
    let started_at = segment.started_at();
    let day = started_at.format("%Y%m%d").to_string();
    let stem = started_at.format("%Y%m%dT%H%M%S%.6fZ").to_string();
    let rel_dir = PathBuf::from("recordings").join(camera).join(&day);
    std::fs::create_dir_all(data_dir.join(&rel_dir))?;
    let rel_mp4 = rel_dir.join(format!("{stem}.mp4"));
    let rel_idx = rel_dir.join(format!("{stem}.idx"));

    let mp4_path = data_dir.join(&rel_mp4);
    let tmp = mp4_path.with_extension("mp4.tmp");
    let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    let entries = write_mp4(file, &segment.info, &segment.samples)?;
    std::fs::rename(&tmp, &mp4_path)?;
    let bytes = std::fs::metadata(&mp4_path)?.len();

    let index = SegmentIndex {
        codec: match segment.info.codec {
            Codec::H264 => "h264".into(),
            Codec::H265 => "h265".into(),
        },
        width: segment.info.width,
        height: segment.info.height,
        decoder_config_hex: hex(&segment.info.decoder_config),
        samples: entries,
    };
    crate::snapshot::write_atomic(&data_dir.join(&rel_idx), &serde_json::to_vec(&index)?)?;
    Ok(WrittenSegment {
        path: rel_mp4.to_string_lossy().into_owned(),
        index_path: rel_idx.to_string_lossy().into_owned(),
        started_at,
        ended_at: segment.ended_at(),
        bytes,
    })
}

/// Records a stream: builds segments from `items`, writes them under `data_dir`, and sends each
/// written segment on `written` (the pipeline stores it). When `items` closes (the source
/// stopped), the partial segment is written too.
pub fn spawn_recorder(
    camera: String,
    data_dir: PathBuf,
    segment_seconds: u32,
    mut items: tokio::sync::mpsc::Receiver<StreamItem>,
    written: tokio::sync::mpsc::Sender<WrittenSegment>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut builder = SegmentBuilder::new(segment_seconds);
        loop {
            let (segment, last) = match items.recv().await {
                Some(item) => (builder.push(item), false),
                None => (builder.flush(), true),
            };
            if let Some(segment) = segment {
                let (dir, cam) = (data_dir.clone(), camera.clone());
                match tokio::task::spawn_blocking(move || write_segment(&dir, &cam, &segment)).await
                {
                    Ok(Ok(w)) => {
                        let _ = written.send(w).await;
                    }
                    Ok(Err(e)) => tracing::warn!(camera = %camera, "cannot write segment: {e}"),
                    Err(e) => tracing::warn!(camera = %camera, "segment writer failed: {e}"),
                }
            }
            if last {
                return;
            }
        }
    })
}

/// Reads a segment's `.idx` file.
pub fn read_segment_index(path: &Path) -> std::io::Result<SegmentIndex> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use bytes::Bytes;
    use chrono::TimeZone;

    use super::*;
    use crate::h264::tests::fixture;
    use crate::h264::{NAL_IDR, NAL_PPS, NAL_SPS, annexb_nals, build_avc_config, nal_type};

    /// The fixture as live stream items: 10 fps, arriving from `start`, with a little jitter.
    pub(crate) fn live_items(start: DateTime<Utc>, repeat: usize) -> Vec<StreamItem> {
        let data = fixture("testsrc_high_640x360_10fps.h264");
        let nals = annexb_nals(&data);
        let sps = nals.iter().find(|n| nal_type(n) == NAL_SPS).unwrap();
        let pps = nals.iter().find(|n| nal_type(n) == NAL_PPS).unwrap();
        let mut items = vec![StreamItem::Info(StreamInfo {
            codec: Codec::H264,
            width: 640,
            height: 360,
            decoder_config: Bytes::from(build_avc_config(sps, pps).unwrap()),
        })];
        let slices: Vec<&[u8]> = nals
            .iter()
            .copied()
            .filter(|n| matches!(nal_type(n), 1 | 5))
            .collect();
        let mut n = 0i64;
        for _ in 0..repeat {
            for nal in &slices {
                let mut avcc = (nal.len() as u32).to_be_bytes().to_vec();
                avcc.extend_from_slice(nal);
                let jitter = (n % 3) * 7; // ms
                items.push(StreamItem::Unit(AccessUnit {
                    received_at: start + chrono::Duration::milliseconds(n * 100 + jitter),
                    ts_90k: 123_456 + n * 9000,
                    is_keyframe: nal_type(nal) == NAL_IDR,
                    avcc: Bytes::from(avcc),
                }));
                n += 1;
            }
        }
        items
    }

    pub(crate) fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, 12, 0, 0).unwrap()
    }

    #[test]
    fn segments_start_on_keyframes_and_cover_the_stream_without_gaps() {
        let mut builder = SegmentBuilder::new(4);
        let mut segments = Vec::new();
        for item in live_items(start(), 3) {
            segments.extend(builder.push(item));
        }
        segments.extend(builder.flush());
        // 30 s at a keyframe every 2 s, cut after >= 4 s: 7 full segments + the rest.
        assert!(segments.len() >= 7, "{} segments", segments.len());
        let total: usize = segments.iter().map(|s| s.samples.len()).sum();
        assert_eq!(total, 300);
        for s in &segments {
            assert!(s.samples[0].is_key);
        }
        for pair in segments.windows(2) {
            let gap = pair[1].started_at() - pair[0].ended_at();
            assert!(gap.num_milliseconds().abs() <= 1, "gap {gap}");
        }
        assert_eq!(segments[0].started_at(), start());
    }

    #[test]
    fn frames_before_the_first_keyframe_are_skipped_and_info_changes_flush() {
        let mut builder = SegmentBuilder::new(10);
        let mut items = live_items(start(), 1);
        items.remove(1); // drop the first keyframe
        let mut segments = Vec::new();
        for item in items.iter().cloned() {
            segments.extend(builder.push(item));
        }
        // A reconnect sends Info again: the buffered frames become a segment.
        segments.extend(builder.push(items[0].clone()));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].samples.len(), 80); // frames 20..100
        assert!(builder.flush().is_none());
    }

    #[tokio::test]
    async fn recorder_task_writes_segments_and_flushes_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let (item_tx, item_rx) = tokio::sync::mpsc::channel(512);
        let (seg_tx, mut seg_rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_recorder("cam".into(), dir.path().to_path_buf(), 4, item_rx, seg_tx);
        for item in live_items(start(), 1) {
            item_tx.send(item).await.unwrap();
        }
        drop(item_tx); // the source stopped
        handle.await.unwrap();
        let mut segments = Vec::new();
        while let Some(s) = seg_rx.recv().await {
            segments.push(s);
        }
        // 10 s in 4 s segments: 4 + 4 + the final 2 s.
        assert_eq!(segments.len(), 3);
        for s in &segments {
            assert!(dir.path().join(&s.path).exists());
            assert!(dir.path().join(&s.index_path).exists());
        }
    }

    #[test]
    fn large_clock_drift_re_anchors_to_the_wall_clock() {
        let mut builder = SegmentBuilder::new(60);
        let mut items = live_items(start(), 1);
        // From frame 50 on, frames arrive 2 s later than their timestamps say.
        for item in items.iter_mut().skip(51) {
            if let StreamItem::Unit(u) = item {
                u.received_at += chrono::Duration::seconds(2);
            }
        }
        for item in items {
            assert!(builder.push(item).is_none());
        }
        let segment = builder.flush().unwrap();
        let wall = |i: usize| segment.samples[i].wall_us - start().timestamp_micros();
        assert_eq!(wall(49), 4_900_000);
        assert!((wall(50) - 7_000_000).abs() < 20_000, "{}", wall(50));
        assert!((wall(99) - 11_900_000).abs() < 20_000, "{}", wall(99));
    }

    #[test]
    fn written_segment_and_index_agree() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = SegmentBuilder::new(10);
        let mut segment = None;
        for item in live_items(start(), 2) {
            if let Some(s) = builder.push(item) {
                segment = Some(s);
                break;
            }
        }
        let segment = segment.unwrap();
        let written = write_segment(dir.path(), "cam", &segment).unwrap();
        assert_eq!(
            written.path,
            "recordings/cam/20260918/20260918T120000.000000Z.mp4"
        );
        let index = read_segment_index(&dir.path().join(&written.index_path)).unwrap();
        assert_eq!(index.info().unwrap(), segment.info);
        assert_eq!(index.samples.len(), segment.samples.len());
        assert_eq!(index.samples[0].wall_us, start().timestamp_micros());
        let (_, entries) = crate::mp4r::read_mp4_index(&dir.path().join(&written.path)).unwrap();
        assert_eq!(entries.len(), index.samples.len());
        assert_eq!(entries[5].offset, index.samples[5].offset);
    }
}
