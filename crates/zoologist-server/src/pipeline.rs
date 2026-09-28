//! Starting and stopping everything `zoologist run` does (plan Step 7.1).
//!
//! Per `kind = "stream"` camera:
//!
//! ```text
//! detect source ─► decode thread ─► analysis thread ─┐
//!                                                     ├─► writer task ─► store, pictures,
//! record source ─► recorder ─► segment task ─► store  │                  clips, species
//!                                                     │
//!                          (other cameras) ──────────┘
//! ```
//!
//! Shutdown runs front to back: the sources stop, every channel closes behind them, each
//! analysis thread ends its open events, and the writer finishes pending clips and species
//! before [`Pipeline::shutdown`] returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zoologist_core::Config;
use zoologist_core::config::CameraKind;
use zoologist_store::{SegmentRecord, Store};
use zoologist_video::decode::spawn_decode_worker;
use zoologist_video::file_source::spawn_file_stream;
use zoologist_video::recorder::spawn_recorder;
use zoologist_video::snapshot::spawn_latest_writer;
use zoologist_video::source::{SharedStatus, StreamRole, spawn_source};
use zoologist_vision::detector::DetectorModel;
use zoologist_vision::pool::{DetectorHandle, spawn_detector_pool};
use zoologist_vision::species::{SpeciesHandle, SpeciesModel, spawn_species_pool};

use crate::analysis::{AnalysisOptions, spawn_analysis};
use crate::app::{AppState, CameraRuntime};
use crate::hub_import::{HubRuntime, Importer};
use crate::live::{split, tee};
use crate::tools::decoder_choice;
use crate::writer::{HubClips, RecorderDone, run_writer};

/// Decoded frames waiting for analysis, per camera. Small on purpose: stale frames are useless.
const FRAME_QUEUE: usize = 2;
/// Access units between a source and its consumer.
const UNIT_QUEUE: usize = 256;
/// Species jobs waiting for a worker; more are dropped.
const SPECIES_QUEUE: usize = 16;
/// How often the newest frame of each camera is written for the UI.
const LATEST_INTERVAL: Duration = Duration::from_secs(2);
/// How often pipeline counters are logged.
const STATS_INTERVAL: Duration = Duration::from_secs(60);

/// Options of `zoologist run`.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Play `file://` sources as fast as analysis allows, all starting at the same instant,
    /// and stop once they have ended. For offline tests.
    pub fast_files: bool,
    /// Start even when the CPU lacks AVX2 (inference is then several times slower).
    pub allow_slow_cpu: bool,
}

/// A running pipeline.
pub struct Pipeline {
    pub app: AppState,
    cancel: CancellationToken,
    sources: Vec<JoinHandle<()>>,
    analysis: Vec<std::thread::JoinHandle<()>>,
    segment_tasks: Vec<JoinHandle<()>>,
    background: Vec<JoinHandle<()>>,
    detector_threads: Vec<std::thread::JoinHandle<()>>,
    hub_threads: Vec<std::thread::JoinHandle<()>>,
    updates: Option<mpsc::Sender<crate::analysis::CameraUpdate>>,
    writer: JoinHandle<()>,
}

/// Refuses to start on a CPU without AVX2 unless allowed: tract then falls back to generic
/// kernels and the detector is several times slower (e.g. a Proxmox VM whose CPU type is not
/// `host`).
pub fn check_cpu(allow_slow_cpu: bool) -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        let fast = std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma");
        if !fast {
            if !allow_slow_cpu {
                bail!(
                    "this CPU does not expose AVX2/FMA, so detection would be very slow. In \
                     Proxmox set the VM's CPU type to `host` (see docs/PROXMOX.md), or pass --allow-slow-cpu"
                );
            }
            tracing::warn!("CPU lacks AVX2/FMA: detection will be slow");
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = allow_slow_cpu;
    Ok(())
}

pub fn load_detector(
    config: &Config,
) -> Result<(DetectorHandle, Vec<std::thread::JoinHandle<()>>)> {
    let key = &config.inference.detector;
    let model_cfg = config
        .models
        .get(key)
        .with_context(|| format!("inference.detector = {key:?} has no [models.{key}] section"))?;
    let model = DetectorModel::load(model_cfg).with_context(|| {
        format!(
            "cannot load detector {key} from {} (run scripts/fetch-models.sh)",
            model_cfg.path.display()
        )
    })?;
    tracing::info!(model = %key, size = model_cfg.input_size, workers = config.inference.workers, "detector loaded");
    Ok(spawn_detector_pool(
        Arc::new(model),
        config.inference.workers,
        config.inference.queue_capacity,
    )?)
}

/// Loads the species classifier. `Ok(None)` when it is disabled; `Err` says why it could not be
/// loaded (also logged). Either way events are then stored without species.
pub fn load_species(config: &Config) -> Result<Option<SpeciesHandle>, String> {
    let cfg = &config.species;
    if !cfg.enabled {
        return Ok(None);
    }
    if !cfg.path.exists() || !cfg.labels.exists() {
        let problem = format!(
            "species model {} not found (run scripts/fetch-models.sh)",
            cfg.path.display()
        );
        tracing::warn!("{problem}: animals will not be named");
        return Err(problem);
    }
    match SpeciesModel::load(cfg, &config.station) {
        Ok(model) => {
            tracing::info!(
                labels = model.rules.labels.len(),
                "species classifier loaded"
            );
            match spawn_species_pool(Arc::new(model), cfg.workers, SPECIES_QUEUE) {
                // The workers stop when the last handle is dropped.
                Ok((handle, _threads)) => Ok(Some(handle)),
                Err(e) => {
                    tracing::warn!("cannot start species workers: {e}");
                    Err(format!("cannot start species workers: {e}"))
                }
            }
        }
        Err(e) => {
            tracing::warn!("cannot load species model: {e}");
            Err(format!("cannot load species model: {e}"))
        }
    }
}

impl Pipeline {
    /// Opens the database, loads the models and starts every enabled camera.
    pub async fn start(config: Config, opts: &RunOptions) -> Result<Pipeline> {
        check_cpu(opts.allow_slow_cpu)?;
        let config = Arc::new(config);
        let data_dir = config.server.data_dir.clone();
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
        let store = Store::open(&data_dir.join("zoologist.redb"), config.station.timezone)
            .context("cannot open the database")?;
        let closed = store.call(|s| s.close_dangling_events()).await?;
        if closed > 0 {
            tracing::info!("closed {closed} event(s) left open by the previous run");
        }

        let (detector, detector_threads) = {
            let config = config.clone();
            tokio::task::spawn_blocking(move || load_detector(&config)).await??
        };
        let species = {
            let config = config.clone();
            tokio::task::spawn_blocking(move || load_species(&config)).await?
        };
        let (species, species_problem) = match species {
            Ok(handle) => (handle, None),
            Err(problem) => (None, Some(problem)),
        };

        let cancel = CancellationToken::new();
        let (updates_tx, updates_rx) = mpsc::channel(256);
        let (events, _) = broadcast::channel(256);
        let file_start = Utc::now();
        let mut sources = Vec::new();
        let mut analysis = Vec::new();
        let mut segment_tasks = Vec::new();
        let mut background = Vec::new();
        let mut cameras = Vec::new();
        let mut recorders = RecorderDone::new();

        for camera in config.enabled_cameras() {
            if camera.kind == CameraKind::HubClips {
                continue; // imported by the Hub's importer thread
            }
            // Detect and record from the same URL: open it once and split it.
            let shared = camera.record
                && camera.detect_url.is_some()
                && camera.detect_url == camera.record_url;
            let record_status = SharedStatus::default();
            let runtime = CameraRuntime {
                id: camera.id.clone(),
                name: camera.name.clone(),
                detect: if shared {
                    record_status.clone()
                } else {
                    SharedStatus::default()
                },
                record: record_status,
                decode: Arc::default(),
                analysis: Arc::default(),
                latest: Arc::default(),
                last_segment_at: Arc::default(),
                live: Arc::default(),
            };
            let start_source = |role, tx, status: SharedStatus| {
                let url = match role {
                    StreamRole::Detect => camera.detect_url.as_deref(),
                    StreamRole::Record => camera.record_url.as_deref(),
                };
                match url {
                    Some(url) if opts.fast_files && url.starts_with("file://") => {
                        Some(spawn_file_stream(
                            url.to_string(),
                            file_start,
                            true,
                            tx,
                            status,
                            cancel.clone(),
                        ))
                    }
                    _ => spawn_source(camera, role, tx, status, cancel.clone()),
                }
            };

            // Analysis: detect stream → decoder → motion, detection, tracking, events.
            let (units_tx, units_rx) = mpsc::channel(UNIT_QUEUE);
            let (frames_tx, frames_rx) = mpsc::channel(FRAME_QUEUE);
            // Live view shows the record stream; a camera that does not record shows its
            // detect stream instead.
            let detect_tx = if camera.record {
                units_tx
            } else {
                let (tee_tx, tee_rx) = mpsc::channel(UNIT_QUEUE);
                background.push(tee(tee_rx, units_tx, runtime.live.clone()));
                tee_tx
            };
            if !shared {
                sources.extend(start_source(
                    StreamRole::Detect,
                    detect_tx.clone(),
                    runtime.detect.clone(),
                ));
            }
            spawn_decode_worker(
                camera.id.clone(),
                decoder_choice(&config),
                camera.detect_fps,
                units_rx,
                frames_tx,
                runtime.decode.clone(),
                opts.fast_files,
                config.video.analysis_max_width,
            )?;
            analysis.push(spawn_analysis(
                camera.clone(),
                config.clone(),
                frames_rx,
                Some(detector.clone()),
                updates_tx.clone(),
                runtime.latest.clone(),
                runtime.analysis.clone(),
                AnalysisOptions::default(),
            )?);
            background.push(spawn_latest_writer(
                runtime.latest.clone(),
                data_dir.join("latest").join(format!("{}.jpg", camera.id)),
                LATEST_INTERVAL,
                cancel.clone(),
            ));

            // Recording: main stream → segments on disk → segment index in the store.
            if camera.record {
                let (units_tx, units_rx) = mpsc::channel(UNIT_QUEUE);
                let (written_tx, mut written_rx) = mpsc::channel(16);
                let (tee_tx, tee_rx) = mpsc::channel(UNIT_QUEUE);
                background.push(if shared {
                    split(
                        tee_rx,
                        units_tx,
                        detect_tx.clone(),
                        runtime.live.clone(),
                        opts.fast_files,
                    )
                } else {
                    tee(tee_rx, units_tx, runtime.live.clone())
                });
                sources.extend(start_source(
                    StreamRole::Record,
                    tee_tx,
                    runtime.record.clone(),
                ));
                spawn_recorder(
                    camera.id.clone(),
                    data_dir.clone(),
                    config.recording.segment_seconds,
                    units_rx,
                    written_tx,
                );
                let done = Arc::new(AtomicBool::new(false));
                recorders.insert(camera.id.clone(), done.clone());
                let (store, id) = (store.clone(), camera.id.clone());
                let last_segment_at = runtime.last_segment_at.clone();
                segment_tasks.push(tokio::spawn(async move {
                    while let Some(w) = written_rx.recv().await {
                        let record = SegmentRecord {
                            path: w.path,
                            index_path: w.index_path,
                            started_at: w.started_at,
                            ended_at: w.ended_at,
                            bytes: w.bytes,
                        };
                        *last_segment_at.write().unwrap_or_else(|e| e.into_inner()) =
                            Some(record.ended_at);
                        let cam = id.clone();
                        if let Err(e) = store.call(move |s| s.insert_segment(&cam, &record)).await {
                            tracing::warn!(camera = %id, "cannot store segment: {e}");
                        }
                    }
                    done.store(true, Ordering::Relaxed);
                }));
            }
            drop(detect_tx);
            tracing::info!(camera = %camera.id, record = camera.record, shared, "camera started");
            cameras.push(runtime);
        }
        let hub_cameras = config
            .enabled_cameras()
            .filter(|c| c.kind == CameraKind::HubClips)
            .count();
        if cameras.is_empty() && hub_cameras == 0 {
            bail!("no enabled cameras in the config");
        }
        let hub_clips = HubClips::default();
        let detector_handle = detector.clone();
        let hubs: Vec<HubRuntime> = config
            .reolink_hubs
            .iter()
            .map(|h| HubRuntime {
                id: h.id.clone(),
                status: Arc::default(),
            })
            .collect();

        let app = AppState {
            config: config.clone(),
            data_dir,
            store,
            detector: Some(detector.clone()),
            detector_id: config.inference.detector.clone(),
            species,
            species_problem,
            events,
            cameras,
            hubs: hubs.clone(),
            started_at: Utc::now(),
            disk: Arc::default(),
            shutdown: cancel.clone(),
        };
        let writer = tokio::spawn(run_writer(
            app.clone(),
            updates_rx,
            recorders,
            hub_clips.clone(),
        ));
        background.push(tokio::spawn(log_stats(app.clone(), cancel.clone())));
        let mut hub_threads = Vec::new();
        for (hub, runtime) in config.reolink_hubs.iter().zip(&hubs) {
            let importer = Importer {
                hub: hub.clone(),
                config: config.clone(),
                data_dir: app.data_dir.clone(),
                store: app.store.clone(),
                detector: detector_handle.clone(),
                updates: updates_tx.clone(),
                hub_clips: hub_clips.clone(),
                status: runtime.status.clone(),
                cancel: cancel.clone(),
                app: Some(app.clone()),
            };
            hub_threads.push(
                std::thread::Builder::new()
                    .name(format!("hub-{}", hub.id))
                    .spawn(move || importer.run())?,
            );
        }
        background.push(tokio::spawn(crate::disk::measure_disk_forever(
            app.clone(),
            cancel.clone(),
        )));
        if !opts.fast_files {
            background.push(tokio::spawn(crate::janitor::janitor_forever(
                app.clone(),
                cancel.clone(),
            )));
        }
        Ok(Pipeline {
            app,
            cancel,
            sources,
            analysis,
            segment_tasks,
            background,
            detector_threads,
            hub_threads,
            updates: Some(updates_tx),
            writer,
        })
    }

    /// Waits until every source has ended by itself (file sources). Live sources never do.
    pub async fn sources_ended(&mut self) {
        for source in &mut self.sources {
            let _ = source.await;
        }
        self.sources.clear();
    }

    /// Stops the sources and waits until every event, segment, clip and species answer is
    /// written.
    pub async fn shutdown(mut self) {
        tracing::info!("shutting down");
        self.cancel.cancel();
        for source in self.sources.drain(..) {
            let _ = source.await;
        }
        // Sources gone → decoders and analysis threads drain and end their events. Hub
        // importers finish the recording they are on.
        let mut analysis = std::mem::take(&mut self.analysis);
        analysis.append(&mut self.hub_threads);
        let _ = tokio::task::spawn_blocking(move || {
            for t in analysis {
                let _ = t.join();
            }
        })
        .await;
        if let Some(pool) = &self.app.detector {
            pool.shutdown();
        }
        for task in self.segment_tasks.drain(..) {
            let _ = task.await;
        }
        drop(self.updates.take());
        let _ = (&mut self.writer).await;
        for task in self.background.drain(..) {
            task.abort();
        }
        let threads = std::mem::take(&mut self.detector_threads);
        let _ = tokio::task::spawn_blocking(move || {
            for t in threads {
                let _ = t.join();
            }
        })
        .await;
        tracing::info!("stopped");
    }
}

/// Logs per-camera counters every [`STATS_INTERVAL`].
async fn log_stats(app: AppState, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(STATS_INTERVAL) => {}
        }
        for cam in &app.cameras {
            let detect = cam.detect.read().unwrap_or_else(|e| e.into_inner()).clone();
            let decode = cam.decode.read().unwrap_or_else(|e| e.into_inner()).clone();
            let analysis = cam
                .analysis
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            tracing::info!(
                camera = %cam.id,
                state = ?detect.state,
                fps = detect.fps_measured,
                decoded = decode.decoded,
                frames_dropped = decode.dropped,
                decode_ms = decode.mean_decode_ms,
                analysed = analysis.analysed_frames,
                motion = analysis.motion_frames,
                detect_dropped = analysis.detect_dropped,
                "camera stats"
            );
        }
        if let Some(pool) = &app.detector {
            tracing::info!(stats = ?pool.stats(), "detector stats");
        }
        if let Some(species) = &app.species {
            tracing::info!(
                queued = species.queue_depth(),
                dropped = species.dropped(),
                "species stats"
            );
        }
    }
}

/// `zoologist run`: starts the pipeline and the web server and runs until Ctrl-C / SIGTERM
/// (or, with `--fast-files`, until the files have been analysed).
pub async fn run(config: Config, opts: RunOptions) -> Result<()> {
    let mut pipeline = Pipeline::start(config, &opts).await?;
    let mut server = tokio::spawn(crate::api::serve(pipeline.app.clone()));
    tokio::select! {
        _ = pipeline.sources_ended(), if opts.fast_files => tracing::info!("all file sources ended"),
        _ = shutdown_signal() => {}
        result = &mut server => {
            // The server stopped by itself: it could not bind, so stop everything.
            pipeline.shutdown().await;
            return result?;
        }
    }
    pipeline.shutdown().await;
    match tokio::time::timeout(Duration::from_secs(5), server).await {
        Ok(result) => result?,
        Err(_) => Ok(()),
    }
}

/// Resolves on Ctrl-C or (on Unix) SIGTERM, which `docker stop` sends.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
