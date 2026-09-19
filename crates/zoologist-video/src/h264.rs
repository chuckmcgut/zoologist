//! H.264 helpers: reading the decoder configuration and converting between NAL framings.
//!
//! MP4, FLV and retina use "AVCC" framing (each NAL unit prefixed with its length). Most decoders
//! want "Annex-B" framing (each NAL unit prefixed with a `00 00 00 01` start code).

use h264_reader::avcc::AvcDecoderConfigurationRecord;
use h264_reader::nal::sps::SeqParameterSet;
use h264_reader::nal::{Nal, RefNal};

/// Errors reading H.264 data.
#[derive(Debug, thiserror::Error)]
pub enum H264Error {
    #[error("invalid avcC decoder configuration: {0}")]
    Config(String),
    #[error("invalid AVCC data: {0}")]
    Framing(String),
}

/// What we need from an `AVCDecoderConfigurationRecord`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcConfig {
    pub width: u32,
    pub height: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    /// Bytes per NAL length prefix (1, 2 or 4).
    pub nal_length_size: usize,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

/// Parses an avcC record and the first SPS inside it.
pub fn parse_avc_config(avcc: &[u8]) -> Result<AvcConfig, H264Error> {
    let record = AvcDecoderConfigurationRecord::try_from(avcc)
        .map_err(|e| H264Error::Config(format!("{e:?}")))?;
    let sps: Vec<Vec<u8>> = record
        .sequence_parameter_sets()
        .map(|s| s.map(<[u8]>::to_vec).map_err(|e| format!("{e:?}")))
        .collect::<Result<_, _>>()
        .map_err(H264Error::Config)?;
    let pps: Vec<Vec<u8>> = record
        .picture_parameter_sets()
        .map(|p| p.map(<[u8]>::to_vec).map_err(|e| format!("{e:?}")))
        .collect::<Result<_, _>>()
        .map_err(H264Error::Config)?;
    let first = sps
        .first()
        .ok_or_else(|| H264Error::Config("no SPS in avcC".into()))?;
    let parsed = SeqParameterSet::from_bits(RefNal::new(first, &[], true).rbsp_bits())
        .map_err(|e| H264Error::Config(format!("SPS: {e:?}")))?;
    let (width, height) = parsed
        .pixel_dimensions()
        .map_err(|e| H264Error::Config(format!("SPS dimensions: {e:?}")))?;
    Ok(AvcConfig {
        width,
        height,
        profile_idc: parsed.profile_idc.into(),
        level_idc: parsed.level_idc,
        nal_length_size: record.length_size_minus_one() as usize + 1,
        sps,
        pps,
    })
}

/// Builds an avcC record from one SPS and one PPS (both without start codes). Used when a source
/// delivers parameter sets in-band instead of as a decoder configuration.
pub fn build_avc_config(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>, H264Error> {
    if sps.len() < 4 {
        return Err(H264Error::Config("SPS too short".into()));
    }
    let mut out = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Ok(out)
}

/// Iterates over the NAL units of AVCC-framed data with `length_size`-byte length prefixes.
pub fn avcc_nals(
    data: &[u8],
    length_size: usize,
) -> impl Iterator<Item = Result<&[u8], H264Error>> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        if rest.len() < length_size {
            rest = &[];
            return Some(Err(H264Error::Framing("truncated length prefix".into())));
        }
        let len = rest[..length_size]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | b as usize);
        let body = &rest[length_size..];
        if body.len() < len {
            rest = &[];
            return Some(Err(H264Error::Framing(format!(
                "NAL of {len} bytes but only {} left",
                body.len()
            ))));
        }
        let (nal, tail) = body.split_at(len);
        rest = tail;
        Some(Ok(nal))
    })
}

/// Appends AVCC-framed NAL units to `out` in Annex-B framing.
pub fn avcc_to_annexb(avcc: &[u8], length_size: usize, out: &mut Vec<u8>) -> Result<(), H264Error> {
    for nal in avcc_nals(avcc, length_size) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal?);
    }
    Ok(())
}

/// The SPS and PPS of a configuration in Annex-B framing, to put before each keyframe.
pub fn param_sets_annexb(config: &AvcConfig) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in config.sps.iter().chain(&config.pps) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// Splits Annex-B data into NAL units (without start codes). Handles 3- and 4-byte start codes.
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = match starts.get(n + 1) {
                // Next start code begins 3 bytes before its payload, plus a leading zero if it
                // was a 4-byte start code.
                Some(&next) => {
                    let mut end = next - 3;
                    while end > start && data[end - 1] == 0 {
                        end -= 1;
                    }
                    end
                }
                None => data.len(),
            };
            &data[start..end]
        })
        .collect()
}

/// NAL unit type (low 5 bits of the first byte).
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

/// NAL type of an IDR slice (a keyframe).
pub const NAL_IDR: u8 = 5;
/// NAL type of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// NAL type of a picture parameter set.
pub const NAL_PPS: u8 = 8;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Reads the committed synthetic fixture and returns its NAL units.
    pub(crate) fn fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/../../tools/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn fixture_config(name: &str) -> AvcConfig {
        let data = fixture(name);
        let nals = annexb_nals(&data);
        let sps = nals.iter().find(|n| nal_type(n) == NAL_SPS).unwrap();
        let pps = nals.iter().find(|n| nal_type(n) == NAL_PPS).unwrap();
        parse_avc_config(&build_avc_config(sps, pps).unwrap()).unwrap()
    }

    #[test]
    fn reads_dimensions_and_profile_from_fixtures() {
        let main = fixture_config("testsrc_main_640x360_10fps.h264");
        assert_eq!((main.width, main.height, main.profile_idc), (640, 360, 77));
        assert_eq!(main.nal_length_size, 4);
        let high = fixture_config("testsrc_high_640x360_10fps.h264");
        assert_eq!((high.width, high.height, high.profile_idc), (640, 360, 100));
    }

    #[test]
    fn annexb_split_finds_every_frame_of_the_fixture() {
        let data = fixture("testsrc_main_640x360_10fps.h264");
        let nals = annexb_nals(&data);
        let idr = nals.iter().filter(|n| nal_type(n) == NAL_IDR).count();
        let slices = nals.iter().filter(|n| matches!(nal_type(n), 1 | 5)).count();
        assert_eq!(slices, 100);
        assert_eq!(idr, 5); // keyframe every 20 frames
    }

    #[test]
    fn avcc_and_annexb_round_trip() {
        let nals: [&[u8]; 3] = [&[0x67, 1, 2], &[0x68, 3], &[0x65, 4, 5, 6, 0, 0]];
        let mut avcc = Vec::new();
        for nal in nals {
            avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            avcc.extend_from_slice(nal);
        }
        let mut annexb = Vec::new();
        avcc_to_annexb(&avcc, 4, &mut annexb).unwrap();
        assert_eq!(annexb_nals(&annexb), nals.to_vec());
    }

    #[test]
    fn truncated_avcc_is_an_error() {
        let bad = [0, 0, 0, 9, 1, 2];
        assert!(avcc_nals(&bad, 4).any(|r| r.is_err()));
        assert!(avcc_nals(&[0, 0], 4).any(|r| r.is_err()));
    }

    #[test]
    fn bad_config_is_an_error() {
        assert!(parse_avc_config(&[1, 2, 3]).is_err());
        assert!(build_avc_config(&[0x67], &[0x68]).is_err());
    }
}
