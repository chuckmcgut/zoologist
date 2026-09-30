//! Sharp pictures of animals for the species classifier (`species.snapshots`).
//!
//! Zoologist watches a camera's H.264 sub stream, which is small: an animal a few metres away is
//! a few dozen pixels. A Reolink camera behind a Home Hub can hand out a JPEG of its main stream
//! at full resolution (7680×2160 on the owner's panorama camera, 25 times the pixels) without
//! Zoologist having to decode that (H.265) stream. While an animal is in view, a few such
//! snapshots are taken. When the visit ends, the detector finds the animal again in the area
//! around its last known box, and those sharp views are voted with the usual crops.
//!
//! The main picture shows exactly the sub stream's view (checked on the owner's camera), so a
//! box from the sub stream points at the same place in the snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::oneshot;
use zoologist_core::config::{CameraConfig, CameraKind};
use zoologist_core::yuv::{PixelRect, rgb_to_i420};
use zoologist_core::{BBox, Frame, Label};
use zoologist_video::reolink_hub::HubClient;
use zoologist_vision::pool::{DetectJob, DetectResult, DetectorHandle};
use zoologist_vision::species::SpeciesCrop;

use crate::app::AppState;

/// Snapshots per animal visit.
pub const MAX_PER_EVENT: usize = 3;
/// Time between two snapshots of one visit.
pub const INTERVAL: Duration = Duration::from_secs(4);
/// The area cut out around the animal's box, as a multiple of the box (the animal may have
/// moved a little between the sub-stream frame and the snapshot).
const AREA: f32 = 1.5;
/// Sharp views count this much more than sub-stream crops in the species vote.
const WEIGHT: f32 = 2.0;

/// A snapshot's area around the animal, ready for the detector.
pub struct Snap {
    /// The cut-out area, at the snapshot's resolution.
    pub frame: Frame,
    /// Where the animal was expected, normalised to `frame`.
    pub expected: BBox,
}

/// One logged-in Hub client per Hub, shared by all snapshot requests.
#[derive(Clone, Default)]
pub struct Snapshotter {
    clients: Arc<Mutex<HashMap<String, Arc<Mutex<HubClient>>>>>,
}

/// The Hub channel of a camera that can give snapshots: a live camera reached through a Hub.
/// The channel comes from `channel`, or from the RTSP path (`…Preview_01…` is channel 0).
pub fn snapshot_channel(camera: &CameraConfig) -> Option<(String, u8)> {
    if camera.kind != CameraKind::Stream {
        return None; // battery cameras must not be woken up
    }
    let hub = camera.hub.clone()?;
    let channel = camera.channel.or_else(|| {
        let url = camera.detect_url.as_deref()?;
        let i = url.find("Preview_")? + "Preview_".len();
        let n: u8 = url.get(i..i + 2)?.parse().ok()?;
        n.checked_sub(1)
    })?;
    Some((hub, channel))
}

impl Snapshotter {
    fn client(&self, app: &AppState, hub_id: &str) -> Option<Arc<Mutex<HubClient>>> {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = clients.get(hub_id) {
            return Some(c.clone());
        }
        let hub = app.config.reolink_hubs.iter().find(|h| h.id == hub_id)?;
        let client = Arc::new(Mutex::new(HubClient::new(
            &hub.url,
            &hub.user,
            &hub.password,
        )));
        clients.insert(hub_id.to_string(), client.clone());
        Some(client)
    }

    /// Takes a snapshot of `camera` now and cuts out the area around `bbox` (a box from the
    /// sub stream). `None` when the camera cannot give one or the Hub does not answer.
    pub async fn take(&self, app: &AppState, camera: &CameraConfig, bbox: BBox) -> Option<Snap> {
        let (hub_id, channel) = snapshot_channel(camera)?;
        let client = self.client(app, &hub_id)?;
        let camera_id = camera.id.clone();
        let started = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let jpeg = client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .snap(channel, true)
                .map_err(|e| e.to_string())?;
            cut_out(&jpeg, bbox, &camera_id, Utc::now())
        })
        .await;
        match result {
            Ok(Ok(snap)) => {
                tracing::debug!(
                    camera = %camera.id,
                    ms = started.elapsed().as_millis() as u64,
                    w = snap.frame.width,
                    h = snap.frame.height,
                    "animal snapshot"
                );
                Some(snap)
            }
            Ok(Err(e)) => {
                tracing::warn!(camera = %camera.id, "no snapshot: {e}");
                None
            }
            Err(e) => {
                tracing::warn!(camera = %camera.id, "no snapshot: {e}");
                None
            }
        }
    }
}

/// Decodes the snapshot and keeps only the area around `bbox`, as an I420 frame.
fn cut_out(jpeg: &[u8], bbox: BBox, camera_id: &str, at: DateTime<Utc>) -> Result<Snap, String> {
    let image = image::load_from_memory(jpeg)
        .map_err(|e| format!("cannot decode the snapshot: {e}"))?
        .into_rgb8();
    let (w, h) = image.dimensions();
    let area = bbox.expand(AREA).clamp();
    let (x, y, aw, ah) = area.to_pixels(w, h);
    // I420 needs even sizes.
    let (aw, ah) = (aw & !1, ah & !1);
    if aw < 32 || ah < 32 {
        return Err("the animal's area is too small".into());
    }
    let mut rgb = Vec::with_capacity((aw * ah * 3) as usize);
    for row in y..y + ah {
        let start = ((row * w + x) * 3) as usize;
        rgb.extend_from_slice(&image.as_raw()[start..start + (aw * 3) as usize]);
    }
    let i420 = rgb_to_i420(&rgb, aw, ah).map_err(|e| e.to_string())?;
    // The expected box, relative to the cut-out area.
    let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
    let (fw, fh) = (aw as f32 / w as f32, ah as f32 / h as f32);
    let expected = BBox::new(
        (bbox.x1 - fx) / fw,
        (bbox.y1 - fy) / fh,
        (bbox.x2 - fx) / fw,
        (bbox.y2 - fy) / fh,
    )
    .clamp();
    Ok(Snap {
        frame: Frame {
            camera_id: camera_id.into(),
            seq: 0,
            captured_at: at,
            width: aw,
            height: ah,
            i420: Arc::new(i420),
        },
        expected,
    })
}

/// Finds the animal in a snapshot's area with the detector: the animal box overlapping the
/// expected one most (or the most confident). `None` if the detector finds no animal there.
pub async fn locate(detector: &DetectorHandle, snap: Snap) -> Option<SpeciesCrop> {
    let (job, rx) = locate_job(&snap);
    detector.submit_background(job);
    pick(snap, rx.await.ok()?)
}

/// [`locate`] for threads outside the async runtime (the Hub importer).
pub fn locate_blocking(detector: &DetectorHandle, snap: Snap) -> Option<SpeciesCrop> {
    let (job, rx) = locate_job(&snap);
    detector.submit_background(job);
    pick(snap, rx.blocking_recv().ok()?)
}

fn locate_job(snap: &Snap) -> (DetectJob, oneshot::Receiver<DetectResult>) {
    let (tx, rx) = oneshot::channel();
    let full = PixelRect {
        x: 0,
        y: 0,
        w: snap.frame.width,
        h: snap.frame.height,
    };
    let job = DetectJob {
        frame: snap.frame.clone(),
        regions: vec![full],
        reply: tx,
    };
    (job, rx)
}

fn pick(snap: Snap, result: DetectResult) -> Option<SpeciesCrop> {
    let best = result
        .detections
        .into_iter()
        .filter(|d| d.label == Label::Animal)
        .max_by(|a, b| {
            let key = |d: &zoologist_core::Detection| (d.bbox.iou(&snap.expected), d.score);
            key(a)
                .partial_cmp(&key(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })?;
    Some(SpeciesCrop {
        frame: snap.frame,
        bbox: best.bbox,
        quality: best.score * best.bbox.area().sqrt() * WEIGHT,
    })
}

/// The frame `offset` into a recording, as a JPEG, from `ffmpeg` (any codec it can decode, e.g.
/// the H.265 main recordings of Reolink battery cameras). `None` when ffmpeg is missing or fails.
pub fn ffmpeg_frame(
    ffmpeg: &std::path::Path,
    video: &std::path::Path,
    offset: f64,
) -> Option<Vec<u8>> {
    let out = std::process::Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-ss",
            &format!("{:.3}", offset.max(0.0)),
            "-i",
        ])
        .arg(video)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            "-q:v",
            "2",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    (out.status.success() && out.stdout.starts_with(&[0xff, 0xd8])).then_some(out.stdout)
}

/// Cuts the area around `bbox` out of a full-resolution picture (see [`Snap`]).
pub fn snap_from_jpeg(jpeg: &[u8], bbox: BBox, camera_id: &str, at: DateTime<Utc>) -> Option<Snap> {
    cut_out(jpeg, bbox, camera_id, at).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(url: &str, kind: CameraKind, hub: Option<&str>) -> CameraConfig {
        let mut c: CameraConfig = serde_json::from_value(serde_json::json!({
            "id": "c", "name": "C", "detect_url": url, "record_url": url
        }))
        .unwrap();
        c.kind = kind;
        c.hub = hub.map(Into::into);
        c
    }

    #[test]
    fn the_hub_channel_comes_from_the_rtsp_path_or_the_config() {
        let url = "rtsp://10.0.0.1:554/h264Preview_01_sub";
        assert_eq!(
            snapshot_channel(&camera(url, CameraKind::Stream, Some("hub"))),
            Some(("hub".into(), 0))
        );
        let url3 = "rtsp://10.0.0.1:554/Preview_04_main";
        assert_eq!(
            snapshot_channel(&camera(url3, CameraKind::Stream, Some("hub"))),
            Some(("hub".into(), 3))
        );
        let mut explicit = camera("rtsp://10.0.0.1/live", CameraKind::Stream, Some("hub"));
        explicit.channel = Some(5);
        assert_eq!(snapshot_channel(&explicit), Some(("hub".into(), 5)));
        // Not through a Hub, or a battery camera (never woken for a snapshot).
        assert_eq!(
            snapshot_channel(&camera(url, CameraKind::Stream, None)),
            None
        );
        assert_eq!(
            snapshot_channel(&camera(url, CameraKind::HubClips, Some("hub"))),
            None
        );
    }

    #[test]
    fn the_area_around_the_animal_is_cut_out_with_the_box_inside() {
        let (w, h) = (800u32, 400u32);
        let img = image::RgbImage::from_fn(w, h, |x, _| image::Rgb([(x % 256) as u8, 90, 30]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let bbox = BBox::new(0.5, 0.5, 0.6, 0.7);
        let snap = cut_out(&jpeg, bbox, "c", Utc::now()).unwrap();
        // 2.5 × the box (80×80 px) = 200×200 px, around its centre.
        assert_eq!((snap.frame.width, snap.frame.height), (200, 200));
        let e = snap.expected;
        assert!(
            (e.x1 - 0.3).abs() < 0.02 && (e.x2 - 0.7).abs() < 0.02,
            "{e:?}"
        );
        assert!(
            (e.y1 - 0.3).abs() < 0.02 && (e.y2 - 0.7).abs() < 0.02,
            "{e:?}"
        );
    }
}
