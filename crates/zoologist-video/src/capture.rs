//! Saving and replaying raw camera streams (`zoologist capture`, plan Step 0.4).
//!
//! A capture named `x` is three files:
//! - `x.h264`: the H.264 stream in Annex-B framing (SPS/PPS before each keyframe), playable
//!   with `ffplay x.h264` and decodable by any tool, for comparing decoders;
//! - `x.units.jsonl`: one JSON line per access unit with its byte range in `x.h264`, arrival
//!   time and stream timestamp, so the stream can be replayed exactly;
//! - `x.info.json`: codec, size and the avcC decoder configuration (hex).

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::h264::{AvcConfig, annexb_nals, avcc_to_annexb, param_sets_annexb, parse_avc_config};
use crate::stream::{AccessUnit, Codec, StreamInfo, StreamItem};

/// One line of `x.units.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRecord {
    /// Byte offset of this unit's own NAL units in `x.h264` (after any parameter sets).
    pub offset: u64,
    pub len: u64,
    pub received_us: i64,
    pub ts_90k: i64,
    pub key: bool,
}

/// `x.info.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub decoder_config_hex: String,
}

fn paths(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let with = |ext: &str| {
        let mut p = base.as_os_str().to_owned();
        p.push(ext);
        PathBuf::from(p)
    };
    (with(".h264"), with(".units.jsonl"), with(".info.json"))
}

/// Writes a capture as items arrive. Units before the first `Info` or first keyframe are
/// skipped, so the file always starts decodable.
pub struct CaptureWriter {
    base: PathBuf,
    h264: BufWriter<File>,
    units: BufWriter<File>,
    config: Option<AvcConfig>,
    offset: u64,
    started: bool,
    buf: Vec<u8>,
    pub units_written: u64,
}

impl CaptureWriter {
    /// Creates the files `base.h264` and `base.units.jsonl`.
    pub fn create(base: &Path) -> std::io::Result<Self> {
        let (h264, units, _) = paths(base);
        if let Some(dir) = base.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Self {
            base: base.to_path_buf(),
            h264: BufWriter::new(File::create(h264)?),
            units: BufWriter::new(File::create(units)?),
            config: None,
            offset: 0,
            started: false,
            buf: Vec::new(),
            units_written: 0,
        })
    }

    /// Adds one stream item.
    pub fn push(&mut self, item: &StreamItem) -> std::io::Result<()> {
        match item {
            StreamItem::Info(info) => {
                if info.codec != Codec::H264 {
                    return Err(std::io::Error::other("only H.264 streams can be captured"));
                }
                let config =
                    parse_avc_config(&info.decoder_config).map_err(std::io::Error::other)?;
                let record = CaptureInfo {
                    codec: "h264".into(),
                    width: info.width,
                    height: info.height,
                    decoder_config_hex: hex(&info.decoder_config),
                };
                let (_, _, info_path) = paths(&self.base);
                std::fs::write(info_path, serde_json::to_vec_pretty(&record)?)?;
                self.config = Some(config);
            }
            StreamItem::Unit(unit) => {
                let Some(config) = &self.config else {
                    return Ok(());
                };
                if !self.started && !unit.is_keyframe {
                    return Ok(());
                }
                self.started = true;
                if unit.is_keyframe {
                    let params = param_sets_annexb(config);
                    self.h264.write_all(&params)?;
                    self.offset += params.len() as u64;
                }
                self.buf.clear();
                avcc_to_annexb(&unit.avcc, config.nal_length_size, &mut self.buf)
                    .map_err(std::io::Error::other)?;
                self.h264.write_all(&self.buf)?;
                let record = UnitRecord {
                    offset: self.offset,
                    len: self.buf.len() as u64,
                    received_us: unit.received_at.timestamp_micros(),
                    ts_90k: unit.ts_90k,
                    key: unit.is_keyframe,
                };
                serde_json::to_writer(&mut self.units, &record)?;
                self.units.write_all(b"\n")?;
                self.offset += self.buf.len() as u64;
                self.units_written += 1;
            }
        }
        Ok(())
    }

    /// Flushes both files.
    pub fn finish(mut self) -> std::io::Result<()> {
        self.h264.flush()?;
        self.units.flush()
    }
}

/// Reads a capture back as the stream items a live source would have sent.
pub fn read_capture(base: &Path) -> std::io::Result<Vec<StreamItem>> {
    let (h264_path, units_path, info_path) = paths(base);
    let info: CaptureInfo = serde_json::from_slice(&std::fs::read(info_path)?)?;
    let config = unhex(&info.decoder_config_hex)
        .ok_or_else(|| std::io::Error::other("bad decoder_config_hex"))?;
    let data = std::fs::read(h264_path)?;
    let mut items = vec![StreamItem::Info(StreamInfo {
        codec: Codec::H264,
        width: info.width,
        height: info.height,
        decoder_config: Bytes::from(config),
    })];
    for line in BufReader::new(File::open(units_path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: UnitRecord = serde_json::from_str(&line)?;
        let (start, end) = (
            record.offset as usize,
            (record.offset + record.len) as usize,
        );
        let annexb = data
            .get(start..end)
            .ok_or_else(|| std::io::Error::other("unit outside the .h264 file"))?;
        let mut avcc = Vec::with_capacity(annexb.len());
        for nal in annexb_nals(annexb) {
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }
        items.push(StreamItem::Unit(AccessUnit {
            received_at: DateTime::<Utc>::from_timestamp_micros(record.received_us)
                .unwrap_or_default(),
            ts_90k: record.ts_90k,
            is_keyframe: record.key,
            avcc: Bytes::from(avcc),
        }));
    }
    Ok(items)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::tests::fixture;
    use crate::h264::{NAL_IDR, NAL_PPS, NAL_SPS, build_avc_config, nal_type};

    #[test]
    fn capture_round_trips_and_is_plain_annexb() {
        // Build stream items from the fixture, as a source would.
        let data = fixture("testsrc_high_640x360_10fps.h264");
        let nals = annexb_nals(&data);
        let sps = nals.iter().find(|n| nal_type(n) == NAL_SPS).unwrap();
        let pps = nals.iter().find(|n| nal_type(n) == NAL_PPS).unwrap();
        let config = build_avc_config(sps, pps).unwrap();
        let mut items = vec![StreamItem::Info(StreamInfo {
            codec: Codec::H264,
            width: 640,
            height: 360,
            decoder_config: Bytes::from(config),
        })];
        for (i, nal) in nals
            .iter()
            .filter(|n| matches!(nal_type(n), 1 | 5))
            .enumerate()
        {
            let mut avcc = (nal.len() as u32).to_be_bytes().to_vec();
            avcc.extend_from_slice(nal);
            items.push(StreamItem::Unit(AccessUnit {
                received_at: DateTime::from_timestamp_micros(1_000_000 + i as i64 * 100_000)
                    .unwrap(),
                ts_90k: i as i64 * 9000,
                is_keyframe: nal_type(nal) == NAL_IDR,
                avcc: Bytes::from(avcc),
            }));
        }

        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cam-detect");
        let mut writer = CaptureWriter::create(&base).unwrap();
        for item in &items {
            writer.push(item).unwrap();
        }
        assert_eq!(writer.units_written, 100);
        writer.finish().unwrap();

        assert_eq!(read_capture(&base).unwrap(), items);
        let written = std::fs::read(dir.path().join("cam-detect.h264")).unwrap();
        let slices = annexb_nals(&written)
            .into_iter()
            .filter(|n| matches!(nal_type(n), 1 | 5))
            .count();
        assert_eq!(slices, 100);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(),
            vec![0, 1, 0xab, 0xff]
        );
        assert!(unhex("abc").is_none());
    }
}
