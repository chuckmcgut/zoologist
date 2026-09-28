//! Shared state of a running Zoologist: what the pipeline updates and the API reads.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zoologist_core::Config;
use zoologist_store::{EventRecord, Store};
use zoologist_video::decode::DecodeStats;
use zoologist_video::snapshot::LatestFrame;
use zoologist_video::source::SharedStatus;
use zoologist_vision::pool::DetectorHandle;
use zoologist_vision::species::SpeciesHandle;

/// What happened to an event, for the live stream (SSE) and anyone else listening.
#[derive(Clone, Debug)]
pub enum ApiEvent {
    Started(EventRecord),
    Updated(EventRecord),
    Ended(EventRecord),
}

impl ApiEvent {
    /// The event record carried by this message.
    pub fn record(&self) -> &EventRecord {
        match self {
            ApiEvent::Started(r) | ApiEvent::Updated(r) | ApiEvent::Ended(r) => r,
        }
    }
}

/// Analysis counters of one camera.
#[derive(Clone, Debug, Default)]
pub struct AnalysisStats {
    pub analysed_frames: u64,
    pub motion_frames: u64,
    pub detect_jobs: u64,
    pub detect_dropped: u64,
    pub last_frame_at: Option<DateTime<Utc>>,
    /// Changed fraction of the last frame (0..1).
    pub motion_fraction: f32,
    /// Frames analysed per second, measured over a few seconds.
    pub analysed_fps: f32,
}

/// Disk space used by recordings and clips, refreshed in the background (walking the
/// directories is too slow to do per request).
#[derive(Clone, Debug, Default)]
pub struct DiskUsage {
    pub recordings_bytes: u64,
    pub clips_bytes: u64,
    /// Free space on the data directory's file system.
    pub free_bytes: u64,
    pub measured_at: Option<DateTime<Utc>>,
}

/// Live status of one camera.
#[derive(Clone)]
pub struct CameraRuntime {
    pub id: String,
    pub name: String,
    pub detect: SharedStatus,
    pub record: SharedStatus,
    pub decode: Arc<RwLock<DecodeStats>>,
    pub analysis: Arc<RwLock<AnalysisStats>>,
    pub latest: LatestFrame,
    /// End of the newest recorded segment.
    pub last_segment_at: Arc<RwLock<Option<DateTime<Utc>>>>,
    /// The camera's video for live view (the record stream, or the detect stream when the
    /// camera does not record).
    pub live: Arc<crate::live::LiveFeed>,
}

/// Everything the API and the pipeline share. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub data_dir: PathBuf,
    pub store: Store,
    pub detector: Option<DetectorHandle>,
    /// Key of the detector model in `[models.*]`.
    pub detector_id: String,
    pub species: Option<SpeciesHandle>,
    /// Why the species classifier is not running although it is enabled (shown on the dashboard).
    pub species_problem: Option<String>,
    pub events: broadcast::Sender<ApiEvent>,
    pub cameras: Vec<CameraRuntime>,
    /// Reolink Hub importers.
    pub hubs: Vec<crate::hub_import::HubRuntime>,
    pub started_at: DateTime<Utc>,
    pub disk: Arc<RwLock<DiskUsage>>,
    /// Cancelled when Zoologist shuts down: sources stop and live API streams end.
    pub shutdown: CancellationToken,
}

impl AppState {
    /// State without cameras or models, for tests and tools that only need the store.
    pub fn without_pipeline(config: Config, store: Store) -> AppState {
        let (events, _) = broadcast::channel(256);
        AppState {
            data_dir: config.server.data_dir.clone(),
            detector_id: config.inference.detector.clone(),
            config: Arc::new(config),
            store,
            detector: None,
            species: None,
            species_problem: None,
            events,
            cameras: Vec::new(),
            hubs: Vec::new(),
            started_at: Utc::now(),
            disk: Arc::default(),
            shutdown: CancellationToken::new(),
        }
    }

    /// The live status of camera `id`, if it is running.
    pub fn camera(&self, id: &str) -> Option<&CameraRuntime> {
        self.cameras.iter().find(|c| c.id == id)
    }

    /// Sends an event to live listeners (ignored when nobody listens).
    pub fn publish(&self, event: ApiEvent) {
        let _ = self.events.send(event);
    }
}
