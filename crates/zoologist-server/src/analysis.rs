//! Per-camera analysis: motion → detector regions → detection → tracking → events
//! (plan Step 7.1). Runs on its own thread, one per camera.

use std::sync::{Arc, RwLock};

use tokio::sync::{mpsc, oneshot};
use zoologist_core::config::{CameraConfig, Config};
use zoologist_core::{Frame, Label};
use zoologist_video::snapshot::LatestFrame;
use zoologist_vision::events::{EventManager, EventUpdate};
use zoologist_vision::motion::MotionDetector;
use zoologist_vision::pool::{DetectJob, DetectorHandle};
use zoologist_vision::regions::{select_regions, tile_regions};
use zoologist_vision::tracker::Tracker;

use crate::app::AnalysisStats;

/// An event update and the camera it belongs to.
pub type CameraUpdate = (String, EventUpdate);

/// How a camera's frames are analysed.
#[derive(Clone, Copy, Debug, Default)]
pub struct AnalysisOptions {
    /// Recorded clips: detection jobs wait behind every live camera's.
    pub background: bool,
    /// Until this time, run the detector on the whole frame (in tiles) without waiting for
    /// motion: a battery camera's clip starts with the animal already in view.
    pub tiles_until: Option<chrono::DateTime<chrono::Utc>>,
    /// The detector's input size, when it is not `inference.detector` (a Hub's own detector).
    pub model_size: Option<u32>,
}

/// Detector runs in a row that found nothing, on frames with motion, before a live camera's
/// detector slows down to `inference.idle_motion_seconds` (3 seconds at 5 frames a second).
const IDLE_AFTER_RUNS: u32 = 15;
/// This long without motion and the count starts again.
const IDLE_FORGET: chrono::Duration = chrono::Duration::seconds(10);

/// Starts the analysis thread of `camera`. It ends when `frames` closes, after ending every
/// open event.
pub fn spawn_analysis(
    camera: CameraConfig,
    config: Arc<Config>,
    mut frames: mpsc::Receiver<Frame>,
    detector: Option<DetectorHandle>,
    updates: mpsc::Sender<CameraUpdate>,
    latest: LatestFrame,
    stats: Arc<RwLock<AnalysisStats>>,
    options: AnalysisOptions,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("analyse-{}", camera.id))
        .spawn(move || {
            let model_size = options.model_size.unwrap_or_else(|| {
                config
                    .models
                    .get(&config.inference.detector)
                    .map_or(320, |m| m.input_size)
            });
            let keepalive = chrono::Duration::milliseconds(
                (config.inference.keepalive_seconds * 1000.0) as i64,
            );
            let mut motion: Option<MotionDetector> = None;
            let mut tracker = Tracker::new(
                &config.tracking_for(&camera),
                config.species.max_crops_per_event,
            );
            let max_event = chrono::Duration::milliseconds(
                (config.recording.max_event_minutes * 60_000.0) as i64,
            );
            let mut events = EventManager::new(camera.id.clone(), &camera.labels, &config.motion)
                .with_max_event_length(Some(max_event));
            let wants_objects = camera.labels.iter().any(|l| *l != Label::Motion);
            let send = |ups: Vec<EventUpdate>| {
                ups.into_iter()
                    .all(|u| updates.blocking_send((camera.id.clone(), u)).is_ok())
            };
            // Motion that keeps coming while the detector finds nothing (wind, rain): after
            // `IDLE_AFTER_RUNS` such runs in a row it only looks every `idle_every`.
            let idle_every = chrono::Duration::milliseconds(
                (config.inference.idle_motion_seconds * 1000.0) as i64,
            );
            let mut fruitless = 0u32;
            let mut last_detect: Option<chrono::DateTime<chrono::Utc>> = None;
            let mut last_motion: Option<chrono::DateTime<chrono::Utc>> = None;
            let mut last_now = None;
            let mut fps_window = (std::time::Instant::now(), 0u32);

            while let Some(frame) = frames.blocking_recv() {
                let now = frame.captured_at;
                last_now = Some(now);
                *latest.write().unwrap_or_else(|e| e.into_inner()) = Some(frame.clone());
                let md = motion.get_or_insert_with(|| {
                    MotionDetector::new(
                        &config.motion,
                        frame.width,
                        frame.height,
                        &camera.motion_mask,
                    )
                    .warm_only(camera.warm_motion_only)
                });
                let boxes = md.process(&frame);

                let keep = tracker.keepalive_due(now, keepalive);
                let tiles = options.tiles_until.is_some_and(|until| now < until);
                let regions = if !wants_objects || detector.is_none() {
                    Vec::new()
                } else if tiles {
                    tile_regions(frame.width, frame.height)
                } else {
                    select_regions(
                        &boxes,
                        &keep,
                        frame.width,
                        frame.height,
                        model_size,
                        config.inference.max_regions_per_frame,
                    )
                };
                if !boxes.is_empty() {
                    last_motion = Some(now);
                } else if last_motion.is_none_or(|t| now - t > IDLE_FORGET) {
                    fruitless = 0;
                }
                let idle = !options.background
                    && !tiles
                    && keep.is_empty()
                    && fruitless >= IDLE_AFTER_RUNS
                    && last_detect.is_some_and(|t| now - t < idle_every);
                let regions = if idle { Vec::new() } else { regions };
                let detecting = detector.is_some() && !regions.is_empty();
                let mut dropped = false;
                let track_events = match (&detector, regions.is_empty()) {
                    (Some(pool), false) => {
                        last_detect = Some(now);
                        let (tx, rx) = oneshot::channel();
                        let job = DetectJob {
                            frame: frame.clone(),
                            regions,
                            reply: tx,
                        };
                        if options.background {
                            pool.submit_background(job);
                        } else {
                            pool.submit(job);
                        }
                        match rx.blocking_recv() {
                            Ok(result) if !result.dropped => {
                                if result.detections.is_empty() && keep.is_empty() {
                                    fruitless = fruitless.saturating_add(1);
                                } else {
                                    fruitless = 0;
                                }
                                tracker.update(now, &frame, &result.detections)
                            }
                            _ => {
                                dropped = true;
                                tracker.tick(now)
                            }
                        }
                    }
                    _ => tracker.tick(now),
                };
                {
                    let mut s = stats.write().unwrap_or_else(|e| e.into_inner());
                    s.analysed_frames += 1;
                    s.motion_frames += u64::from(!boxes.is_empty());
                    s.motion_fraction = md.last_changed_fraction();
                    s.last_frame_at = Some(now);
                    s.detect_jobs += u64::from(detecting);
                    s.detect_dropped += u64::from(dropped);
                    fps_window.1 += 1;
                    let elapsed = fps_window.0.elapsed().as_secs_f32();
                    if elapsed >= 5.0 {
                        s.analysed_fps = fps_window.1 as f32 / elapsed;
                        fps_window = (std::time::Instant::now(), 0);
                    }
                }
                let mut ups = events.on_track_events(track_events);
                ups.extend(events.on_motion(now, &frame, &boxes, tracker.has_confirmed()));
                if !send(ups) {
                    return;
                }
            }

            // The stream ended (shutdown or end of file): close every open event.
            let mut ups = events.on_track_events(tracker.finish());
            if let Some(now) = last_now {
                ups.extend(events.end_motion(now));
            }
            send(ups);
        })
}

/// Runs a recorded clip through the same decoder and analysis as a live camera, with frames
/// timed from `start`. Returns every event update (in order) and the first decoded frame. Used
/// by the Hub importer and by `zoologist replay`.
pub fn analyse_recording(
    config: &Arc<Config>,
    camera: &CameraConfig,
    detector: &DetectorHandle,
    info: zoologist_video::stream::StreamInfo,
    samples: Vec<zoologist_video::mp4w::Sample>,
    start: chrono::DateTime<chrono::Utc>,
    options: AnalysisOptions,
) -> std::io::Result<(Vec<EventUpdate>, Option<Frame>)> {
    use zoologist_video::decode::DecodeWorker;
    use zoologist_video::stream::{AccessUnit, StreamItem};

    let (frames_tx, frames_rx) = mpsc::channel::<Frame>(4);
    let (local_tx, mut local_rx) = mpsc::channel::<CameraUpdate>(64);
    let analysis = spawn_analysis(
        camera.clone(),
        config.clone(),
        frames_rx,
        Some(detector.clone()),
        local_tx,
        Arc::default(),
        Arc::default(),
        options,
    )?;
    // Decode on a helper thread while this one collects event updates, so neither side can
    // block the other.
    let mut worker = DecodeWorker::new(
        camera.id.clone(),
        crate::tools::decoder_choice(config),
        camera.detect_fps,
    )
    .with_max_width(config.video.analysis_max_width);
    let feeder = std::thread::Builder::new()
        .name(format!("replay-decode-{}", camera.id))
        .spawn(move || {
            let mut first = None;
            let items =
                std::iter::once(StreamItem::Info(info)).chain(samples.into_iter().map(|s| {
                    StreamItem::Unit(AccessUnit {
                        received_at: start + chrono::Duration::microseconds(s.wall_us),
                        ts_90k: s.wall_us * 9 / 100,
                        is_keyframe: s.is_key,
                        avcc: s.data,
                    })
                }));
            for item in items {
                for frame in worker.handle(item) {
                    if first.is_none() {
                        first = Some(frame.clone());
                    }
                    if frames_tx.blocking_send(frame).is_err() {
                        return first;
                    }
                }
            }
            first
        })?;
    let mut updates = Vec::new();
    while let Some((_, update)) = local_rx.blocking_recv() {
        updates.push(update);
    }
    let first = feeder.join().ok().flatten();
    let _ = analysis.join();
    Ok((updates, first))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detector runs on 20 seconds of a square jumping about (motion in every frame, nothing
    /// the detector knows), with `inference.idle_motion_seconds` set to `idle`.
    fn runs_on_endless_motion(idle: f32) -> Option<u64> {
        let model = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../models/md_v1000_sorrel_320.onnx");
        if !model.exists() {
            eprintln!("skipping: {} not found", model.display());
            return None;
        }
        let mut config: Config = Config::parse(&format!(
            "[models.md-sorrel]\npath = \"{}\"\nkind = \"yolov8\"\nclasses = \"megadetector\"\n\
             input_size = 320\nscore_threshold = 0.35\n",
            model.display()
        ))
        .unwrap();
        config.inference.workers = 1;
        config.inference.idle_motion_seconds = idle;
        let camera: CameraConfig = serde_json::from_value(serde_json::json!({
            "id": "yard", "name": "yard", "labels": ["person", "vehicle", "animal"],
            "detect_url": "rtsp://10.0.0.1/sub", "record_url": "rtsp://10.0.0.1/main"
        }))
        .unwrap();
        let (detector, _workers) = crate::pipeline::load_detector(&config).unwrap();
        let (frames_tx, frames_rx) = mpsc::channel::<Frame>(4);
        let (updates_tx, mut updates_rx) = mpsc::channel::<CameraUpdate>(64);
        let stats = Arc::<RwLock<AnalysisStats>>::default();
        let analysis = spawn_analysis(
            camera,
            Arc::new(config),
            frames_rx,
            Some(detector),
            updates_tx,
            Arc::default(),
            stats.clone(),
            AnalysisOptions::default(),
        )
        .unwrap();
        let drain = std::thread::spawn(move || while updates_rx.blocking_recv().is_some() {});
        let (w, h) = (640u32, 360u32);
        let start = chrono::Utc::now();
        for i in 0..100u32 {
            let mut rgb = vec![70u8; (w * h * 3) as usize];
            // No frame of the first second has the square, so the background settles.
            if i >= 12 {
                let (sx, sy) = (40 + (i * 97) % 480, 40 + (i * 53) % 200);
                for y in sy..sy + 60 {
                    for x in sx..sx + 60 {
                        let p = ((y * w + x) * 3) as usize;
                        rgb[p..p + 3].copy_from_slice(&[230, 230, 230]);
                    }
                }
            }
            let frame = Frame {
                camera_id: "yard".into(),
                seq: i as u64,
                captured_at: start + chrono::Duration::milliseconds(200 * i as i64),
                width: w,
                height: h,
                i420: Arc::new(zoologist_core::yuv::rgb_to_i420(&rgb, w, h).unwrap()),
            };
            frames_tx.blocking_send(frame).unwrap();
        }
        drop(frames_tx);
        analysis.join().unwrap();
        drain.join().unwrap();
        let s = stats.read().unwrap();
        assert!(
            s.motion_frames >= 80,
            "motion in {} frames",
            s.motion_frames
        );
        Some(s.detect_jobs)
    }

    #[test]
    fn endless_motion_with_nothing_in_it_slows_the_detector_down() {
        let (Some(every_frame), Some(idle)) =
            (runs_on_endless_motion(0.0), runs_on_endless_motion(1.0))
        else {
            return;
        };
        // 88 frames with motion: all of them, or 15 and then one a second for 14.6 seconds.
        assert!(every_frame >= 80, "{every_frame} runs with the rule off");
        assert!((25..=35).contains(&idle), "{idle} runs with one a second");
    }
}
