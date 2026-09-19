//! Live view: each camera's stream, as Zoologist already receives it, re-sent to browsers as
//! fragmented MP4 (no decoding, no re-encoding, no extra connection to the camera).
//!
//! A [`LiveFeed`] sits between a camera's source and its recorder (or decoder). It keeps the
//! stream parameters and the current group of pictures, so a new viewer starts at a keyframe
//! at once, and broadcasts every access unit to the viewers.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use futures_util::Stream;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use zoologist_video::mp4w::{Sample, fragment, fragmented_init};
use zoologist_video::stream::{AccessUnit, StreamInfo, StreamItem};

/// Units kept for a new viewer when the camera sends no keyframe for a long time.
const MAX_GOP: usize = 600;
/// Units buffered per viewer before a slow viewer skips ahead to the next keyframe.
const VIEWER_BUFFER: usize = 256;
/// Frame duration used before two frames have been seen (25 fps).
const DEFAULT_DURATION: u32 = 3600;

#[derive(Default)]
struct Cache {
    info: Option<StreamInfo>,
    /// Units since (and including) the last keyframe.
    gop: Vec<AccessUnit>,
}

/// One camera's live stream.
pub struct LiveFeed {
    tx: broadcast::Sender<Arc<StreamItem>>,
    cache: Mutex<Cache>,
}

impl Default for LiveFeed {
    fn default() -> Self {
        LiveFeed {
            tx: broadcast::channel(VIEWER_BUFFER).0,
            cache: Mutex::default(),
        }
    }
}

impl LiveFeed {
    /// Records `item` for new viewers and sends it to current ones.
    pub fn push(&self, item: &StreamItem) {
        {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            match item {
                StreamItem::Info(info) => {
                    cache.info = Some(info.clone());
                    cache.gop.clear();
                }
                StreamItem::Unit(unit) => {
                    if unit.is_keyframe {
                        cache.gop.clear();
                    }
                    if !cache.gop.is_empty() || unit.is_keyframe {
                        cache.gop.push(unit.clone());
                    }
                    if cache.gop.len() > MAX_GOP {
                        cache.gop.clear();
                    }
                }
            }
        }
        if self.tx.receiver_count() > 0 {
            let _ = self.tx.send(Arc::new(item.clone()));
        }
    }

    /// The stream parameters, the units of the current group of pictures, and a receiver for
    /// what follows. `None` until the camera has sent its parameters.
    pub fn subscribe(
        &self,
    ) -> Option<(
        StreamInfo,
        Vec<AccessUnit>,
        broadcast::Receiver<Arc<StreamItem>>,
    )> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        Some((cache.info.clone()?, cache.gop.clone(), rx))
    }

    /// Viewers watching now.
    pub fn viewers(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// Forwards `input` to `output`, showing every item to `feed` on the way. Ends when either side
/// closes.
pub fn tee(
    mut input: mpsc::Receiver<StreamItem>,
    output: mpsc::Sender<StreamItem>,
    feed: Arc<LiveFeed>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(item) = input.recv().await {
            feed.push(&item);
            if output.send(item).await.is_err() {
                return;
            }
        }
    })
}

/// Turns access units into MP4 fragments on a gap-free timeline.
///
/// Many cameras stamp frames when they send them, not when they captured them, so their
/// timestamps jitter (the owner's cameras: from 0.2 ms to 280 ms apart at 20–25 fps). In a
/// browser's MediaSource a jump in decode time is a discontinuity, and every frame after it is
/// dropped until the next keyframe. So each fragment starts exactly where the previous one
/// ended, and all frames get the camera's average frame duration (a running average of its
/// timestamps), which keeps the live view at the camera's real rate.
struct Fragmenter {
    sequence: u32,
    last_ts: Option<i64>,
    /// Running average frame duration, 90 kHz units.
    average: f64,
    next_decode_time: u64,
}

/// Weight of each new frame interval in the running average.
const AVERAGE_WEIGHT: f64 = 0.05;

impl Fragmenter {
    fn new() -> Self {
        Fragmenter {
            sequence: 0,
            last_ts: None,
            average: f64::from(DEFAULT_DURATION),
            next_decode_time: 0,
        }
    }

    fn fragment(&mut self, unit: &AccessUnit) -> Bytes {
        if let Some(last) = self.last_ts {
            let delta = unit.ts_90k - last;
            // Ignore resets and pauses (reconnects, dropped seconds).
            if (1..=45_000).contains(&delta) {
                self.average += AVERAGE_WEIGHT * (delta as f64 - self.average);
            }
        }
        self.last_ts = Some(unit.ts_90k);
        let duration = self.average.round().clamp(1.0, 45_000.0) as u32;
        let decode_time = self.next_decode_time;
        self.next_decode_time += u64::from(duration);
        self.sequence += 1;
        let sample = Sample {
            data: unit.avcc.clone(),
            duration_90k: duration,
            is_key: unit.is_keyframe,
            wall_us: 0,
        };
        Bytes::from(fragment(self.sequence, decode_time, &sample))
    }
}

/// The byte stream for one viewer: the MP4 header, the current group of pictures, then live
/// fragments. A viewer that falls behind skips to the next keyframe; a parameter change or the
/// feed closing ends the stream (the browser reconnects).
pub fn viewer_stream(
    info: StreamInfo,
    gop: Vec<AccessUnit>,
    rx: broadcast::Receiver<Arc<StreamItem>>,
) -> std::io::Result<impl Stream<Item = Result<Bytes, std::io::Error>>> {
    let mut first: VecDeque<Bytes> = VecDeque::new();
    first.push_back(Bytes::from(fragmented_init(&info)?));
    let mut frag = Fragmenter::new();
    let need_key = gop.is_empty();
    for unit in &gop {
        first.push_back(frag.fragment(unit));
    }
    struct State {
        pending: VecDeque<Bytes>,
        rx: broadcast::Receiver<Arc<StreamItem>>,
        frag: Fragmenter,
        need_key: bool,
    }
    let state = State {
        pending: first,
        rx,
        frag,
        need_key,
    };
    Ok(futures_util::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(bytes) = s.pending.pop_front() {
                return Some((Ok(bytes), s));
            }
            match s.rx.recv().await {
                Ok(item) => match item.as_ref() {
                    StreamItem::Info(_) => return None,
                    StreamItem::Unit(unit) => {
                        if s.need_key && !unit.is_keyframe {
                            continue;
                        }
                        s.need_key = false;
                        let bytes = s.frag.fragment(unit);
                        return Some((Ok(bytes), s));
                    }
                },
                Err(broadcast::error::RecvError::Lagged(_)) => s.need_key = true,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use futures_util::StreamExt;
    use zoologist_video::stream::Codec;

    use super::*;

    fn info() -> StreamInfo {
        StreamInfo {
            codec: Codec::H264,
            width: 640,
            height: 360,
            decoder_config: Bytes::from_static(&[1, 0x64, 0, 0x1f, 0xff, 0xe0, 0]),
        }
    }

    fn unit(ts: i64, key: bool) -> StreamItem {
        StreamItem::Unit(AccessUnit {
            received_at: Utc::now(),
            ts_90k: ts,
            is_keyframe: key,
            avcc: Bytes::from(vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }]),
        })
    }

    /// Decode time and duration of a fragment made by [`fragment`].
    fn timing(f: &[u8]) -> (u64, u32) {
        // moof(8) mfhd(16) traf(8) tfhd(16), then tfdt (header 8 + version/flags 4 + u64) and
        // trun (header 8 + version/flags 4 + count 4 + data offset 4, then duration).
        let tfdt = 8 + 16 + 8 + 16;
        let decode = u64::from_be_bytes(f[tfdt + 12..tfdt + 20].try_into().unwrap());
        let trun = tfdt + 20;
        let duration = u32::from_be_bytes(f[trun + 20..trun + 24].try_into().unwrap());
        (decode, duration)
    }

    #[test]
    fn jittery_camera_timestamps_give_a_gap_free_timeline() {
        let mut frag = Fragmenter::new();
        // 25 fps on average, stamped at send time: bursts and pauses.
        let stamps = [
            0, 54, 83, 99, 116, 7107, 10680, 14276, 17964, 21471, 25000, 28600,
        ];
        let mut prev: Option<(u64, u32)> = None;
        for (i, ts) in stamps.iter().enumerate() {
            let StreamItem::Unit(u) = unit(*ts, i == 0) else {
                unreachable!()
            };
            let (decode, duration) = timing(&frag.fragment(&u));
            if let Some((d, dur)) = prev {
                assert_eq!(
                    decode,
                    d + u64::from(dur),
                    "frame {i} must start where {} ended",
                    i - 1
                );
            }
            assert!(
                (1000..=6000).contains(&duration),
                "duration {duration} stays near 40 ms"
            );
            prev = Some((decode, duration));
        }
    }

    #[tokio::test]
    async fn a_new_viewer_starts_at_the_last_keyframe_then_goes_live() {
        let feed = LiveFeed::default();
        assert!(feed.subscribe().is_none(), "nothing before stream info");
        feed.push(&StreamItem::Info(info()));
        feed.push(&unit(0, false)); // before any keyframe: not kept
        feed.push(&unit(3600, true));
        feed.push(&unit(7200, false));
        let (info, gop, rx) = feed.subscribe().unwrap();
        let new_info = info.clone();
        assert_eq!(gop.len(), 2);
        assert!(gop[0].is_keyframe);
        let mut stream = Box::pin(viewer_stream(info, gop, rx).unwrap());
        let init = stream.next().await.unwrap().unwrap();
        assert_eq!(&init[4..8], b"ftyp");
        for _ in 0..2 {
            let f = stream.next().await.unwrap().unwrap();
            assert_eq!(&f[4..8], b"moof");
        }
        assert_eq!(feed.viewers(), 1);
        feed.push(&unit(10800, false));
        let live = stream.next().await.unwrap().unwrap();
        assert_eq!(&live[4..8], b"moof");
        // New stream parameters end the viewer's stream; the browser reconnects.
        feed.push(&StreamItem::Info(new_info));
        assert!(stream.next().await.is_none());
    }
}
