//! H.264 decoding (plan Step 2.3).
//!
//! Two interchangeable decoders behind [`H264Decoder`]:
//! - [`RustDecoder`]: pure Rust (`rusty_h264-decoder`), in process. The default.
//! - [`FfmpegPipeDecoder`]: the `ffmpeg` executable as a child process, fed Annex-B on stdin and
//!   read as raw I420 on stdout. Used only if Phase 0 shows the pure-Rust decoder cannot handle
//!   the owner's cameras. No FFI either way.
//!
//! [`spawn_decode_worker`] runs one decoder per camera on its own thread and turns
//! [`StreamItem`]s into [`Frame`]s at the camera's analysis rate.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use zoologist_core::{CameraId, Frame};

use crate::h264::{AvcConfig, avcc_to_annexb, param_sets_annexb, parse_avc_config};
use crate::stream::{Codec, StreamItem};

/// Frames wider than this are downscaled before analysis, unless the worker is given another
/// limit (`video.analysis_max_width`).
pub const DEFAULT_ANALYSIS_MAX_WIDTH: u32 = 1536;

/// Decoding failed.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("decoder error: {0}")]
    Decoder(String),
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
}

/// One decoded picture in I420 layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedYuv {
    pub width: u32,
    pub height: u32,
    pub i420: Vec<u8>,
}

/// An H.264 decoder. Pictures come out in the order access units went in (the cameras send no
/// B-frames), possibly with a delay of a few pictures.
pub trait H264Decoder: Send {
    /// Short name for logs and the health endpoint.
    fn name(&self) -> &'static str;
    /// Feeds one access unit in Annex-B framing; returns the pictures that became available.
    fn decode(&mut self, annexb: &[u8]) -> Result<Vec<DecodedYuv>, DecodeError>;
}

// ---------------------------------------------------------------------------------------------
// Pure Rust
// ---------------------------------------------------------------------------------------------

/// The pure-Rust decoder.
#[derive(Default)]
pub struct RustDecoder {
    inner: rusty_h264_decoder::Decoder,
}

impl RustDecoder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl H264Decoder for RustDecoder {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn decode(&mut self, annexb: &[u8]) -> Result<Vec<DecodedYuv>, DecodeError> {
        let picture = self
            .inner
            .decode(annexb)
            .map_err(|e| DecodeError::Decoder(e.to_string()))?;
        Ok(picture
            .map(|p| {
                let mut i420 = Vec::with_capacity(p.y.len() + p.u.len() + p.v.len());
                i420.extend_from_slice(&p.y);
                i420.extend_from_slice(&p.u);
                i420.extend_from_slice(&p.v);
                DecodedYuv {
                    width: p.width as u32,
                    height: p.height as u32,
                    i420,
                }
            })
            .into_iter()
            .collect())
    }
}

// ---------------------------------------------------------------------------------------------
// ffmpeg child process
// ---------------------------------------------------------------------------------------------

/// Decodes with an `ffmpeg` child process. The output size must be known up front (from the
/// SPS), because raw video on a pipe has no framing.
pub struct FfmpegPipeDecoder {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: std_mpsc::Receiver<Vec<u8>>,
    width: u32,
    height: u32,
}

impl FfmpegPipeDecoder {
    /// Starts `ffmpeg` for a `width × height` H.264 stream.
    pub fn spawn(ffmpeg: &Path, width: u32, height: u32) -> Result<Self, DecodeError> {
        let mut child = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                // Not "-fflags nobuffer": with piped raw H.264 it makes ffmpeg drop frames.
                "-flags",
                "low_delay",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| DecodeError::Ffmpeg(format!("cannot start {}: {e}", ffmpeg.display())))?;
        let stdin = child.stdin.take();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| DecodeError::Ffmpeg("no stdout".into()))?;
        let frame_len = Frame::i420_len(width, height);
        let (tx, frames) = std_mpsc::channel();
        // Reading on its own thread avoids a deadlock where we block writing stdin while ffmpeg
        // blocks writing a full stdout pipe.
        std::thread::Builder::new()
            .name("ffmpeg-decode-read".into())
            .spawn(move || {
                loop {
                    let mut frame = vec![0u8; frame_len];
                    if stdout.read_exact(&mut frame).is_err() || tx.send(frame).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| DecodeError::Ffmpeg(e.to_string()))?;
        Ok(Self {
            child,
            stdin,
            frames,
            width,
            height,
        })
    }

    /// Closes stdin and returns every remaining picture (used by tests and file analysis).
    pub fn finish(mut self) -> Vec<DecodedYuv> {
        drop(self.stdin.take());
        let out = self.frames.iter().map(|i420| self.picture(i420)).collect();
        let _ = self.child.wait();
        out
    }

    fn picture(&self, i420: Vec<u8>) -> DecodedYuv {
        DecodedYuv {
            width: self.width,
            height: self.height,
            i420,
        }
    }
}

impl H264Decoder for FfmpegPipeDecoder {
    fn name(&self) -> &'static str {
        "ffmpeg"
    }

    fn decode(&mut self, annexb: &[u8]) -> Result<Vec<DecodedYuv>, DecodeError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| DecodeError::Ffmpeg("already finished".into()))?;
        stdin
            .write_all(annexb)
            .and_then(|()| stdin.flush())
            .map_err(|e| DecodeError::Ffmpeg(format!("ffmpeg exited: {e}")))?;
        Ok(self
            .frames
            .try_iter()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|i420| self.picture(i420))
            .collect())
    }
}

impl Drop for FfmpegPipeDecoder {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// Per-camera decode worker
// ---------------------------------------------------------------------------------------------

/// Which decoder the worker creates.
#[derive(Clone, Debug)]
pub enum DecoderChoice {
    Rust,
    Ffmpeg(std::path::PathBuf),
}

/// Counters published by a decode worker.
#[derive(Clone, Debug, Default)]
pub struct DecodeStats {
    pub decoded: u64,
    pub emitted: u64,
    /// Frames dropped because analysis was not keeping up.
    pub dropped: u64,
    pub errors: u64,
    /// Rolling mean decode time per access unit, milliseconds.
    pub mean_decode_ms: f32,
}

/// Decodes a camera's detect stream on a dedicated thread. Every access unit is decoded (later
/// frames depend on earlier ones), but only one frame per `1 / detect_fps` seconds of arrival
/// time is sent on `frames`. If `frames` is full the frame is dropped and counted, unless
/// `lossless` is set (offline file analysis), in which case the worker waits.
pub fn spawn_decode_worker(
    camera_id: CameraId,
    choice: DecoderChoice,
    detect_fps: u32,
    mut items: mpsc::Receiver<StreamItem>,
    frames: mpsc::Sender<Frame>,
    stats: Arc<std::sync::RwLock<DecodeStats>>,
    lossless: bool,
    max_width: u32,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("decode-{camera_id}"))
        .spawn(move || {
            let mut worker =
                DecodeWorker::new(camera_id, choice, detect_fps).with_max_width(max_width);
            while let Some(item) = items.blocking_recv() {
                for frame in worker.handle(item) {
                    if lossless {
                        if frames.blocking_send(frame).is_err() {
                            return;
                        }
                        stats.write().unwrap_or_else(|e| e.into_inner()).emitted += 1;
                        continue;
                    }
                    let mut s = stats.write().unwrap_or_else(|e| e.into_inner());
                    match frames.try_send(frame) {
                        Ok(()) => s.emitted += 1,
                        Err(mpsc::error::TrySendError::Full(_)) => s.dropped += 1,
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
                let mut s = stats.write().unwrap_or_else(|e| e.into_inner());
                s.decoded = worker.decoded;
                s.errors = worker.errors;
                s.mean_decode_ms = worker.mean_decode_ms;
            }
        })
}

/// The decode worker's logic without the thread, so it can be tested directly.
pub struct DecodeWorker {
    camera_id: CameraId,
    choice: DecoderChoice,
    min_interval: chrono::Duration,
    /// Frames wider than this are scaled down before analysis.
    max_width: u32,
    decoder: Option<Box<dyn H264Decoder>>,
    config: Option<AvcConfig>,
    /// Arrival times of access units whose pictures have not come out yet.
    pending: VecDeque<DateTime<Utc>>,
    last_emitted: Option<DateTime<Utc>>,
    seen_keyframe: bool,
    seq: u64,
    annexb: Vec<u8>,
    pub decoded: u64,
    pub errors: u64,
    pub mean_decode_ms: f32,
}

impl DecodeWorker {
    pub fn new(camera_id: CameraId, choice: DecoderChoice, detect_fps: u32) -> Self {
        Self {
            camera_id,
            choice,
            min_interval: chrono::Duration::microseconds(1_000_000 / i64::from(detect_fps.max(1))),
            max_width: DEFAULT_ANALYSIS_MAX_WIDTH,
            decoder: None,
            config: None,
            pending: VecDeque::new(),
            last_emitted: None,
            seen_keyframe: false,
            seq: 0,
            annexb: Vec::new(),
            decoded: 0,
            errors: 0,
            mean_decode_ms: 0.0,
        }
    }

    /// Scales frames wider than `max_width` down before analysis.
    pub fn with_max_width(mut self, max_width: u32) -> Self {
        self.max_width = max_width.max(2);
        self
    }

    /// Handles one stream item and returns the frames to analyse (usually zero or one).
    pub fn handle(&mut self, item: StreamItem) -> Vec<Frame> {
        match item {
            StreamItem::Info(info) => {
                if info.codec != Codec::H264 {
                    warn!(camera = %self.camera_id, "detect stream is not H.264; cannot decode it");
                    self.config = None;
                    self.decoder = None;
                    return Vec::new();
                }
                match parse_avc_config(&info.decoder_config) {
                    Ok(config) => {
                        // New parameters: start a fresh decoder at the next keyframe.
                        self.decoder = None;
                        self.pending.clear();
                        self.seen_keyframe = false;
                        self.config = Some(config);
                    }
                    Err(e) => {
                        warn!(camera = %self.camera_id, "bad stream parameters: {e}");
                        self.config = None;
                    }
                }
                Vec::new()
            }
            StreamItem::Unit(unit) => {
                let Some(config) = &self.config else {
                    return Vec::new();
                };
                if !self.seen_keyframe {
                    if !unit.is_keyframe {
                        return Vec::new();
                    }
                    self.seen_keyframe = true;
                }
                if self.decoder.is_none() {
                    match make_decoder(&self.choice, config) {
                        Ok(d) => self.decoder = Some(d),
                        Err(e) => {
                            warn!(camera = %self.camera_id, "cannot start decoder: {e}");
                            self.errors += 1;
                            return Vec::new();
                        }
                    }
                }
                self.annexb.clear();
                if unit.is_keyframe {
                    self.annexb.extend_from_slice(&param_sets_annexb(config));
                }
                if let Err(e) = avcc_to_annexb(&unit.avcc, config.nal_length_size, &mut self.annexb)
                {
                    debug!(camera = %self.camera_id, "skipping malformed access unit: {e}");
                    self.errors += 1;
                    return Vec::new();
                }
                self.pending.push_back(unit.received_at);
                let started = std::time::Instant::now();
                let decoder = self.decoder.as_mut().expect("created above");
                let pictures = match decoder.decode(&self.annexb) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(camera = %self.camera_id, "decode failed, waiting for next keyframe: {e}");
                        self.errors += 1;
                        self.decoder = None;
                        self.pending.clear();
                        self.seen_keyframe = false;
                        return Vec::new();
                    }
                };
                let ms = started.elapsed().as_secs_f32() * 1000.0;
                self.mean_decode_ms = if self.decoded == 0 {
                    ms
                } else {
                    self.mean_decode_ms * 0.95 + ms * 0.05
                };
                self.decoded += 1;
                pictures
                    .into_iter()
                    .filter_map(|picture| {
                        let at = self.pending.pop_front().unwrap_or(unit.received_at);
                        self.maybe_emit(picture, at)
                    })
                    .collect()
            }
        }
    }

    fn maybe_emit(&mut self, picture: DecodedYuv, at: DateTime<Utc>) -> Option<Frame> {
        // 10 % slack: network jitter must not turn "every other frame" into "every third".
        if let Some(last) = self.last_emitted
            && (at - last) * 10 < self.min_interval * 9
        {
            return None;
        }
        self.last_emitted = Some(at);
        let (width, height, i420) = if picture.width > self.max_width {
            downscale(picture, self.max_width)?
        } else {
            (picture.width, picture.height, picture.i420)
        };
        self.seq += 1;
        Some(Frame {
            camera_id: self.camera_id.clone(),
            seq: self.seq,
            captured_at: at,
            width,
            height,
            i420: Arc::new(i420),
        })
    }
}

fn make_decoder(
    choice: &DecoderChoice,
    config: &AvcConfig,
) -> Result<Box<dyn H264Decoder>, DecodeError> {
    Ok(match choice {
        DecoderChoice::Rust => Box::new(RustDecoder::new()),
        DecoderChoice::Ffmpeg(path) => {
            Box::new(FfmpegPipeDecoder::spawn(path, config.width, config.height)?)
        }
    })
}

/// Downscales a picture to `max_width` wide (even), keeping the aspect ratio (even height).
fn downscale(picture: DecodedYuv, max_width: u32) -> Option<(u32, u32, Vec<u8>)> {
    let out_w = max_width & !1;
    let out_h = ((picture.height as u64 * out_w as u64 / picture.width as u64) as u32 + 1) & !1;
    zoologist_core::yuv::scale_i420(&picture.i420, picture.width, picture.height, out_w, out_h)
        .ok()
        .map(|i420| (out_w, out_h, i420))
}

/// Waits briefly for trailing pictures from an ffmpeg decoder (tests only).
#[cfg(test)]
fn settle() {
    std::thread::sleep(std::time::Duration::from_millis(200));
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use bytes::Bytes;
    use chrono::TimeZone;

    use super::*;
    use crate::h264::tests::fixture;
    use crate::h264::{NAL_IDR, NAL_PPS, NAL_SPS, annexb_nals, build_avc_config, nal_type};
    use crate::stream::{AccessUnit, StreamInfo};

    /// Splits an Annex-B fixture into stream items, as a source would send them.
    fn fixture_items(name: &str, fps: i64) -> Vec<StreamItem> {
        let data = fixture(name);
        let nals = annexb_nals(&data);
        let sps = nals.iter().find(|n| nal_type(n) == NAL_SPS).unwrap();
        let pps = nals.iter().find(|n| nal_type(n) == NAL_PPS).unwrap();
        let config = build_avc_config(sps, pps).unwrap();
        let parsed = parse_avc_config(&config).unwrap();
        let mut items = vec![StreamItem::Info(StreamInfo {
            codec: Codec::H264,
            width: parsed.width,
            height: parsed.height,
            decoder_config: Bytes::from(config),
        })];
        let start = Utc.with_ymd_and_hms(2026, 9, 18, 12, 0, 0).unwrap();
        let slices = nals.iter().filter(|n| matches!(nal_type(n), 1 | 5));
        for (n, nal) in (0i64..).zip(slices) {
            let mut avcc = (nal.len() as u32).to_be_bytes().to_vec();
            avcc.extend_from_slice(nal);
            items.push(StreamItem::Unit(AccessUnit {
                received_at: start + chrono::Duration::milliseconds(n * 1000 / fps),
                ts_90k: n * 90_000 / fps,
                is_keyframe: nal_type(nal) == NAL_IDR,
                avcc: Bytes::from(avcc),
            }));
        }
        items
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// Decodes a whole fixture with ffmpeg as the reference.
    fn reference(name: &str) -> Vec<DecodedYuv> {
        let mut decoder = FfmpegPipeDecoder::spawn(Path::new("ffmpeg"), 640, 360).unwrap();
        let mut out = decoder.decode(&fixture(name)).unwrap();
        settle();
        out.extend(decoder.finish());
        out
    }

    fn psnr_y(a: &DecodedYuv, b: &DecodedYuv) -> f64 {
        let n = (a.width * a.height) as usize;
        let mse: f64 = a.i420[..n]
            .iter()
            .zip(&b.i420[..n])
            .map(|(&x, &y)| {
                let d = x as f64 - y as f64;
                d * d
            })
            .sum::<f64>()
            / n as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    #[test]
    fn worker_throttles_to_detect_fps_and_keeps_arrival_times() {
        // 10 fps in, 5 fps out.
        let mut worker = DecodeWorker::new("cam".into(), DecoderChoice::Rust, 5);
        let mut frames = Vec::new();
        for item in fixture_items("testsrc_main_640x360_10fps.h264", 10) {
            frames.extend(worker.handle(item));
        }
        assert_eq!(worker.decoded, 100, "errors: {}", worker.errors);
        assert_eq!(frames.len(), 50);
        assert_eq!((frames[0].width, frames[0].height), (640, 360));
        assert_eq!(frames[0].i420.len(), Frame::i420_len(640, 360));
        let gap = frames[1].captured_at - frames[0].captured_at;
        assert_eq!(gap, chrono::Duration::milliseconds(200));
        assert!(frames.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    }

    #[test]
    fn frames_wider_than_the_limit_are_scaled_down() {
        let mut worker =
            DecodeWorker::new("cam".into(), DecoderChoice::Rust, 5).with_max_width(320);
        let frame = fixture_items("testsrc_main_640x360_10fps.h264", 10)
            .into_iter()
            .flat_map(|item| worker.handle(item))
            .next()
            .expect("a frame");
        assert_eq!((frame.width, frame.height), (320, 180));
        assert_eq!(frame.i420.len(), Frame::i420_len(320, 180));
    }

    #[test]
    fn throttle_tolerates_arrival_jitter() {
        let mut worker = DecodeWorker::new("cam".into(), DecoderChoice::Rust, 5);
        let mut items = fixture_items("testsrc_main_640x360_10fps.h264", 10);
        // Every other frame arrives 5 ms early.
        for (i, item) in items.iter_mut().enumerate() {
            if let StreamItem::Unit(unit) = item
                && i % 2 == 0
            {
                unit.received_at -= chrono::Duration::milliseconds(5);
            }
        }
        let frames: usize = items.into_iter().map(|i| worker.handle(i).len()).sum();
        assert_eq!(frames, 50);
    }

    #[test]
    fn worker_waits_for_a_keyframe() {
        let mut worker = DecodeWorker::new("cam".into(), DecoderChoice::Rust, 10);
        let items = fixture_items("testsrc_main_640x360_10fps.h264", 10);
        // Info, then skip the first keyframe: nothing decodes until the next keyframe (#21).
        let mut frames = worker.handle(items[0].clone());
        for item in items.into_iter().skip(2) {
            frames.extend(worker.handle(item));
        }
        assert_eq!(frames.len(), 80);
    }

    #[test]
    fn rust_decoder_matches_ffmpeg_on_main_profile() {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let reference = reference("testsrc_main_640x360_10fps.h264");
        let mut worker = DecodeWorker::new("cam".into(), DecoderChoice::Rust, 100);
        let frames: Vec<Frame> = fixture_items("testsrc_main_640x360_10fps.h264", 10)
            .into_iter()
            .flat_map(|i| worker.handle(i))
            .collect();
        assert_eq!(frames.len(), reference.len());
        for (frame, reference) in frames.iter().zip(&reference) {
            let ours = DecodedYuv {
                width: frame.width,
                height: frame.height,
                i420: frame.i420.to_vec(),
            };
            assert!(psnr_y(&ours, reference) >= 40.0);
        }
    }

    /// Decodes `items` with the pure-Rust decoder and with ffmpeg (the reference), from the same
    /// Annex-B bytes, and prints one table row: frames, errors, ms per frame, worst Y-PSNR.
    fn report_row(name: &str, items: &[StreamItem]) {
        let Some(StreamItem::Info(info)) = items.first() else {
            println!("| {name} | no stream info |");
            return;
        };
        if info.codec != Codec::H264 {
            println!("| {name} | {:?}: not H.264, skipped |||||", info.codec);
            return;
        }
        let config = parse_avc_config(&info.decoder_config).unwrap();
        // One Annex-B access unit per sample, with parameter sets before keyframes.
        let units: Vec<Vec<u8>> = items[1..]
            .iter()
            .filter_map(|item| match item {
                StreamItem::Unit(unit) => {
                    let mut annexb = Vec::new();
                    if unit.is_keyframe {
                        annexb.extend(param_sets_annexb(&config));
                    }
                    avcc_to_annexb(&unit.avcc, 4, &mut annexb).ok()?;
                    Some(annexb)
                }
                StreamItem::Info(_) => None,
            })
            .collect();

        let mut reference_decoder =
            FfmpegPipeDecoder::spawn(Path::new("ffmpeg"), config.width, config.height).unwrap();
        let mut reference = reference_decoder.decode(&units.concat()).unwrap();
        settle();
        reference.extend(reference_decoder.finish());

        let mut decoder = RustDecoder::new();
        let (mut pictures, mut errors) = (Vec::new(), 0);
        let start = Instant::now();
        for unit in &units {
            match decoder.decode(unit) {
                Ok(p) => pictures.extend(p),
                Err(e) => {
                    if errors == 0 {
                        println!("<!-- {name}: first error: {e} -->");
                    }
                    errors += 1;
                }
            }
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / units.len().max(1) as f64;
        let min_psnr = pictures
            .iter()
            .zip(&reference)
            .map(|(a, b)| psnr_y(a, b))
            .fold(f64::INFINITY, f64::min);
        println!(
            "| {name} | {}×{} | {}/{} | {errors} | {ms:.2} | {} |",
            config.width,
            config.height,
            pictures.len(),
            reference.len(),
            if pictures.is_empty() {
                "-".into()
            } else {
                format!("{min_psnr:.1} dB")
            }
        );
    }

    /// The Phase 0 decoder comparison: the synthetic fixtures, plus the owner's captures and Hub
    /// recordings when `tools/fixtures/owner/` has them (`zoologist capture`, `zoologist
    /// hub-test`). Prints a table.
    /// Run with `cargo test --release -p zoologist-video -- --ignored --nocapture decoder_report`.
    #[test]
    #[ignore]
    fn decoder_report() {
        println!(
            "\n| Input | Size | Frames (rust/ffmpeg) | Errors | ms/frame (rust) | min Y-PSNR vs ffmpeg |"
        );
        println!("|---|---|---|---|---|---|");
        for name in [
            "testsrc_main_640x360_10fps.h264",
            "testsrc_high_640x360_10fps.h264",
            "moving_square_640x360_10fps.h264",
        ] {
            report_row(name, &fixture_items(name, 10));
        }
        let owner = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/fixtures/owner");
        let mut captures: Vec<_> = std::fs::read_dir(&owner)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.to_string_lossy().ends_with(".units.jsonl"))
            .collect();
        captures.sort();
        for units in captures {
            let base = units
                .to_string_lossy()
                .trim_end_matches(".units.jsonl")
                .to_string();
            let name = Path::new(&base)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            match crate::capture::read_capture(Path::new(&base)) {
                Ok(items) => report_row(&name, &items),
                Err(e) => println!("| {name} | cannot read: {e} |||||"),
            }
        }
        let mut hub: Vec<_> = std::fs::read_dir(owner.join("hub"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "mp4"))
            .collect();
        hub.sort();
        for path in hub {
            let name = format!("hub/{}", path.file_name().unwrap().to_string_lossy());
            let (info, entries) = match crate::mp4r::read_mp4_index(&path) {
                Ok(x) => x,
                Err(e) => {
                    println!("| {name} | cannot read: {e} |||||");
                    continue;
                }
            };
            let samples = crate::mp4r::read_samples(&path, &entries).unwrap();
            let start = Utc::now();
            let items: Vec<StreamItem> = std::iter::once(StreamItem::Info(info))
                .chain(samples.into_iter().map(|s| {
                    StreamItem::Unit(AccessUnit {
                        received_at: start + chrono::Duration::microseconds(s.wall_us),
                        ts_90k: s.wall_us * 9 / 100,
                        is_keyframe: s.is_key,
                        avcc: s.data,
                    })
                }))
                .collect();
            report_row(&name, &items);
        }
    }
}
