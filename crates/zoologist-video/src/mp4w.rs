//! Writing MP4 files from compressed video samples, without decoding (plan Step 6.2).
//!
//! Files are "faststart" (the `moov` index comes before the `mdat` data) so browsers can start
//! playing clips before the whole file has downloaded. One video track, no audio.
//! Box layouts follow ISO/IEC 14496-12 (ISO base media) and 14496-15 (AVC/HEVC in MP4); retina's
//! example MP4 writer (MIT/Apache-2.0) was a useful reference.

use std::io::{self, Write};

use bytes::Bytes;

use crate::stream::{Codec, StreamInfo};

/// Timescale of the video track: 90 kHz, like RTP.
pub const TIMESCALE: u32 = 90_000;

/// One compressed frame to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// NAL units with 4-byte length prefixes (as in the stream's decoder configuration).
    pub data: Bytes,
    pub duration_90k: u32,
    pub is_key: bool,
    /// Wall-clock time of the frame in microseconds since the Unix epoch.
    pub wall_us: i64,
}

/// Where a sample ended up in a written file. Saved as the `.idx` next to each segment so
/// clips can be cut later without parsing the MP4.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SampleIndexEntry {
    pub offset: u64,
    pub size: u32,
    pub duration_90k: u32,
    pub is_key: bool,
    pub wall_us: i64,
}

/// Writes `samples` as a faststart MP4 to `out`. The first sample must be a keyframe.
/// Returns where each sample was written.
pub fn write_mp4<W: Write>(
    out: W,
    info: &StreamInfo,
    samples: &[Sample],
) -> io::Result<Vec<SampleIndexEntry>> {
    let layout: Vec<SampleIndexEntry> = samples
        .iter()
        .map(|s| SampleIndexEntry {
            offset: 0,
            size: s.data.len() as u32,
            duration_90k: s.duration_90k,
            is_key: s.is_key,
            wall_us: s.wall_us,
        })
        .collect();
    write_mp4_streamed(out, info, &layout, |out| {
        samples.iter().try_for_each(|s| out.write_all(&s.data))
    })
}

/// Writes a faststart MP4 whose samples are described by `layout` (sizes, durations,
/// keyframes; the offsets are ignored). `write_data` must then write exactly those samples'
/// bytes, in order. The samples never have to be in memory together, so a long clip costs no
/// more memory than a short one. Returns where each sample was written.
pub fn write_mp4_streamed<W: Write>(
    mut out: W,
    info: &StreamInfo,
    layout: &[SampleIndexEntry],
    write_data: impl FnOnce(&mut CountingWriter<&mut W>) -> io::Result<()>,
) -> io::Result<Vec<SampleIndexEntry>> {
    if layout.first().is_none_or(|s| !s.is_key) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an MP4 must start with a keyframe",
        ));
    }
    let data_len: u64 = layout.iter().map(|s| u64::from(s.size)).sum();
    let ftyp = ftyp(info.codec);
    // The mdat header is 16 bytes when the payload needs a 64-bit size.
    let mdat_header: u64 = if data_len + 8 > u32::MAX as u64 {
        16
    } else {
        8
    };

    // Build moov once to learn its size (co64 entries have a fixed size), then again with the
    // real sample offsets.
    let placeholder = moov(info, layout, 0)?;
    let first_offset = ftyp.len() as u64 + placeholder.len() as u64 + mdat_header;
    let moov = moov(info, layout, first_offset)?;
    debug_assert_eq!(moov.len(), placeholder.len());

    out.write_all(&ftyp)?;
    out.write_all(&moov)?;
    if mdat_header == 16 {
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&(data_len + 16).to_be_bytes())?;
    } else {
        out.write_all(&((data_len + 8) as u32).to_be_bytes())?;
        out.write_all(b"mdat")?;
    }
    let mut counting = CountingWriter {
        inner: &mut out,
        written: 0,
    };
    write_data(&mut counting)?;
    if counting.written != data_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "wrote {} bytes of samples, the layout says {data_len}",
                counting.written
            ),
        ));
    }
    out.flush()?;
    let mut offset = first_offset;
    Ok(layout
        .iter()
        .map(|s| {
            let entry = SampleIndexEntry {
                offset,
                ..s.clone()
            };
            offset += u64::from(s.size);
            entry
        })
        .collect())
}

/// A writer that counts the bytes written through it.
pub struct CountingWriter<W> {
    inner: W,
    written: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A box: 32-bit size, four-character type, payload.
pub(crate) fn bx(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    out
}

/// A "full box": a box whose payload starts with a version byte and 24-bit flags.
fn full_box(fourcc: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + 4);
    body.push(version);
    body.extend_from_slice(&flags.to_be_bytes()[1..]);
    body.extend_from_slice(payload);
    bx(fourcc, &body)
}

fn ftyp(codec: Codec) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"isom");
    p.extend_from_slice(&0x200u32.to_be_bytes());
    p.extend_from_slice(b"isom");
    p.extend_from_slice(b"iso2");
    p.extend_from_slice(match codec {
        Codec::H264 => b"avc1",
        Codec::H265 => b"hvc1",
    });
    p.extend_from_slice(b"mp41");
    bx(b"ftyp", &p)
}

/// The identity transformation matrix used by `mvhd` and `tkhd`.
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn push_matrix(p: &mut Vec<u8>) {
    for v in MATRIX {
        p.extend_from_slice(&v.to_be_bytes());
    }
}

fn moov(info: &StreamInfo, samples: &[SampleIndexEntry], first_offset: u64) -> io::Result<Vec<u8>> {
    let duration: u64 = samples.iter().map(|s| u64::from(s.duration_90k)).sum();
    let duration32 = duration.min(u32::MAX as u64) as u32;

    let mut mvhd = Vec::new();
    mvhd.extend_from_slice(&[0; 8]); // creation + modification time
    mvhd.extend_from_slice(&TIMESCALE.to_be_bytes());
    mvhd.extend_from_slice(&duration32.to_be_bytes());
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    mvhd.extend_from_slice(&[0; 10]); // reserved
    push_matrix(&mut mvhd);
    mvhd.extend_from_slice(&[0; 24]); // pre_defined
    mvhd.extend_from_slice(&2u32.to_be_bytes()); // next_track_ID

    let mut tkhd = Vec::new();
    tkhd.extend_from_slice(&[0; 8]);
    tkhd.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    tkhd.extend_from_slice(&[0; 4]);
    tkhd.extend_from_slice(&duration32.to_be_bytes());
    tkhd.extend_from_slice(&[0; 8]);
    tkhd.extend_from_slice(&[0; 2]); // layer
    tkhd.extend_from_slice(&[0; 2]); // alternate_group
    tkhd.extend_from_slice(&[0; 2]); // volume (video: 0)
    tkhd.extend_from_slice(&[0; 2]);
    push_matrix(&mut tkhd);
    tkhd.extend_from_slice(&(info.width << 16).to_be_bytes());
    tkhd.extend_from_slice(&(info.height << 16).to_be_bytes());

    let mut mdhd = Vec::new();
    mdhd.extend_from_slice(&[0; 8]);
    mdhd.extend_from_slice(&TIMESCALE.to_be_bytes());
    mdhd.extend_from_slice(&duration32.to_be_bytes());
    mdhd.extend_from_slice(&0x55c4u16.to_be_bytes()); // language "und"
    mdhd.extend_from_slice(&[0; 2]);

    let mut hdlr = Vec::new();
    hdlr.extend_from_slice(&[0; 4]);
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0; 12]);
    hdlr.extend_from_slice(b"VideoHandler\0");

    let dref = full_box(
        b"dref",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &full_box(b"url ", 0, 1, &[])].concat(),
    );
    let minf = bx(
        b"minf",
        &[
            full_box(b"vmhd", 0, 1, &[0; 8]),
            bx(b"dinf", &dref),
            stbl(info, samples, first_offset)?,
        ]
        .concat(),
    );
    let mdia = bx(
        b"mdia",
        &[
            full_box(b"mdhd", 0, 0, &mdhd),
            full_box(b"hdlr", 0, 0, &hdlr),
            minf,
        ]
        .concat(),
    );
    let trak = bx(b"trak", &[full_box(b"tkhd", 0, 3, &tkhd), mdia].concat());
    Ok(bx(
        b"moov",
        &[full_box(b"mvhd", 0, 0, &mvhd), trak].concat(),
    ))
}

/// The start of a fragmented MP4 stream (for live view): `ftyp` and a `moov` with no samples
/// and an `mvex` box, so the samples can follow as [`fragment`]s.
pub fn fragmented_init(info: &StreamInfo) -> io::Result<Vec<u8>> {
    let moov = moov(info, &[], 0)?;
    let mut trex = Vec::new();
    trex.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    trex.extend_from_slice(&1u32.to_be_bytes()); // default sample description index
    trex.extend_from_slice(&[0; 12]); // default duration, size, flags
    let mvex = bx(b"mvex", &full_box(b"trex", 0, 0, &trex));
    // Re-wrap the moov payload with mvex appended.
    let moov = bx(b"moov", &[&moov[8..], &mvex[..]].concat());
    Ok([ftyp(info.codec), moov].concat())
}

/// One sample as a `moof` + `mdat` fragment. `sequence` counts fragments from 1;
/// `decode_time` is in [`TIMESCALE`] units from the stream's start.
pub fn fragment(sequence: u32, decode_time: u64, sample: &Sample) -> Vec<u8> {
    let mfhd = full_box(b"mfhd", 0, 0, &sequence.to_be_bytes());
    // default-base-is-moof: data offsets count from the start of this moof.
    let tfhd = full_box(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes());
    let tfdt = full_box(b"tfdt", 1, 0, &decode_time.to_be_bytes());
    let flags: u32 = if sample.is_key {
        0x0200_0000 // depends on no other sample
    } else {
        0x0101_0000 // depends on others; not a sync sample
    };
    // trun: data offset, duration, size and flags present (0x701); one sample.
    let trun_len = 12 + 4 + 4 + 12;
    let traf_len = 8 + tfhd.len() + tfdt.len() + trun_len;
    let moof_len = 8 + mfhd.len() + traf_len;
    let mut trun = Vec::new();
    trun.extend_from_slice(&1u32.to_be_bytes()); // sample_count
    trun.extend_from_slice(&((moof_len + 8) as u32).to_be_bytes()); // data_offset: past mdat header
    trun.extend_from_slice(&sample.duration_90k.to_be_bytes());
    trun.extend_from_slice(&(sample.data.len() as u32).to_be_bytes());
    trun.extend_from_slice(&flags.to_be_bytes());
    let trun = full_box(b"trun", 0, 0x0701, &trun);
    let traf = bx(b"traf", &[tfhd, tfdt, trun].concat());
    let moof = bx(b"moof", &[mfhd, traf].concat());
    debug_assert_eq!(moof.len(), moof_len);
    [moof, bx(b"mdat", &sample.data)].concat()
}

/// The MIME codec string for a stream, e.g. `avc1.64001f` (for the browser's MediaSource).
pub fn codec_string(info: &StreamInfo) -> Option<String> {
    match info.codec {
        Codec::H264 => {
            let c = &info.decoder_config;
            (c.len() >= 4).then(|| format!("avc1.{:02x}{:02x}{:02x}", c[1], c[2], c[3]))
        }
        Codec::H265 => None,
    }
}

fn stbl(info: &StreamInfo, samples: &[SampleIndexEntry], first_offset: u64) -> io::Result<Vec<u8>> {
    // stsd: one visual sample entry with the decoder configuration.
    let (entry_type, config_type): (&[u8; 4], &[u8; 4]) = match info.codec {
        Codec::H264 => (b"avc1", b"avcC"),
        Codec::H265 => (b"hvc1", b"hvcC"),
    };
    let width = u16::try_from(info.width)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "width too large"))?;
    let height = u16::try_from(info.height)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "height too large"))?;
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0; 6]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry.extend_from_slice(&[0; 16]); // pre_defined + reserved
    entry.extend_from_slice(&width.to_be_bytes());
    entry.extend_from_slice(&height.to_be_bytes());
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
    entry.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    entry.extend_from_slice(&[0; 4]);
    entry.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    entry.extend_from_slice(&[0; 32]); // compressorname
    entry.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    entry.extend_from_slice(&0xffffu16.to_be_bytes()); // pre_defined = -1
    entry.extend_from_slice(&bx(config_type, &info.decoder_config));
    let stsd = full_box(
        b"stsd",
        0,
        0,
        &[&1u32.to_be_bytes()[..], &bx(entry_type, &entry)].concat(),
    );

    // stts: run-length durations.
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        match runs.last_mut() {
            Some((count, delta)) if *delta == s.duration_90k => *count += 1,
            _ => runs.push((1, s.duration_90k)),
        }
    }
    let mut stts = (runs.len() as u32).to_be_bytes().to_vec();
    for (count, delta) in runs {
        stts.extend_from_slice(&count.to_be_bytes());
        stts.extend_from_slice(&delta.to_be_bytes());
    }

    // stss: 1-based numbers of keyframes.
    let keys: Vec<u32> = samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_key)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    let mut stss = (keys.len() as u32).to_be_bytes().to_vec();
    for k in keys {
        stss.extend_from_slice(&k.to_be_bytes());
    }

    // stsc: every chunk holds one sample.
    let mut stsc = 1u32.to_be_bytes().to_vec();
    for v in [1u32, 1, 1] {
        stsc.extend_from_slice(&v.to_be_bytes());
    }

    let mut stsz = vec![0; 4]; // sample_size 0 = sizes follow
    stsz.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    let mut co64 = (samples.len() as u32).to_be_bytes().to_vec();
    let mut offset = first_offset;
    for s in samples {
        stsz.extend_from_slice(&s.size.to_be_bytes());
        co64.extend_from_slice(&offset.to_be_bytes());
        offset += u64::from(s.size);
    }

    Ok(bx(
        b"stbl",
        &[
            stsd,
            full_box(b"stts", 0, 0, &stts),
            full_box(b"stss", 0, 0, &stss),
            full_box(b"stsc", 0, 0, &stsc),
            full_box(b"stsz", 0, 0, &stsz),
            full_box(b"co64", 0, 0, &co64),
        ]
        .concat(),
    ))
}
