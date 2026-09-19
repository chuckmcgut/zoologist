//! Playing a recorded stream from disk as if it were a camera (tests and `--fast-files`).
//!
//! `file://<path>` accepts either a `zoologist capture` base name (`<path>.h264` with
//! `<path>.units.jsonl` and `<path>.info.json`) or a raw Annex-B `.h264` file. Raw files are
//! played at `?fps=N` (default 10). With `fast`, frames are sent as fast as the receiver takes
//! them, with timestamps that still advance at the stream's own rate.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::capture::read_capture;
use crate::h264::{
    NAL_IDR, NAL_PPS, NAL_SPS, annexb_nals, build_avc_config, nal_type, parse_avc_config,
};
use crate::source::SharedStatus;
use crate::stream::{AccessUnit, Codec, StreamInfo, StreamItem, StreamState};

/// Parses `file:///path?fps=10` into the path and frame rate.
fn parse(url: &str) -> Result<(PathBuf, u32), String> {
    let rest = url.strip_prefix("file://").ok_or("not a file:// URL")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let fps = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("fps="))
        .map(|v| v.parse::<u32>().map_err(|_| format!("bad fps {v:?}")))
        .transpose()?
        .unwrap_or(10)
        .max(1);
    Ok((PathBuf::from(path), fps))
}

/// Loads the stream items of a file source, timed from `start`.
pub fn load_file_items(url: &str, start: DateTime<Utc>) -> Result<Vec<StreamItem>, String> {
    let (path, fps) = parse(url)?;
    let mut units_path = path.as_os_str().to_owned();
    units_path.push(".units.jsonl");
    if Path::new(&units_path).exists() {
        // A capture: keep its own timing, shifted to start at `start`.
        let mut items = read_capture(&path).map_err(|e| e.to_string())?;
        let first = items.iter().find_map(|i| match i {
            StreamItem::Unit(u) => Some(u.received_at),
            StreamItem::Info(_) => None,
        });
        if let Some(first) = first {
            for item in &mut items {
                if let StreamItem::Unit(u) = item {
                    u.received_at = start + (u.received_at - first);
                }
            }
        }
        return Ok(items);
    }
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    raw_items(&data, fps, start)
}

/// Splits a raw Annex-B stream into one access unit per slice NAL (encoder output with one slice
/// per picture, as ffmpeg and cameras produce).
fn raw_items(data: &[u8], fps: u32, start: DateTime<Utc>) -> Result<Vec<StreamItem>, String> {
    let nals = annexb_nals(data);
    let sps = nals
        .iter()
        .find(|n| nal_type(n) == NAL_SPS)
        .ok_or("no SPS in file")?;
    let pps = nals
        .iter()
        .find(|n| nal_type(n) == NAL_PPS)
        .ok_or("no PPS in file")?;
    let config = build_avc_config(sps, pps).map_err(|e| e.to_string())?;
    let parsed = parse_avc_config(&config).map_err(|e| e.to_string())?;
    let mut items = vec![StreamItem::Info(StreamInfo {
        codec: Codec::H264,
        width: parsed.width,
        height: parsed.height,
        decoder_config: Bytes::from(config),
    })];
    let frame_us = 1_000_000 / i64::from(fps);
    for (n, nal) in (0i64..).zip(nals.iter().filter(|n| matches!(nal_type(n), 1 | 5))) {
        let mut avcc = (nal.len() as u32).to_be_bytes().to_vec();
        avcc.extend_from_slice(nal);
        items.push(StreamItem::Unit(AccessUnit {
            received_at: start + chrono::Duration::microseconds(n * frame_us),
            ts_90k: n * 90_000 / i64::from(fps),
            is_keyframe: nal_type(nal) == NAL_IDR,
            avcc: Bytes::from(avcc),
        }));
    }
    Ok(items)
}

/// Plays a file source once. In real-time mode units are sent at their timestamps; in `fast`
/// mode as fast as the receiver takes them.
pub fn spawn_file_stream(
    url: String,
    start: DateTime<Utc>,
    fast: bool,
    tx: mpsc::Sender<StreamItem>,
    status: SharedStatus,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let items = match load_file_items(&url, start) {
            Ok(items) => items,
            Err(e) => {
                let mut s = status.write().unwrap_or_else(|e| e.into_inner());
                s.state = StreamState::Unsupported(e.clone());
                s.last_error = Some(e);
                return;
            }
        };
        status.write().unwrap_or_else(|e| e.into_inner()).state = StreamState::Streaming;
        let wall_start = tokio::time::Instant::now();
        for item in items {
            if let (false, StreamItem::Unit(u)) = (fast, &item) {
                let due = (u.received_at - start).to_std().unwrap_or(Duration::ZERO);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep_until(wall_start + due) => {}
                }
            }
            if let StreamItem::Unit(u) = &item {
                status
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_unit_at = Some(u.received_at);
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                sent = tx.send(item) => if sent.is_err() { return },
            }
        }
        status.write().unwrap_or_else(|e| e.into_inner()).state = StreamState::Ended;
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn fixture_url(name: &str, query: &str) -> String {
        format!(
            "file://{}/../../tools/fixtures/{name}{query}",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    #[test]
    fn raw_file_becomes_timed_units() {
        let start = Utc.with_ymd_and_hms(2026, 9, 18, 12, 0, 0).unwrap();
        let items = load_file_items(
            &fixture_url("testsrc_main_640x360_10fps.h264", "?fps=5"),
            start,
        )
        .unwrap();
        assert!(matches!(&items[0], StreamItem::Info(i) if i.width == 640));
        assert_eq!(items.len(), 101);
        let StreamItem::Unit(u) = &items[2] else {
            panic!()
        };
        assert_eq!(u.received_at, start + chrono::Duration::milliseconds(200));
        assert!(load_file_items("file:///nope.h264", start).is_err());
        assert!(parse("file:///x.h264?fps=abc").is_err());
    }

    #[tokio::test]
    async fn fast_file_stream_sends_everything() {
        let (tx, mut rx) = mpsc::channel(8);
        let status = SharedStatus::default();
        spawn_file_stream(
            fixture_url("testsrc_high_640x360_10fps.h264", ""),
            Utc::now(),
            true,
            tx,
            status.clone(),
            CancellationToken::new(),
        );
        let mut n = 0;
        while rx.recv().await.is_some() {
            n += 1;
        }
        assert_eq!(n, 101);
    }
}
