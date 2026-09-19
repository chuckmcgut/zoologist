//! Reading the sample table of an MP4 file (plan Step 6.2).
//!
//! Used for our own recording segments and for recordings downloaded from a Reolink Hub. Only
//! the first video track is read. Works whether the `moov` index is before or after the data,
//! and for fragmented MP4 (`moof` + `mdat` pairs, as the Reolink Hub sends its recordings).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use bytes::Bytes;

use crate::mp4w::{Sample, SampleIndexEntry, TIMESCALE};
use crate::stream::{Codec, StreamInfo};

/// `moov` boxes larger than this are rejected (a day of 30 fps video needs ~20 MB).
const MAX_MOOV: u64 = 64 * 1024 * 1024;
/// `moof` boxes larger than this are rejected (one holds a few seconds of samples).
const MAX_MOOF: u64 = 16 * 1024 * 1024;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Reads the video track's parameters and sample index. `wall_us` in the result counts from 0
/// at the first sample's presentation time; add the file's start time to get wall-clock time.
pub fn read_mp4_index(path: &Path) -> io::Result<(StreamInfo, Vec<SampleIndexEntry>)> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut pos = 0u64;
    let mut moov = None;
    let mut moofs = Vec::new();
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let size32 = u32::from_be_bytes(header[..4].try_into().expect("4 bytes"));
        let fourcc: [u8; 4] = header[4..8].try_into().expect("4 bytes");
        let (size, header_len) = match size32 {
            0 => (file_len - pos, 8),
            1 => {
                file.read_exact(&mut header[8..16])?;
                (
                    u64::from_be_bytes(header[8..16].try_into().expect("8 bytes")),
                    16,
                )
            }
            n => (u64::from(n), 8),
        };
        if size < header_len {
            return Err(invalid("box smaller than its header"));
        }
        match &fourcc {
            b"moov" => {
                if size > MAX_MOOV {
                    return Err(invalid("moov box too large"));
                }
                let mut body = vec![0u8; (size - header_len) as usize];
                file.read_exact(&mut body)?;
                moov = Some(body);
            }
            // A download cut short can end inside a fragment: keep only complete moof boxes.
            b"moof" if pos + size <= file_len => {
                if size > MAX_MOOF {
                    return Err(invalid("moof box too large"));
                }
                let mut body = vec![0u8; (size - header_len) as usize];
                file.read_exact(&mut body)?;
                moofs.push((pos, body));
            }
            _ => {}
        }
        pos += size;
    }
    let moov = moov.ok_or_else(|| invalid("no moov box"))?;
    let track = parse_moov(&moov)?;
    if !track.entries.is_empty() || moofs.is_empty() {
        return Ok((track.info, track.entries));
    }
    let entries = parse_fragments(&track, &moofs, file_len)?;
    Ok((track.info, entries))
}

/// Reads the bytes of the given samples, in order.
pub fn read_samples(path: &Path, entries: &[SampleIndexEntry]) -> io::Result<Vec<Sample>> {
    let mut file = File::open(path)?;
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        file.seek(SeekFrom::Start(e.offset))?;
        let mut data = vec![0u8; e.size as usize];
        file.read_exact(&mut data)?;
        out.push(Sample {
            data: Bytes::from(data),
            duration_90k: e.duration_90k,
            is_key: e.is_key,
            wall_us: e.wall_us,
        });
    }
    Ok(out)
}

/// Iterates over the child boxes of a box payload: `(fourcc, payload)`.
fn children(mut data: &[u8]) -> impl Iterator<Item = io::Result<([u8; 4], &[u8])>> {
    std::iter::from_fn(move || {
        if data.len() < 8 {
            return None;
        }
        let size32 = u32::from_be_bytes(data[..4].try_into().expect("4 bytes")) as usize;
        let fourcc: [u8; 4] = data[4..8].try_into().expect("4 bytes");
        let (size, header) = match size32 {
            0 => (data.len(), 8),
            1 if data.len() >= 16 => (
                u64::from_be_bytes(data[8..16].try_into().expect("8 bytes")) as usize,
                16,
            ),
            n => (n, 8),
        };
        if size < header || size > data.len() {
            data = &[];
            return Some(Err(invalid(format!(
                "bad size for box {}",
                String::from_utf8_lossy(&fourcc)
            ))));
        }
        let payload = &data[header..size];
        data = &data[size..];
        Some(Ok((fourcc, payload)))
    })
}

fn child<'a>(data: &'a [u8], want: &[u8; 4]) -> io::Result<Option<&'a [u8]>> {
    for c in children(data) {
        let (fourcc, payload) = c?;
        if &fourcc == want {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

fn require<'a>(data: &'a [u8], want: &[u8; 4]) -> io::Result<&'a [u8]> {
    child(data, want)?
        .ok_or_else(|| invalid(format!("missing {} box", String::from_utf8_lossy(want))))
}

/// A small big-endian cursor that turns short reads into errors.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.data.len() < n {
            return Err(invalid("box truncated"));
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("2 bytes"),
        ))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
    /// Version and flags of a full box.
    fn full_header(&mut self) -> io::Result<(u8, u32)> {
        let version = self.u8()?;
        let flags = self.take(3)?;
        Ok((
            version,
            u32::from_be_bytes([0, flags[0], flags[1], flags[2]]),
        ))
    }
}

/// The video track of a `moov`: parameters, (non-fragmented) samples, and what fragments need.
struct VideoTrack {
    info: StreamInfo,
    entries: Vec<SampleIndexEntry>,
    track_id: u32,
    timescale: u32,
    /// `trex` defaults: duration, size, flags.
    defaults: (u32, u32, u32),
}

fn parse_moov(moov: &[u8]) -> io::Result<VideoTrack> {
    for c in children(moov) {
        let (fourcc, trak) = c?;
        if &fourcc != b"trak" {
            continue;
        }
        let mdia = require(trak, b"mdia")?;
        let hdlr = require(mdia, b"hdlr")?;
        if hdlr.get(8..12) != Some(b"vide") {
            continue;
        }
        let mut tkhd = Reader::new(require(trak, b"tkhd")?);
        let (version, _) = tkhd.full_header()?;
        tkhd.take(if version == 1 { 16 } else { 8 })?;
        let track_id = tkhd.u32()?;
        let (info, entries, timescale) = parse_video_trak(mdia)?;
        let mut defaults = (0, 0, 0);
        if let Some(mvex) = child(moov, b"mvex")? {
            for c in children(mvex) {
                let (fourcc, trex) = c?;
                if &fourcc != b"trex" {
                    continue;
                }
                let mut r = Reader::new(trex);
                r.full_header()?;
                if r.u32()? == track_id {
                    r.u32()?; // default sample description index
                    defaults = (r.u32()?, r.u32()?, r.u32()?);
                }
            }
        }
        return Ok(VideoTrack {
            info,
            entries,
            track_id,
            timescale,
            defaults,
        });
    }
    Err(invalid("no video track"))
}

/// `sample_is_non_sync_sample` in ISO BMFF sample flags.
const NON_SYNC: u32 = 0x0001_0000;

/// Builds the sample index of a fragmented file from its `moof` boxes (`(file offset, body)`).
/// Samples that would lie past the end of the file (a cut-short download) are dropped.
fn parse_fragments(
    track: &VideoTrack,
    moofs: &[(u64, Vec<u8>)],
    file_len: u64,
) -> io::Result<Vec<SampleIndexEntry>> {
    let mut entries = Vec::new();
    let mut next_decode_time: Option<u64> = None;
    for (moof_pos, moof) in moofs {
        for c in children(moof) {
            let (fourcc, traf) = c?;
            if &fourcc != b"traf" {
                continue;
            }
            let mut tfhd = Reader::new(require(traf, b"tfhd")?);
            let (_, flags) = tfhd.full_header()?;
            if tfhd.u32()? != track.track_id {
                continue;
            }
            let base = if flags & 0x1 != 0 {
                tfhd.u64()?
            } else {
                *moof_pos
            };
            if flags & 0x2 != 0 {
                tfhd.u32()?; // sample description index
            }
            let (mut def_dur, mut def_size, mut def_flags) = track.defaults;
            if flags & 0x8 != 0 {
                def_dur = tfhd.u32()?;
            }
            if flags & 0x10 != 0 {
                def_size = tfhd.u32()?;
            }
            if flags & 0x20 != 0 {
                def_flags = tfhd.u32()?;
            }
            let mut decode_time = match child(traf, b"tfdt")? {
                Some(tfdt) => {
                    let mut r = Reader::new(tfdt);
                    let (version, _) = r.full_header()?;
                    if version == 1 {
                        r.u64()?
                    } else {
                        u64::from(r.u32()?)
                    }
                }
                None => next_decode_time.unwrap_or(0),
            };
            // Consecutive `trun`s continue where the previous one ended.
            let mut data_pos = base;
            for c in children(traf) {
                let (fourcc, trun) = c?;
                if &fourcc != b"trun" {
                    continue;
                }
                let mut r = Reader::new(trun);
                let (version, tflags) = r.full_header()?;
                let count = r.u32()?;
                if tflags & 0x1 != 0 {
                    data_pos = base.wrapping_add_signed(i64::from(r.u32()? as i32));
                }
                let first_flags = if tflags & 0x4 != 0 {
                    Some(r.u32()?)
                } else {
                    None
                };
                for i in 0..count {
                    let dur = if tflags & 0x100 != 0 {
                        r.u32()?
                    } else {
                        def_dur
                    };
                    let size = if tflags & 0x200 != 0 {
                        r.u32()?
                    } else {
                        def_size
                    };
                    let sflags = if tflags & 0x400 != 0 {
                        r.u32()?
                    } else if i == 0 {
                        first_flags.unwrap_or(def_flags)
                    } else {
                        def_flags
                    };
                    let cts = if tflags & 0x800 != 0 {
                        let raw = r.u32()?;
                        if version == 1 {
                            i64::from(raw as i32)
                        } else {
                            i64::from(raw)
                        }
                    } else {
                        0
                    };
                    if data_pos + u64::from(size) > file_len {
                        return Ok(finish(entries));
                    }
                    let pts = decode_time as i64 + cts;
                    entries.push(SampleIndexEntry {
                        offset: data_pos,
                        size,
                        duration_90k: rescale(i64::from(dur), track.timescale) as u32,
                        is_key: sflags & NON_SYNC == 0,
                        wall_us: pts * 1_000_000 / i64::from(track.timescale),
                    });
                    data_pos += u64::from(size);
                    decode_time += u64::from(dur);
                }
            }
            next_decode_time = Some(decode_time);
        }
    }
    Ok(finish(entries))
}

/// Makes `wall_us` start at 0 for the first frame shown.
fn finish(mut entries: Vec<SampleIndexEntry>) -> Vec<SampleIndexEntry> {
    if let Some(first) = entries.iter().map(|e| e.wall_us).min() {
        for e in &mut entries {
            e.wall_us -= first;
        }
    }
    entries
}

fn parse_video_trak(mdia: &[u8]) -> io::Result<(StreamInfo, Vec<SampleIndexEntry>, u32)> {
    let mut mdhd = Reader::new(require(mdia, b"mdhd")?);
    let (version, _) = mdhd.full_header()?;
    let timescale = if version == 1 {
        mdhd.take(16)?;
        mdhd.u32()?
    } else {
        mdhd.take(8)?;
        mdhd.u32()?
    };
    if timescale == 0 {
        return Err(invalid("timescale is zero"));
    }
    let stbl = require(require(mdia, b"minf")?, b"stbl")?;
    let info = parse_stsd(require(stbl, b"stsd")?)?;

    // Sample sizes.
    let mut stsz = Reader::new(require(stbl, b"stsz")?);
    stsz.full_header()?;
    let fixed_size = stsz.u32()?;
    let count = stsz.u32()? as usize;
    let sizes: Vec<u32> = if fixed_size != 0 {
        vec![fixed_size; count]
    } else {
        (0..count).map(|_| stsz.u32()).collect::<io::Result<_>>()?
    };

    // Durations (in the track timescale), expanded per sample.
    let mut stts = Reader::new(require(stbl, b"stts")?);
    stts.full_header()?;
    let mut durations = Vec::with_capacity(count);
    for _ in 0..stts.u32()? {
        let (n, delta) = (stts.u32()?, stts.u32()?);
        durations.extend(std::iter::repeat_n(delta, n as usize));
    }
    durations.resize(count, durations.last().copied().unwrap_or(0));

    // Composition offsets (B-frames), optional.
    let mut cts = vec![0i64; count];
    if let Some(ctts) = child(stbl, b"ctts")? {
        let mut r = Reader::new(ctts);
        let (version, _) = r.full_header()?;
        let mut i = 0;
        for _ in 0..r.u32()? {
            let n = r.u32()? as usize;
            let raw = r.u32()?;
            let offset = if version == 1 {
                raw as i32 as i64
            } else {
                raw as i64
            };
            for slot in cts.iter_mut().skip(i).take(n) {
                *slot = offset;
            }
            i += n;
        }
    }

    // Keyframes; without stss every sample is a keyframe.
    let keys = match child(stbl, b"stss")? {
        None => vec![true; count],
        Some(stss) => {
            let mut r = Reader::new(stss);
            r.full_header()?;
            let mut keys = vec![false; count];
            for _ in 0..r.u32()? {
                let n = r.u32()? as usize;
                if (1..=count).contains(&n) {
                    keys[n - 1] = true;
                }
            }
            keys
        }
    };

    // Chunk offsets.
    let chunk_offsets: Vec<u64> = if let Some(co64) = child(stbl, b"co64")? {
        let mut r = Reader::new(co64);
        r.full_header()?;
        (0..r.u32()?).map(|_| r.u64()).collect::<io::Result<_>>()?
    } else {
        let mut r = Reader::new(require(stbl, b"stco")?);
        r.full_header()?;
        (0..r.u32()?)
            .map(|_| r.u32().map(u64::from))
            .collect::<io::Result<_>>()?
    };

    // Sample-to-chunk runs: (first_chunk, samples_per_chunk), 1-based chunks.
    let mut stsc = Reader::new(require(stbl, b"stsc")?);
    stsc.full_header()?;
    let runs: Vec<(u32, u32)> = (0..stsc.u32()?)
        .map(|_| {
            let first = stsc.u32()?;
            let per = stsc.u32()?;
            stsc.u32()?; // sample description index
            Ok((first, per))
        })
        .collect::<io::Result<_>>()?;

    let mut entries = Vec::with_capacity(count);
    let mut sample = 0usize;
    let mut decode_time = 0i64;
    for (chunk_index, &chunk_offset) in chunk_offsets.iter().enumerate() {
        let chunk_no = chunk_index as u32 + 1;
        let per_chunk = runs
            .iter()
            .rev()
            .find(|(first, _)| *first <= chunk_no)
            .map_or(0, |(_, per)| *per);
        let mut offset = chunk_offset;
        for _ in 0..per_chunk {
            if sample >= count {
                break;
            }
            let pts = decode_time + cts[sample];
            entries.push(SampleIndexEntry {
                offset,
                size: sizes[sample],
                duration_90k: rescale(durations[sample] as i64, timescale) as u32,
                is_key: keys[sample],
                wall_us: pts * 1_000_000 / timescale as i64,
            });
            offset += u64::from(sizes[sample]);
            decode_time += i64::from(durations[sample]);
            sample += 1;
        }
    }
    if entries.len() != count {
        return Err(invalid(format!(
            "sample table describes {} of {count} samples",
            entries.len()
        )));
    }
    Ok((info, finish(entries), timescale))
}

fn rescale(value: i64, timescale: u32) -> i64 {
    value * i64::from(TIMESCALE) / i64::from(timescale)
}

fn parse_stsd(stsd: &[u8]) -> io::Result<StreamInfo> {
    let mut r = Reader::new(stsd);
    r.full_header()?;
    if r.u32()? == 0 {
        return Err(invalid("empty stsd"));
    }
    let (fourcc, entry) = children(r.data)
        .next()
        .ok_or_else(|| invalid("empty stsd"))??;
    let (codec, config_type): (Codec, &[u8; 4]) = match &fourcc {
        b"avc1" | b"avc3" => (Codec::H264, b"avcC"),
        b"hvc1" | b"hev1" => (Codec::H265, b"hvcC"),
        other => {
            return Err(invalid(format!(
                "unsupported video codec {}",
                String::from_utf8_lossy(other)
            )));
        }
    };
    let mut e = Reader::new(entry);
    e.take(24)?;
    let width = u32::from(e.u16()?);
    let height = u32::from(e.u16()?);
    e.take(50)?; // resolution, reserved, frame count, compressor name, depth, pre_defined
    let config = require(e.data, config_type)?;
    Ok(StreamInfo {
        codec,
        width,
        height,
        decoder_config: Bytes::copy_from_slice(config),
    })
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::h264::tests::fixture;
    use crate::h264::{NAL_IDR, NAL_PPS, NAL_SPS, annexb_nals, build_avc_config, nal_type};
    use crate::mp4w::write_mp4;

    /// The High-profile fixture as MP4 samples at 10 fps.
    fn fixture_samples() -> (StreamInfo, Vec<Sample>) {
        let data = fixture("testsrc_high_640x360_10fps.h264");
        let nals = annexb_nals(&data);
        let sps = nals.iter().find(|n| nal_type(n) == NAL_SPS).unwrap();
        let pps = nals.iter().find(|n| nal_type(n) == NAL_PPS).unwrap();
        let info = StreamInfo {
            codec: Codec::H264,
            width: 640,
            height: 360,
            decoder_config: Bytes::from(build_avc_config(sps, pps).unwrap()),
        };
        let samples = nals
            .iter()
            .filter(|n| matches!(nal_type(n), 1 | 5))
            .enumerate()
            .map(|(i, nal)| {
                let mut data = (nal.len() as u32).to_be_bytes().to_vec();
                data.extend_from_slice(nal);
                Sample {
                    data: Bytes::from(data),
                    duration_90k: 9000,
                    is_key: nal_type(nal) == NAL_IDR,
                    wall_us: i as i64 * 100_000,
                }
            })
            .collect();
        (info, samples)
    }

    fn have(tool: &str) -> bool {
        Command::new(tool).arg("-version").output().is_ok()
    }

    #[test]
    fn write_then_read_round_trips() {
        let (info, samples) = fixture_samples();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("seg.mp4");
        let written = write_mp4(File::create(&path).unwrap(), &info, &samples).unwrap();
        let (read_info, entries) = read_mp4_index(&path).unwrap();
        assert_eq!(read_info, info);
        assert_eq!(entries, written);
        assert_eq!(read_samples(&path, &entries).unwrap(), samples);
    }

    #[test]
    fn rejects_files_not_starting_with_a_keyframe() {
        let (info, samples) = fixture_samples();
        assert!(write_mp4(Vec::new(), &info, &samples[1..]).is_err());
        assert!(write_mp4(Vec::new(), &info, &[]).is_err());
    }

    #[test]
    fn written_file_is_valid_for_ffprobe_and_decodes_identically() {
        if !have("ffprobe") || !have("ffmpeg") {
            eprintln!("ffmpeg/ffprobe not installed; skipping");
            return;
        }
        let (info, samples) = fixture_samples();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        write_mp4(File::create(&path).unwrap(), &info, &samples).unwrap();
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries"])
            .args(["stream=codec_name,width,height,nb_read_frames:format=duration"])
            .args(["-of", "csv=p=0"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            probe.status.success(),
            "{}",
            String::from_utf8_lossy(&probe.stderr)
        );
        assert!(
            probe.stderr.is_empty(),
            "ffprobe complained: {}",
            String::from_utf8_lossy(&probe.stderr)
        );
        let text = String::from_utf8_lossy(&probe.stdout);
        assert!(text.contains("h264,640,360,100"), "{text}");
        assert!(text.contains("10.0"), "duration should be 10 s: {text}");

        // Decoding the MP4 gives exactly the same pixels as decoding the raw stream.
        let decode = |input: &Path, raw: bool| {
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-v", "error"]);
            if raw {
                cmd.args(["-f", "h264"]);
            }
            let out = cmd
                .arg("-i")
                .arg(input)
                .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
                .output()
                .unwrap();
            out.stdout
        };
        let raw = dir.path().join("raw.h264");
        std::fs::write(&raw, fixture("testsrc_high_640x360_10fps.h264")).unwrap();
        assert_eq!(decode(&path, false), decode(&raw, true));
    }

    #[test]
    fn reads_an_ffmpeg_made_mp4_with_moov_at_the_end() {
        if !have("ffmpeg") {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw.h264");
        std::fs::write(&raw, fixture("testsrc_main_640x360_10fps.h264")).unwrap();
        let path = dir.path().join("ffmpeg.mp4");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-r", "10", "-f", "h264", "-i"])
            .arg(&raw)
            .args(["-c", "copy"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let (info, entries) = read_mp4_index(&path).unwrap();
        assert_eq!(
            (info.codec, info.width, info.height),
            (Codec::H264, 640, 360)
        );
        assert_eq!(entries.len(), 100);
        assert_eq!(entries.iter().filter(|e| e.is_key).count(), 5);
        assert_eq!(entries[0].wall_us, 0);
        assert_eq!(entries[10].wall_us, 1_000_000);
        assert!(entries.iter().all(|e| e.duration_90k == 9000));
        // Every sample must be well-formed AVCC.
        for s in read_samples(&path, &entries).unwrap() {
            assert!(crate::h264::avcc_nals(&s.data, 4).all(|n| n.is_ok()));
        }
    }

    #[test]
    fn reads_fragmented_mp4_even_when_cut_short() {
        if !have("ffmpeg") {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("raw.h264");
        std::fs::write(&raw, fixture("testsrc_main_640x360_10fps.h264")).unwrap();
        let path = dir.path().join("frag.mp4");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-r", "10", "-f", "h264", "-i"])
            .arg(&raw)
            .args([
                "-c",
                "copy",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
            ])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let (info, entries) = read_mp4_index(&path).unwrap();
        assert_eq!(
            (info.codec, info.width, info.height),
            (Codec::H264, 640, 360)
        );
        assert_eq!(entries.len(), 100);
        assert_eq!(entries.iter().filter(|e| e.is_key).count(), 5);
        assert!(entries[0].is_key);
        assert_eq!(entries[10].wall_us, 1_000_000);
        let samples = read_samples(&path, &entries).unwrap();
        for s in &samples {
            assert!(crate::h264::avcc_nals(&s.data, 4).all(|n| n.is_ok()));
        }

        // Cut the file in the middle of the last fragment: the complete samples remain.
        let bytes = std::fs::read(&path).unwrap();
        let cut = dir.path().join("cut.mp4");
        std::fs::write(&cut, &bytes[..bytes.len() - 5000]).unwrap();
        let (_, partial) = read_mp4_index(&cut).unwrap();
        assert!(
            partial.len() < 100 && partial.len() >= 60,
            "{}",
            partial.len()
        );
        assert_eq!(partial[..], entries[..partial.len()]);
    }

    #[test]
    fn live_fragments_read_back_as_the_same_samples() {
        use crate::mp4w::{codec_string, fragment, fragmented_init};
        let (info, samples) = fixture_samples();
        let mut bytes = fragmented_init(&info).unwrap();
        let mut t = 0u64;
        for (i, s) in samples.iter().enumerate() {
            bytes.extend(fragment(i as u32 + 1, t, s));
            t += u64::from(s.duration_90k);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.mp4");
        std::fs::write(&path, &bytes).unwrap();
        let (read_info, entries) = read_mp4_index(&path).unwrap();
        assert_eq!(read_info, info);
        assert_eq!(entries.len(), samples.len());
        let back = read_samples(&path, &entries).unwrap();
        for (a, b) in back.iter().zip(&samples) {
            assert_eq!(
                (&a.data, a.is_key, a.duration_90k),
                (&b.data, b.is_key, b.duration_90k)
            );
        }
        assert_eq!(entries[10].wall_us, 1_000_000);
        assert!(codec_string(&info).unwrap().starts_with("avc1.64"));
        if have("ffprobe") {
            let probe = Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-count_frames",
                    "-show_entries",
                    "stream=nb_read_frames",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                probe.stderr.is_empty(),
                "{}",
                String::from_utf8_lossy(&probe.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "100");
        }
    }

    /// A real Reolink Hub recording, if `zoologist hub-test` saved one (owner fixtures are not
    /// in git).
    #[test]
    fn reads_an_owner_hub_recording() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/fixtures/owner/hub/ch4-sub.mp4");
        if !path.exists() {
            eprintln!("no owner Hub recording; skipping");
            return;
        }
        let (info, entries) = read_mp4_index(&path).unwrap();
        assert_eq!(info.codec, Codec::H264);
        assert!(entries.len() > 20, "{}", entries.len());
        assert!(entries[0].is_key);
        let secs = entries
            .iter()
            .map(|e| f64::from(e.duration_90k))
            .sum::<f64>()
            / 90_000.0;
        eprintln!(
            "Hub recording: {}x{}, {} frames, {secs:.1} s, {} keyframes",
            info.width,
            info.height,
            entries.len(),
            entries.iter().filter(|e| e.is_key).count()
        );
    }

    #[test]
    fn garbage_is_rejected_not_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.mp4");
        std::fs::write(&path, b"\x00\x00\x00\x10moovgarbage!").unwrap();
        assert!(read_mp4_index(&path).is_err());
        std::fs::write(&path, b"not an mp4 at all").unwrap();
        assert!(read_mp4_index(&path).is_err());
    }
}
