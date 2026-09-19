//! Live stream sources: RTSP via retina (plan Step 2.1) and Reolink HTTP-FLV (Step 2.2).
//!
//! Both send the same [`StreamItem`]s, reconnect with backoff, and keep a [`StreamStatus`] up
//! to date. Everything downstream (decoder, recorder) is shared.

use std::io::Read;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zoologist_core::config::{CameraConfig, Transport};
use zoologist_core::redact_url;

use crate::flv::{FlvError, FlvItem, FlvParser};
use crate::h264::parse_avc_config;
use crate::http;
use crate::stream::{
    AccessUnit, Codec, StreamInfo, StreamItem, StreamState, StreamStatus, backoff_seconds,
};

/// A stream that sends nothing for this long is treated as dead and reconnected.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// A session that streamed at least this long resets the backoff to its first step.
const HEALTHY_SESSION: Duration = Duration::from_secs(60);

/// Shared, cheaply clonable status handle.
pub type SharedStatus = Arc<RwLock<StreamStatus>>;

/// Which of a camera's two streams to open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamRole {
    /// Low-resolution substream, decoded for analysis.
    Detect,
    /// Main stream, recorded without decoding.
    Record,
}

/// Starts the source for one of a `kind = "stream"` camera's streams.
///
/// Returns `None` if the camera has no URL for that role (a validated config always has one).
pub fn spawn_source(
    camera: &CameraConfig,
    role: StreamRole,
    tx: mpsc::Sender<StreamItem>,
    status: SharedStatus,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let url = match role {
        StreamRole::Detect => camera.detect_url.clone()?,
        StreamRole::Record => camera.record_url.clone()?,
    };
    let label = format!(
        "{}/{}",
        camera.id,
        if role == StreamRole::Detect {
            "detect"
        } else {
            "record"
        }
    );
    if url.starts_with("file://") {
        return Some(crate::file_source::spawn_file_stream(
            url,
            chrono::Utc::now(),
            false,
            tx,
            status,
            cancel,
        ));
    }
    Some(match camera.transport? {
        Transport::Rtsp => spawn_rtsp_stream(label, url, tx, status, cancel),
        Transport::Flv => spawn_flv_stream(label, url, tx, status, cancel),
    })
}

/// How a single connection attempt ended.
enum SessionEnd {
    /// Cancelled, or nobody is listening any more: stop for good.
    Stop,
    /// Connection lost or failed: reconnect after a delay.
    Retry(String),
    /// The stream can never work as configured (e.g. H.265 over FLV): retry slowly.
    Unsupported(String),
}

/// Measures frames per second and bitrate over ~2 s windows and publishes them.
struct Meter {
    window_start: Instant,
    frames: u32,
    bytes: u64,
}

impl Meter {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            frames: 0,
            bytes: 0,
        }
    }

    fn record(&mut self, status: &SharedStatus, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes as u64;
        let elapsed = self.window_start.elapsed().as_secs_f32();
        let mut s = status.write().unwrap_or_else(|e| e.into_inner());
        s.last_unit_at = Some(Utc::now());
        if s.state != StreamState::Streaming {
            s.state = StreamState::Streaming;
            s.last_error = None;
        }
        if elapsed >= 2.0 {
            s.fps_measured = self.frames as f32 / elapsed;
            s.bitrate_kbps = self.bytes as f32 * 8.0 / 1000.0 / elapsed;
            *self = Meter::new();
        }
    }
}

fn set_state(status: &SharedStatus, state: StreamState, error: Option<String>) {
    let mut s = status.write().unwrap_or_else(|e| e.into_inner());
    if matches!(state, StreamState::Backoff) {
        s.reconnects += 1;
    }
    s.state = state;
    if error.is_some() {
        s.last_error = error;
    }
    s.fps_measured = 0.0;
}

/// Decides what happens after each connection attempt: logs, updates the status and returns
/// the delay before the next attempt, or `None` to stop.
struct Reconnector {
    label: String,
    url: String,
    status: SharedStatus,
    attempt: u32,
    started: Instant,
}

impl Reconnector {
    fn new(label: String, url: String, status: SharedStatus) -> Self {
        Self {
            label,
            url,
            status,
            attempt: 0,
            started: Instant::now(),
        }
    }

    /// Call before each attempt.
    fn connecting(&mut self) {
        set_state(&self.status, StreamState::Connecting, None);
        self.started = Instant::now();
    }

    /// Call with the result of each attempt. Returns seconds to wait, or `None` to stop.
    fn ended(&mut self, end: SessionEnd) -> Option<u64> {
        if self.started.elapsed() >= HEALTHY_SESSION {
            self.attempt = 0;
        }
        let delay = match end {
            SessionEnd::Stop => return None,
            SessionEnd::Retry(reason) => {
                warn!(stream = %self.label, url = %redact_url(&self.url), "stream lost: {reason}");
                set_state(&self.status, StreamState::Backoff, Some(reason));
                backoff_seconds(self.attempt)
            }
            SessionEnd::Unsupported(reason) => {
                warn!(stream = %self.label, "stream unusable: {reason}");
                set_state(
                    &self.status,
                    StreamState::Unsupported(reason.clone()),
                    Some(reason),
                );
                30
            }
        };
        self.attempt += 1;
        Some(delay)
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP-FLV
// ---------------------------------------------------------------------------------------------

/// Reads a Reolink HTTP-FLV stream on a blocking thread and forwards [`StreamItem`]s.
pub fn spawn_flv_stream(
    label: String,
    url: String,
    tx: mpsc::Sender<StreamItem>,
    status: SharedStatus,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        info!(stream = %label, url = %redact_url(&url), "starting HTTP-FLV source");
        let mut reconnector = Reconnector::new(label, url.clone(), status.clone());
        while !cancel.is_cancelled() {
            reconnector.connecting();
            let end = flv_session(&url, &tx, &status, &cancel);
            let Some(delay) = reconnector.ended(end) else {
                return;
            };
            // Sleep in small steps so cancellation is noticed quickly.
            let until = Instant::now() + Duration::from_secs(delay);
            while Instant::now() < until && !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

fn flv_session(
    url: &str,
    tx: &mpsc::Sender<StreamItem>,
    status: &SharedStatus,
    cancel: &CancellationToken,
) -> SessionEnd {
    let mut body = match http::get_stream(url, STALL_TIMEOUT) {
        Ok(body) => body,
        Err(e) => return SessionEnd::Retry(e.to_string()),
    };
    let mut parser = FlvParser::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut info: Option<StreamInfo> = None;
    let mut clock = FlvClock::default();
    let mut meter = Meter::new();
    loop {
        if cancel.is_cancelled() {
            return SessionEnd::Stop;
        }
        let n = match body.read(&mut buf) {
            Ok(0) => return SessionEnd::Retry("camera closed the connection".into()),
            Ok(n) => n,
            Err(e) => return SessionEnd::Retry(format!("read failed: {e}")),
        };
        let items = match parser.push(&buf[..n]) {
            Ok(items) => items,
            Err(e @ FlvError::UnsupportedCodec(_)) => {
                return SessionEnd::Unsupported(e.to_string());
            }
            Err(e) => return SessionEnd::Retry(e.to_string()),
        };
        for item in items {
            let out = match item {
                FlvItem::AvcConfig(config) => {
                    let parsed = match parse_avc_config(&config) {
                        Ok(parsed) => parsed,
                        Err(e) => return SessionEnd::Retry(e.to_string()),
                    };
                    let new_info = StreamInfo {
                        codec: Codec::H264,
                        width: parsed.width,
                        height: parsed.height,
                        decoder_config: config,
                    };
                    if info.as_ref() == Some(&new_info) {
                        continue;
                    }
                    info = Some(new_info.clone());
                    StreamItem::Info(new_info)
                }
                FlvItem::Video {
                    dts_ms,
                    cts_ms,
                    keyframe,
                    avcc,
                } => {
                    if info.is_none() {
                        continue; // frames before the sequence header cannot be decoded
                    }
                    meter.record(status, avcc.len());
                    StreamItem::Unit(AccessUnit {
                        received_at: Utc::now(),
                        ts_90k: (clock.unwrap(dts_ms) + cts_ms as i64) * 90,
                        is_keyframe: keyframe,
                        avcc,
                    })
                }
            };
            if tx.blocking_send(out).is_err() {
                return SessionEnd::Stop;
            }
        }
    }
}

/// Turns FLV's 32-bit millisecond timestamps into a monotonic 64-bit count.
#[derive(Default)]
struct FlvClock {
    last: Option<u32>,
    total: i64,
}

impl FlvClock {
    fn unwrap(&mut self, dts_ms: u32) -> i64 {
        if let Some(last) = self.last {
            self.total += dts_ms.wrapping_sub(last) as i32 as i64;
        } else {
            self.total = dts_ms as i64;
        }
        self.last = Some(dts_ms);
        self.total
    }
}

// ---------------------------------------------------------------------------------------------
// RTSP (retina)
// ---------------------------------------------------------------------------------------------

/// Reads an RTSP stream with retina (TCP interleaved) and forwards [`StreamItem`]s.
pub fn spawn_rtsp_stream(
    label: String,
    url: String,
    tx: mpsc::Sender<StreamItem>,
    status: SharedStatus,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!(stream = %label, url = %redact_url(&url), "starting RTSP source");
        let group = Arc::new(retina::client::SessionGroup::default());
        let mut reconnector = Reconnector::new(label, url.clone(), status.clone());
        while !cancel.is_cancelled() {
            reconnector.connecting();
            let end = rtsp_session(&url, &group, &tx, &status, &cancel).await;
            let Some(delay) = reconnector.ended(end) else {
                return;
            };
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
            }
        }
    })
}

/// Splits `rtsp://user:pass@host/...` into a credential-free URL and retina credentials
/// (retina refuses URLs that contain credentials).
fn split_credentials(url: &str) -> Result<(url::Url, Option<retina::client::Credentials>), String> {
    let mut parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let creds = if parsed.username().is_empty() && parsed.password().is_none() {
        None
    } else {
        let decode = |s: &str| {
            url::form_urlencoded::parse(format!("x={s}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let creds = retina::client::Credentials {
            username: decode(parsed.username()),
            password: decode(parsed.password().unwrap_or("")),
        };
        parsed
            .set_username("")
            .and_then(|()| parsed.set_password(None))
            .map_err(|()| "cannot remove credentials from URL".to_string())?;
        Some(creds)
    };
    Ok((parsed, creds))
}

async fn rtsp_session(
    url: &str,
    group: &Arc<retina::client::SessionGroup>,
    tx: &mpsc::Sender<StreamItem>,
    status: &SharedStatus,
    cancel: &CancellationToken,
) -> SessionEnd {
    use retina::client::{PlayOptions, Session, SessionOptions, SetupOptions, Transport};
    use retina::codec::{CodecItem, FrameFormat, ParametersRef};

    let (clean_url, creds) = match split_credentials(url) {
        Ok(parts) => parts,
        Err(e) => return SessionEnd::Unsupported(e),
    };
    let options = SessionOptions::default()
        .creds(creds)
        .user_agent("zoologist".into())
        .session_group(group.clone());

    let connect = async {
        let mut session = Session::describe(clean_url, options).await?;
        let index = session
            .streams()
            .iter()
            .position(|s| s.media() == "video")
            .ok_or_else(|| "no video stream in the camera's SDP".to_string());
        let index = match index {
            Ok(i) => i,
            Err(e) => return Ok(Err(SessionEnd::Unsupported(e))),
        };
        let encoding = session.streams()[index]
            .encoding_name()
            .to_ascii_lowercase();
        let codec = match encoding.as_str() {
            "h264" => Codec::H264,
            "h265" => Codec::H265,
            other => {
                return Ok(Err(SessionEnd::Unsupported(format!(
                    "video codec {other} is not supported; set the camera stream to H.264"
                ))));
            }
        };
        session
            .setup(
                index,
                SetupOptions::default()
                    .transport(Transport::default())
                    .frame_format(FrameFormat::MP4),
            )
            .await?;
        let playing = session.play(PlayOptions::default()).await?;
        Ok::<_, retina::Error>(Ok((playing.demuxed()?, index, codec)))
    };
    let (mut demuxed, index, codec) = tokio::select! {
        _ = cancel.cancelled() => return SessionEnd::Stop,
        result = tokio::time::timeout(STALL_TIMEOUT, connect) => match result {
            Err(_) => return SessionEnd::Retry("timed out connecting".into()),
            Ok(Err(e)) => return SessionEnd::Retry(e.to_string()),
            Ok(Ok(Err(end))) => return end,
            Ok(Ok(Ok(parts))) => parts,
        },
    };

    let mut sent_info: Option<StreamInfo> = None;
    let mut meter = Meter::new();
    loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => return SessionEnd::Stop,
            item = tokio::time::timeout(STALL_TIMEOUT, demuxed.next()) => item,
        };
        let frame = match item {
            Err(_) => return SessionEnd::Retry("no data for 10 s".into()),
            Ok(None) => return SessionEnd::Retry("camera ended the session".into()),
            Ok(Some(Err(e))) => return SessionEnd::Retry(e.to_string()),
            Ok(Some(Ok(CodecItem::VideoFrame(frame)))) if frame.stream_id() == index => frame,
            Ok(Some(Ok(_))) => continue,
        };
        if sent_info.is_none() || frame.has_new_parameters() {
            let Some(ParametersRef::Video(params)) = demuxed.streams()[index].parameters() else {
                continue; // cannot describe the stream yet
            };
            let (width, height) = params.pixel_dimensions();
            let info = StreamInfo {
                codec,
                width,
                height,
                decoder_config: Bytes::copy_from_slice(params.extra_data()),
            };
            if sent_info.as_ref() != Some(&info) {
                sent_info = Some(info.clone());
                if tx.send(StreamItem::Info(info)).await.is_err() {
                    return SessionEnd::Stop;
                }
            }
        }
        let ts = frame.timestamp();
        let ts_90k = ts.elapsed() * 90_000 / i64::from(ts.clock_rate().get());
        let is_keyframe = frame.is_random_access_point();
        let data = frame.into_data();
        meter.record(status, data.len());
        let unit = AccessUnit {
            received_at: Utc::now(),
            ts_90k,
            is_keyframe,
            avcc: Bytes::from(data),
        };
        if tx.send(StreamItem::Unit(unit)).await.is_err() {
            return SessionEnd::Stop;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::tests::fixture;
    use crate::http::tests::serve_once;

    #[test]
    fn flv_clock_handles_wraparound() {
        let mut clock = FlvClock::default();
        assert_eq!(clock.unwrap(u32::MAX - 100), (u32::MAX - 100) as i64);
        assert_eq!(clock.unwrap(99), u32::MAX as i64 + 100);
        assert_eq!(clock.unwrap(199), u32::MAX as i64 + 200);
    }

    #[test]
    fn credentials_are_moved_out_of_rtsp_urls() {
        let (url, creds) =
            split_credentials("rtsp://admin:p%40ss@10.0.0.2:554/h264Preview_01_sub").unwrap();
        assert_eq!(url.as_str(), "rtsp://10.0.0.2:554/h264Preview_01_sub");
        let creds = creds.unwrap();
        assert_eq!(
            (creds.username.as_str(), creds.password.as_str()),
            ("admin", "p@ss")
        );
        let (_, none) = split_credentials("rtsp://10.0.0.2/live").unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn flv_source_streams_the_fixture_then_reconnects() {
        let url = serve_once(fixture("testsrc_high_640x360_10fps.flv"), true);
        let (tx, mut rx) = mpsc::channel(256);
        let status = SharedStatus::default();
        let cancel = CancellationToken::new();
        let handle = spawn_flv_stream("test".into(), url, tx, status.clone(), cancel.clone());

        let first = rx.recv().await.unwrap();
        let StreamItem::Info(info) = first else {
            panic!("expected Info first, got {first:?}");
        };
        assert_eq!(
            (info.codec, info.width, info.height),
            (Codec::H264, 640, 360)
        );

        let mut units = Vec::new();
        while units.len() < 100 {
            match rx.recv().await.unwrap() {
                StreamItem::Unit(unit) => units.push(unit),
                StreamItem::Info(_) => panic!("unexpected second Info"),
            }
        }
        assert!(units[0].is_keyframe);
        assert_eq!(units.iter().filter(|u| u.is_keyframe).count(), 5);
        assert_eq!(units[1].ts_90k - units[0].ts_90k, 9000); // 100 ms at 90 kHz

        // The test server closes after one response, so the source must back off and retry.
        tokio::time::timeout(Duration::from_secs(5), async {
            while status.read().unwrap().reconnects == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("source should reconnect after the server closes");
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("source stops when cancelled")
            .unwrap();
    }

    /// Needs Docker. Run with `cargo test -p zoologist-video -- --ignored rtsp`.
    /// Starts MediaMTX, publishes the fixture over RTSP with ffmpeg, and reads it back.
    #[tokio::test]
    #[ignore]
    async fn rtsp_source_reads_a_fake_camera() {
        let url = std::env::var("ZOOLOGIST_TEST_RTSP")
            .unwrap_or_else(|_| "rtsp://127.0.0.1:8554/test".into());
        let (tx, mut rx) = mpsc::channel(256);
        let status = SharedStatus::default();
        let cancel = CancellationToken::new();
        let _handle = spawn_rtsp_stream("test".into(), url, tx, status.clone(), cancel.clone());
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            let mut info = None;
            let mut units = 0;
            while units < 40 {
                match rx.recv().await.unwrap() {
                    StreamItem::Info(i) => info = Some(i),
                    StreamItem::Unit(_) => units += 1,
                }
            }
            (info, units)
        })
        .await
        .expect("RTSP frames within 15 s (is scripts/fake-camera.sh running?)");
        let info = result.0.expect("stream info before frames");
        assert_eq!(
            (info.codec, info.width, info.height),
            (Codec::H264, 640, 360)
        );
        crate::h264::parse_avc_config(&info.decoder_config).expect("avcC from retina parses");
        cancel.cancel();
    }
}
