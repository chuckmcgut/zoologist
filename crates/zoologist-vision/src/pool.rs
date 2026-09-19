//! The shared detector pool (plan Step 4.3).
//!
//! A few worker threads share one loaded model and serve every camera. Live camera jobs always
//! go first; Reolink Hub imports use a separate background queue that workers only read when
//! nothing live is waiting. When the live queue is full, the oldest job is dropped: a fresh
//! frame is worth more than a stale one.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use tokio::sync::oneshot;
use zoologist_core::yuv::{PixelRect, RgbCropper};
use zoologist_core::{BBox, Detection, Frame};

use crate::detector::{DetectorError, DetectorModel, NMS_IOU};
use crate::regions::region_to_frame;

/// Something that finds objects in a square RGB image. Implemented by [`DetectorModel`]; tests
/// use fakes.
pub trait ObjectDetector: Send + Sync + 'static {
    /// Side of the square input.
    fn input_size(&self) -> u32;
    /// Detections with boxes normalised to the input square.
    fn detect(&self, rgb: &[u8]) -> Result<Vec<Detection>, DetectorError>;
}

impl ObjectDetector for DetectorModel {
    fn input_size(&self) -> u32 {
        DetectorModel::input_size(self)
    }
    fn detect(&self, rgb: &[u8]) -> Result<Vec<Detection>, DetectorError> {
        DetectorModel::detect(self, rgb)
    }
}

/// Work for the pool: run the detector on some regions of a frame.
pub struct DetectJob {
    pub frame: Frame,
    pub regions: Vec<PixelRect>,
    pub reply: oneshot::Sender<DetectResult>,
}

/// The answer to a [`DetectJob`].
#[derive(Clone, Debug, Default)]
pub struct DetectResult {
    /// Boxes normalised to the whole frame; duplicates across regions removed.
    pub detections: Vec<Detection>,
    /// Time spent running the job (all regions), milliseconds.
    pub infer_ms: f32,
    /// `true` if the job was dropped because the pool was behind.
    pub dropped: bool,
}

/// Pool statistics for the health endpoint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PoolStats {
    pub workers: usize,
    pub busy_workers: usize,
    pub queue_depth: usize,
    pub background_depth: usize,
    pub completed: u64,
    pub mean_infer_ms: f32,
    pub p95_infer_ms: f32,
    /// Dropped live jobs per camera.
    pub drops: HashMap<String, u64>,
}

/// Recent job times kept for the statistics.
const TIMING_WINDOW: usize = 200;

struct Queues {
    live: VecDeque<DetectJob>,
    background: VecDeque<DetectJob>,
}

struct Shared {
    queues: Mutex<Queues>,
    work: Condvar,
    space: Condvar,
    capacity: usize,
    stop: AtomicBool,
    busy: AtomicUsize,
    workers: usize,
    stats: Mutex<(u64, VecDeque<f32>, HashMap<String, u64>)>,
}

impl Shared {
    fn queues(&self) -> MutexGuard<'_, Queues> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Handle for submitting jobs. Cheap to clone.
#[derive(Clone)]
pub struct DetectorHandle {
    shared: Arc<Shared>,
}

/// Starts `workers` threads sharing `detector`. Each queue holds at most `capacity` jobs.
pub fn spawn_detector_pool(
    detector: Arc<dyn ObjectDetector>,
    workers: usize,
    capacity: usize,
) -> std::io::Result<(DetectorHandle, Vec<JoinHandle<()>>)> {
    let shared = Arc::new(Shared {
        queues: Mutex::new(Queues {
            live: VecDeque::new(),
            background: VecDeque::new(),
        }),
        work: Condvar::new(),
        space: Condvar::new(),
        capacity: capacity.max(1),
        stop: AtomicBool::new(false),
        busy: AtomicUsize::new(0),
        workers: workers.max(1),
        stats: Mutex::new((0, VecDeque::new(), HashMap::new())),
    });
    let mut threads = Vec::new();
    for n in 0..workers.max(1) {
        let (shared, detector) = (shared.clone(), detector.clone());
        threads.push(
            std::thread::Builder::new()
                .name(format!("detect-{n}"))
                .spawn(move || worker(&shared, detector.as_ref()))?,
        );
    }
    Ok((DetectorHandle { shared }, threads))
}

impl DetectorHandle {
    /// Queues a live job. If the live queue is full, the oldest job is answered with
    /// `dropped: true` and counted against its camera.
    pub fn submit(&self, job: DetectJob) {
        let dropped = {
            let mut q = self.shared.queues();
            let dropped = (q.live.len() >= self.shared.capacity)
                .then(|| q.live.pop_front())
                .flatten();
            q.live.push_back(job);
            dropped
        };
        self.shared.work.notify_one();
        if let Some(old) = dropped {
            {
                let mut stats = self.shared.stats.lock().unwrap_or_else(|e| e.into_inner());
                *stats.2.entry(old.frame.camera_id.clone()).or_default() += 1;
            }
            let _ = old.reply.send(DetectResult {
                dropped: true,
                ..Default::default()
            });
        }
    }

    /// Queues a background job (Hub imports), waiting while the background queue is full.
    /// Blocking: call from a blocking thread.
    pub fn submit_background(&self, job: DetectJob) {
        let mut q = self.shared.queues();
        while q.background.len() >= self.shared.capacity
            && !self.shared.stop.load(Ordering::Relaxed)
        {
            q = self.shared.space.wait(q).unwrap_or_else(|e| e.into_inner());
        }
        q.background.push_back(job);
        drop(q);
        self.shared.work.notify_one();
    }

    /// Current statistics.
    pub fn stats(&self) -> PoolStats {
        let (live, background) = {
            let q = self.shared.queues();
            (q.live.len(), q.background.len())
        };
        let stats = self.shared.stats.lock().unwrap_or_else(|e| e.into_inner());
        let mut times: Vec<f32> = stats.1.iter().copied().collect();
        times.sort_by(f32::total_cmp);
        let mean = if times.is_empty() {
            0.0
        } else {
            times.iter().sum::<f32>() / times.len() as f32
        };
        let p95 = times
            .get((times.len() * 95 / 100).min(times.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0);
        PoolStats {
            workers: self.shared.workers,
            busy_workers: self.shared.busy.load(Ordering::Relaxed),
            queue_depth: live,
            background_depth: background,
            completed: stats.0,
            mean_infer_ms: mean,
            p95_infer_ms: p95,
            drops: stats.2.clone(),
        }
    }

    /// Stops the workers after their current job. Queued jobs are dropped.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.queues().live.clear();
        self.shared.queues().background.clear();
        self.shared.work.notify_all();
        self.shared.space.notify_all();
    }
}

fn worker(shared: &Shared, detector: &dyn ObjectDetector) {
    let mut cropper = RgbCropper::new();
    let mut rgb = Vec::new();
    loop {
        let job = {
            let mut q = shared.queues();
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(job) = q.live.pop_front() {
                    break job;
                }
                if let Some(job) = q.background.pop_front() {
                    shared.space.notify_one();
                    break job;
                }
                q = shared.work.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        shared.busy.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let detections = run_job(detector, &mut cropper, &mut rgb, &job);
        let infer_ms = started.elapsed().as_secs_f32() * 1000.0;
        shared.busy.fetch_sub(1, Ordering::Relaxed);
        {
            let mut stats = shared.stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.0 += 1;
            stats.1.push_back(infer_ms);
            if stats.1.len() > TIMING_WINDOW {
                stats.1.pop_front();
            }
        }
        let _ = job.reply.send(DetectResult {
            detections,
            infer_ms,
            dropped: false,
        });
    }
}

/// Runs the detector on every region of the job and returns full-frame detections.
fn run_job(
    detector: &dyn ObjectDetector,
    cropper: &mut RgbCropper,
    rgb: &mut Vec<u8>,
    job: &DetectJob,
) -> Vec<Detection> {
    let size = detector.input_size();
    let (fw, fh) = (job.frame.width, job.frame.height);
    let mut all = Vec::new();
    for region in &job.regions {
        let region = region.aligned(fw, fh);
        let result = if region.w == region.h {
            cropper
                .crop(&job.frame, region, size, rgb)
                .map_err(|e| e.to_string())
                .and_then(|()| detector.detect(rgb).map_err(|e| e.to_string()))
                .map(|dets| {
                    dets.into_iter()
                        .map(|d| Detection {
                            bbox: region_to_frame(&d.bbox, &region, fw, fh),
                            ..d
                        })
                        .collect::<Vec<_>>()
                })
        } else {
            cropper
                .crop_letterboxed(&job.frame, region, size, rgb)
                .map_err(|e| e.to_string())
                .and_then(|lb| {
                    let dets = detector.detect(rgb).map_err(|e| e.to_string())?;
                    Ok(dets
                        .into_iter()
                        .map(|d| {
                            let s = size as f32;
                            let (x1, y1) = lb.to_source(d.bbox.x1 * s, d.bbox.y1 * s);
                            let (x2, y2) = lb.to_source(d.bbox.x2 * s, d.bbox.y2 * s);
                            let (rx, ry) = (region.x as f32, region.y as f32);
                            let bbox = BBox::new(
                                (rx + x1) / fw as f32,
                                (ry + y1) / fh as f32,
                                (rx + x2) / fw as f32,
                                (ry + y2) / fh as f32,
                            )
                            .clamp();
                            Detection { bbox, ..d }
                        })
                        .collect::<Vec<_>>())
                })
        };
        match result {
            Ok(dets) => all.extend(dets),
            Err(e) => tracing::warn!(camera = %job.frame.camera_id, "detection failed: {e}"),
        }
    }
    merge_regions(all)
}

/// Same-label boxes whose overlap covers more than this share of the smaller box are one
/// object (e.g. cut in two by the edge of a region).
const CONTAINED: f32 = 0.6;
/// Boxes of different labels overlapping this much are one object seen as two classes.
const SAME_PLACE_IOU: f32 = 0.7;

fn intersection(a: &BBox, b: &BBox) -> f32 {
    let w = (a.x2.min(b.x2) - a.x1.max(b.x1)).max(0.0);
    let h = (a.y2.min(b.y2) - a.y1.max(b.y1)).max(0.0);
    w * h
}

/// Removes duplicates of the same object found in overlapping regions: near-identical boxes,
/// a partial box of an object cut by a region's edge (merged into the other), and the
/// weaker of two labels given to the same place.
fn merge_regions(mut dets: Vec<Detection>) -> Vec<Detection> {
    dets.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    'next: for d in dets {
        for k in kept.iter_mut() {
            if k.label != d.label {
                if k.bbox.iou(&d.bbox) > SAME_PLACE_IOU {
                    continue 'next;
                }
                continue;
            }
            let smaller = k.bbox.area().min(d.bbox.area()).max(f32::EPSILON);
            if k.bbox.iou(&d.bbox) > NMS_IOU || intersection(&k.bbox, &d.bbox) / smaller > CONTAINED
            {
                k.bbox = k.bbox.union(&d.bbox);
                continue 'next;
            }
        }
        kept.push(d);
    }
    kept
}

#[cfg(test)]
mod merge_tests {
    use zoologist_core::Label;

    use super::*;

    fn det(label: Label, score: f32, b: (f32, f32, f32, f32)) -> Detection {
        Detection {
            label,
            raw_class: label.as_str().into(),
            score,
            bbox: BBox::new(b.0, b.1, b.2, b.3),
        }
    }

    #[test]
    fn partial_boxes_from_overlapping_tiles_become_one() {
        // A car across a tile edge: whole in one tile, cut in half in the next.
        let whole = det(Label::Vehicle, 0.9, (0.20, 0.1, 0.60, 0.9));
        let half = det(Label::Vehicle, 0.8, (0.40, 0.1, 0.60, 0.9));
        let other = det(Label::Vehicle, 0.7, (0.75, 0.1, 0.95, 0.9));
        let out = merge_regions(vec![half, other.clone(), whole.clone()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].bbox, whole.bbox);
        assert_eq!(out[1].bbox, other.bbox);
    }

    #[test]
    fn one_object_with_two_labels_keeps_the_stronger() {
        let person = det(Label::Person, 0.95, (0.6, 0.2, 0.8, 0.9));
        let animal = det(Label::Animal, 0.5, (0.61, 0.21, 0.8, 0.9));
        let out = merge_regions(vec![animal, person.clone()]);
        assert_eq!(out, vec![person]);
        // Different places: both stay.
        let dog = det(Label::Animal, 0.5, (0.1, 0.5, 0.2, 0.9));
        assert_eq!(
            merge_regions(vec![dog, det(Label::Person, 0.9, (0.6, 0.2, 0.8, 0.9))]).len(),
            2
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use zoologist_core::Label;

    use super::*;

    /// Finds one "animal" in the middle of every input, after sleeping `delay`.
    struct Fake {
        delay: Duration,
    }

    impl ObjectDetector for Fake {
        fn input_size(&self) -> u32 {
            64
        }
        fn detect(&self, rgb: &[u8]) -> Result<Vec<Detection>, DetectorError> {
            assert_eq!(rgb.len(), 64 * 64 * 3);
            std::thread::sleep(self.delay);
            Ok(vec![Detection {
                label: Label::Animal,
                raw_class: "animal".into(),
                score: 0.9,
                bbox: BBox::new(0.25, 0.25, 0.75, 0.75),
            }])
        }
    }

    fn frame(camera: &str) -> Frame {
        Frame {
            camera_id: camera.into(),
            seq: 0,
            captured_at: Utc::now(),
            width: 640,
            height: 360,
            i420: Arc::new(vec![128; Frame::i420_len(640, 360)]),
        }
    }

    fn job(camera: &str, regions: Vec<PixelRect>) -> (DetectJob, oneshot::Receiver<DetectResult>) {
        let (tx, rx) = oneshot::channel();
        (
            DetectJob {
                frame: frame(camera),
                regions,
                reply: tx,
            },
            rx,
        )
    }

    fn square(x: u32, y: u32, side: u32) -> PixelRect {
        PixelRect {
            x,
            y,
            w: side,
            h: side,
        }
    }

    #[test]
    fn detections_are_mapped_back_to_the_frame() {
        let (handle, _threads) = spawn_detector_pool(
            Arc::new(Fake {
                delay: Duration::ZERO,
            }),
            1,
            4,
        )
        .unwrap();
        let (j, rx) = job("cam", vec![square(100, 20, 320)]);
        handle.submit(j);
        let result = rx.blocking_recv().unwrap();
        assert!(!result.dropped);
        let b = result.detections[0].bbox;
        // Middle half of the 320 px square at (100, 20): x 180..340, y 100..260.
        assert!((b.x1 * 640.0 - 180.0).abs() < 0.5 && (b.x2 * 640.0 - 340.0).abs() < 0.5);
        assert!((b.y1 * 360.0 - 100.0).abs() < 0.5 && (b.y2 * 360.0 - 260.0).abs() < 0.5);
        handle.shutdown();
    }

    #[test]
    fn whole_frame_regions_are_letterboxed_and_mapped_back() {
        let (handle, _threads) = spawn_detector_pool(
            Arc::new(Fake {
                delay: Duration::ZERO,
            }),
            1,
            4,
        )
        .unwrap();
        let (j, rx) = job(
            "cam",
            vec![PixelRect {
                x: 0,
                y: 0,
                w: 640,
                h: 360,
            }],
        );
        handle.submit(j);
        let b = rx.blocking_recv().unwrap().detections[0].bbox;
        // 64 px input: the 640×360 frame is scaled by 0.1 to 64×36 with 14 px bands above/below.
        // The fake box (16..48 in both axes) maps to x 160..480 and y (16-14)/0.1 = 20 .. 340.
        assert!(
            (b.x1 * 640.0 - 160.0).abs() < 1.0 && (b.x2 * 640.0 - 480.0).abs() < 1.0,
            "{b:?}"
        );
        assert!(
            (b.y1 * 360.0 - 20.0).abs() < 1.0 && (b.y2 * 360.0 - 340.0).abs() < 1.0,
            "{b:?}"
        );
        handle.shutdown();
    }

    #[test]
    fn duplicates_from_overlapping_regions_are_merged() {
        let (handle, _threads) = spawn_detector_pool(
            Arc::new(Fake {
                delay: Duration::ZERO,
            }),
            1,
            4,
        )
        .unwrap();
        let (j, rx) = job("cam", vec![square(100, 20, 320), square(110, 20, 320)]);
        handle.submit(j);
        assert_eq!(rx.blocking_recv().unwrap().detections.len(), 1);
        handle.shutdown();
    }

    #[test]
    fn overload_drops_the_oldest_and_every_job_gets_an_answer() {
        let (handle, _threads) = spawn_detector_pool(
            Arc::new(Fake {
                delay: Duration::from_millis(50),
            }),
            2,
            3,
        )
        .unwrap();
        let mut replies = Vec::new();
        // 3 cameras × 10 jobs, much faster than 2 workers at 50 ms can handle.
        for i in 0..30 {
            let (j, rx) = job(["a", "b", "c"][i % 3], vec![square(0, 0, 64)]);
            handle.submit(j);
            assert!(handle.stats().queue_depth <= 3);
            replies.push(rx);
            std::thread::sleep(Duration::from_millis(3));
        }
        let results: Vec<DetectResult> = replies
            .into_iter()
            .map(|rx| rx.blocking_recv().unwrap())
            .collect();
        let dropped = results.iter().filter(|r| r.dropped).count();
        assert!(dropped > 0, "expected drops");
        assert!(results.iter().any(|r| !r.dropped));
        let stats = handle.stats();
        assert_eq!(stats.drops.values().sum::<u64>(), dropped as u64);
        assert_eq!(stats.completed as usize, 30 - dropped);
        assert!(stats.mean_infer_ms >= 45.0, "{stats:?}");
        handle.shutdown();
    }

    #[test]
    fn live_jobs_go_before_background_jobs() {
        let (handle, _threads) = spawn_detector_pool(
            Arc::new(Fake {
                delay: Duration::from_millis(30),
            }),
            1,
            10,
        )
        .unwrap();
        // Keep the single worker busy, then queue background work, then one live job.
        let (first, first_rx) = job("cam", vec![square(0, 0, 64)]);
        handle.submit(first);
        std::thread::sleep(Duration::from_millis(5));
        let mut background = Vec::new();
        for _ in 0..3 {
            let (j, rx) = job("hub-cam", vec![square(0, 0, 64)]);
            handle.submit_background(j);
            background.push(rx);
        }
        let (live, live_rx) = job("cam", vec![square(0, 0, 64)]);
        handle.submit(live);
        first_rx.blocking_recv().unwrap();
        let live_done = Instant::now();
        live_rx.blocking_recv().unwrap();
        let live_at = live_done.elapsed();
        for rx in background {
            assert!(
                !rx.blocking_recv().unwrap().dropped,
                "background jobs are never dropped"
            );
        }
        // The live job ran right after the first one, not after the three background jobs.
        assert!(live_at < Duration::from_millis(60), "{live_at:?}");
        handle.shutdown();
    }
}
