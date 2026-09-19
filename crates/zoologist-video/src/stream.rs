//! What every stream source (RTSP, HTTP-FLV, files) produces (plan Step 2.1).

use bytes::Bytes;
use chrono::{DateTime, Utc};

/// Video codec of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// Stream parameters. Sent before the first [`AccessUnit`] and again whenever they change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// For H.264, the `AVCDecoderConfigurationRecord` ("avcC"): SPS and PPS, as stored in MP4.
    pub decoder_config: Bytes,
}

/// One compressed video frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    /// Wall clock when the source received this frame.
    pub received_at: DateTime<Utc>,
    /// Presentation timestamp in 90 kHz units from the stream (RTP or FLV), used for durations.
    pub ts_90k: i64,
    pub is_keyframe: bool,
    /// NAL units, each prefixed with a 4-byte big-endian length ("AVCC" framing, as in MP4).
    pub avcc: Bytes,
}

/// Items sent by a stream source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamItem {
    Info(StreamInfo),
    Unit(AccessUnit),
}

/// Connection state of a stream source, shown in the health endpoint and UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum StreamState {
    #[default]
    Connecting,
    Streaming,
    /// Waiting before the next reconnect attempt.
    Backoff,
    /// The camera sends something we cannot use; the message says how to fix it.
    Unsupported(String),
    /// A file source played to its end.
    Ended,
}

/// Live statistics for one stream.
#[derive(Clone, Debug, Default)]
pub struct StreamStatus {
    pub state: StreamState,
    pub fps_measured: f32,
    pub last_unit_at: Option<DateTime<Utc>>,
    pub reconnects: u64,
    pub bitrate_kbps: f32,
    pub last_error: Option<String>,
}

/// Reconnect delays: 1, 2, 4, 8, 16, then 30 seconds for every later attempt.
pub fn backoff_seconds(attempt: u32) -> u64 {
    (1u64 << attempt.min(5)).min(30)
}

#[cfg(test)]
mod tests {
    use super::backoff_seconds;

    #[test]
    fn backoff_sequence() {
        let seq: Vec<u64> = (0..8).map(backoff_seconds).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
