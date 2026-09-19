//! JPEG snapshots of frames (the UI's live camera tiles, event thumbnails).

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use image::ImageEncoder;
use image::codecs::jpeg::JpegEncoder;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use zoologist_core::Frame;
use zoologist_core::yuv::i420_to_rgb_full;

/// The newest frame of one camera, shared between the decoder and whoever needs a picture.
pub type LatestFrame = Arc<RwLock<Option<Frame>>>;

/// Encodes an RGB image as JPEG bytes.
pub fn encode_jpeg(
    rgb: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> image::ImageResult<Vec<u8>> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).write_image(
        rgb,
        width,
        height,
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

/// Writes `frame` as a JPEG to `path`, via a temporary file and a rename so readers never see
/// a half-written file.
pub fn write_frame_jpeg(frame: &Frame, path: &Path, quality: u8) -> std::io::Result<()> {
    let jpeg = encode_jpeg(&i420_to_rgb_full(frame), frame.width, frame.height, quality)
        .map_err(std::io::Error::other)?;
    write_atomic(path, &jpeg)
}

/// Writes `bytes` to `path` atomically (temporary file in the same directory, then rename).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = BufWriter::new(std::fs::File::create(&tmp)?);
        file.write_all(bytes)?;
        file.flush()?;
    }
    std::fs::rename(&tmp, path)
}

/// Every `every`, writes the camera's newest frame (if it changed) to `path` as a JPEG.
pub fn spawn_latest_writer(
    latest: LatestFrame,
    path: std::path::PathBuf,
    every: Duration,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_seq = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(every) => {}
            }
            let frame = latest.read().unwrap_or_else(|e| e.into_inner()).clone();
            let Some(frame) = frame else { continue };
            if last_seq == Some(frame.seq) {
                continue;
            }
            last_seq = Some(frame.seq);
            let path = path.clone();
            let result =
                tokio::task::spawn_blocking(move || write_frame_jpeg(&frame, &path, 80)).await;
            if let Ok(Err(e)) = result {
                warn!("cannot write latest frame: {e}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use zoologist_core::yuv::rgb_to_i420;

    use super::*;

    #[test]
    fn writes_a_decodable_jpeg_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest/cam.jpg");
        let rgb: Vec<u8> = (0..64 * 48).flat_map(|_| [30u8, 160, 90]).collect();
        let frame = Frame {
            camera_id: "cam".into(),
            seq: 1,
            captured_at: Utc::now(),
            width: 64,
            height: 48,
            i420: Arc::new(rgb_to_i420(&rgb, 64, 48).unwrap()),
        };
        write_frame_jpeg(&frame, &path, 80).unwrap();
        let decoded = image::open(&path).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (64, 48));
        let px = decoded.get_pixel(10, 10).0;
        assert!((px[1] as i32 - 160).abs() < 12, "{px:?}");
        assert!(!path.with_extension("tmp").exists());
    }
}
