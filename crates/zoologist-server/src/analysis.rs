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
}

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
            let model_size = config
                .models
                .get(&config.inference.detector)
                .map_or(320, |m| m.input_size);
            let keepalive = chrono::Duration::milliseconds(
                (config.inference.keepalive_seconds * 1000.0) as i64,
            );
            let mut motion: Option<MotionDetector> = None;
            let mut tracker = Tracker::new(&config.tracking, config.species.max_crops_per_event);
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
                let mut dropped = false;
                let track_events = match (&detector, regions.is_empty()) {
                    (Some(pool), false) => {
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
                    if !keep.is_empty() || !boxes.is_empty() {
                        s.detect_jobs += 1;
                    }
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
