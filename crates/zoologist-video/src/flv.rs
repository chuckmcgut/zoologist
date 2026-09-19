//! Incremental FLV parser for Reolink HTTP-FLV streams (plan Step 2.2).
//!
//! FLV layout:
//! - Header: `"FLV"`, version `1`, flags, `u32` header size (normally 9), then `u32` PreviousTagSize0.
//! - Tags: `u8` type (8 audio, 9 video, 18 script), `u24` data size, `u24` timestamp + `u8`
//!   timestamp extension (upper 8 bits), `u24` stream id, data, then `u32` previous tag size.
//! - Video data: `frame_type (4 bits: 1 key, 2 inter) | codec_id (4 bits: 7 = AVC)`, then
//!   `u8 AVCPacketType` (0 = sequence header with the avcC, 1 = NAL units in AVCC framing,
//!   2 = end of sequence), then an `i24` composition time offset, then the payload.

use bytes::{Buf, Bytes, BytesMut};

/// Tags larger than this are treated as corrupt data rather than buffered forever.
const MAX_TAG_SIZE: usize = 8 * 1024 * 1024;

const TAG_VIDEO: u8 = 9;
const CODEC_AVC: u8 = 7;

/// Something found in the stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlvItem {
    /// AVC sequence header: the avcC decoder configuration record.
    AvcConfig(Bytes),
    /// One video frame.
    Video {
        /// Decode timestamp in milliseconds (32-bit, wraps after ~49 days).
        dts_ms: u32,
        /// Composition time offset in milliseconds (0 when the camera uses no B-frames).
        cts_ms: i32,
        keyframe: bool,
        /// NAL units in AVCC framing (4-byte lengths).
        avcc: Bytes,
    },
}

/// Errors that mean the stream cannot be used as it is.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FlvError {
    #[error("not an FLV stream (bad signature)")]
    BadSignature,
    #[error("unsupported video codec id {0} (only H.264 is supported; set the stream to H.264)")]
    UnsupportedCodec(u8),
    #[error("FLV tag of {0} bytes is too large")]
    TagTooLarge(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Header,
    Tags,
}

/// Feed it bytes as they arrive with [`FlvParser::push`]; it returns every complete item.
#[derive(Debug)]
pub struct FlvParser {
    buf: BytesMut,
    state: State,
}

impl Default for FlvParser {
    fn default() -> Self {
        Self::new()
    }
}

impl FlvParser {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
            state: State::Header,
        }
    }

    /// Adds `data` and returns the items it completed. Audio and script tags are skipped.
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<FlvItem>, FlvError> {
        self.buf.extend_from_slice(data);
        let mut items = Vec::new();
        loop {
            match self.state {
                State::Header => {
                    if self.buf.len() < 9 {
                        return Ok(items);
                    }
                    if &self.buf[..3] != b"FLV" {
                        return Err(FlvError::BadSignature);
                    }
                    let header_size =
                        u32::from_be_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]])
                            as usize;
                    // Header, then PreviousTagSize0.
                    let needed = header_size.max(9) + 4;
                    if self.buf.len() < needed {
                        return Ok(items);
                    }
                    self.buf.advance(needed);
                    self.state = State::Tags;
                }
                State::Tags => {
                    if self.buf.len() < 11 {
                        return Ok(items);
                    }
                    let tag_type = self.buf[0] & 0x1f;
                    let data_size = u24(&self.buf[1..4]) as usize;
                    if data_size > MAX_TAG_SIZE {
                        return Err(FlvError::TagTooLarge(data_size));
                    }
                    let total = 11 + data_size + 4;
                    if self.buf.len() < total {
                        return Ok(items);
                    }
                    let timestamp = u24(&self.buf[4..7]) | (u32::from(self.buf[7]) << 24);
                    let mut tag = self.buf.split_to(total).freeze();
                    tag.advance(11);
                    tag.truncate(data_size);
                    if tag_type == TAG_VIDEO
                        && let Some(item) = parse_video(tag, timestamp)?
                    {
                        items.push(item);
                    }
                }
            }
        }
    }
}

fn parse_video(mut data: Bytes, dts_ms: u32) -> Result<Option<FlvItem>, FlvError> {
    if data.len() < 5 {
        return Ok(None);
    }
    let frame_type = data[0] >> 4;
    let codec_id = data[0] & 0x0f;
    // Enhanced FLV (HEVC and others) sets the top bit of the first byte.
    if data[0] & 0x80 != 0 || codec_id != CODEC_AVC {
        return Err(FlvError::UnsupportedCodec(codec_id));
    }
    let packet_type = data[1];
    let cts = u24(&data[2..5]);
    // Sign-extend the 24-bit composition time.
    let cts_ms = ((cts << 8) as i32) >> 8;
    data.advance(5);
    Ok(match packet_type {
        0 => Some(FlvItem::AvcConfig(data)),
        1 if !data.is_empty() => Some(FlvItem::Video {
            dts_ms,
            cts_ms,
            keyframe: frame_type == 1,
            avcc: data,
        }),
        _ => None,
    })
}

fn u24(b: &[u8]) -> u32 {
    (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds FLV bytes: header plus the given `(tag_type, timestamp_ms, data)` tags.
    pub(crate) fn flv(tags: &[(u8, u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        for (tag_type, ts, data) in tags {
            let size = data.len() as u32;
            out.push(*tag_type);
            out.extend_from_slice(&size.to_be_bytes()[1..]);
            out.extend_from_slice(&ts.to_be_bytes()[1..]);
            out.push((ts >> 24) as u8);
            out.extend_from_slice(&[0, 0, 0]);
            out.extend_from_slice(data);
            out.extend_from_slice(&(size + 11).to_be_bytes());
        }
        out
    }

    pub(crate) fn video(keyframe: bool, packet_type: u8, cts: i32, payload: &[u8]) -> Vec<u8> {
        let first = if keyframe { 0x17 } else { 0x27 };
        let mut v = vec![first, packet_type];
        v.extend_from_slice(&cts.to_be_bytes()[1..]);
        v.extend_from_slice(payload);
        v
    }

    fn sample_stream() -> Vec<u8> {
        flv(&[
            (18, 0, b"onMetaData".to_vec()),
            (TAG_VIDEO, 0, video(true, 0, 0, &[1, 0x64, 0, 0x1e, 0xff])),
            (8, 0, vec![0xaf, 1, 2, 3]),
            (TAG_VIDEO, 0, video(true, 1, 0, &[0, 0, 0, 2, 0x65, 0x88])),
            (TAG_VIDEO, 200, video(false, 1, -40, &[0, 0, 0, 1, 0x41])),
            // Extended timestamp: 0x01_000010 ms.
            (
                TAG_VIDEO,
                0x0100_0010,
                video(false, 1, 0, &[0, 0, 0, 1, 0x41]),
            ),
            (TAG_VIDEO, 0, video(false, 2, 0, &[])),
        ])
    }

    fn expected() -> Vec<FlvItem> {
        vec![
            FlvItem::AvcConfig(Bytes::from_static(&[1, 0x64, 0, 0x1e, 0xff])),
            FlvItem::Video {
                dts_ms: 0,
                cts_ms: 0,
                keyframe: true,
                avcc: Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]),
            },
            FlvItem::Video {
                dts_ms: 200,
                cts_ms: -40,
                keyframe: false,
                avcc: Bytes::from_static(&[0, 0, 0, 1, 0x41]),
            },
            FlvItem::Video {
                dts_ms: 0x0100_0010,
                cts_ms: 0,
                keyframe: false,
                avcc: Bytes::from_static(&[0, 0, 0, 1, 0x41]),
            },
        ]
    }

    #[test]
    fn parses_a_whole_stream_and_skips_audio_and_script() {
        let mut p = FlvParser::new();
        assert_eq!(p.push(&sample_stream()).unwrap(), expected());
    }

    #[test]
    fn same_result_when_split_at_every_byte_offset() {
        let stream = sample_stream();
        for split in 0..=stream.len() {
            let mut p = FlvParser::new();
            let mut items = p.push(&stream[..split]).unwrap();
            items.extend(p.push(&stream[split..]).unwrap());
            assert_eq!(items, expected(), "split at {split}");
        }
    }

    #[test]
    fn one_byte_at_a_time() {
        let mut p = FlvParser::new();
        let mut items = Vec::new();
        for b in sample_stream() {
            items.extend(p.push(&[b]).unwrap());
        }
        assert_eq!(items, expected());
    }

    #[test]
    fn rejects_non_flv_and_non_h264() {
        assert_eq!(
            FlvParser::new().push(b"RIFF1234567890").unwrap_err(),
            FlvError::BadSignature
        );
        let hevc_legacy = flv(&[(TAG_VIDEO, 0, vec![0x1c, 1, 0, 0, 0, 1])]);
        assert_eq!(
            FlvParser::new().push(&hevc_legacy).unwrap_err(),
            FlvError::UnsupportedCodec(12)
        );
        let enhanced = flv(&[(TAG_VIDEO, 0, vec![0x90, b'h', b'v', b'c', b'1'])]);
        assert!(FlvParser::new().push(&enhanced).is_err());
    }

    #[test]
    fn parses_an_ffmpeg_made_flv_file() {
        let data = crate::h264::tests::fixture("testsrc_high_640x360_10fps.flv");
        let items = FlvParser::new().push(&data).unwrap();
        let FlvItem::AvcConfig(config) = &items[0] else {
            panic!("first item should be the AVC config: {:?}", items[0]);
        };
        let config = crate::h264::parse_avc_config(config).unwrap();
        assert_eq!((config.width, config.height), (640, 360));
        let frames: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                FlvItem::Video {
                    dts_ms,
                    keyframe,
                    avcc,
                    ..
                } => Some((*dts_ms, *keyframe, avcc)),
                _ => None,
            })
            .collect();
        assert_eq!(frames.len(), 100);
        assert_eq!(frames.iter().filter(|f| f.1).count(), 5);
        assert_eq!(frames[1].0 - frames[0].0, 100); // 10 fps
        for (_, _, avcc) in &frames {
            assert!(crate::h264::avcc_nals(avcc, 4).all(|n| n.is_ok()));
        }
    }

    #[test]
    fn rejects_absurd_tag_sizes() {
        let mut bytes = flv(&[]);
        bytes.extend_from_slice(&[TAG_VIDEO, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0]);
        assert!(matches!(
            FlvParser::new().push(&bytes),
            Err(FlvError::TagTooLarge(_))
        ));
    }
}
