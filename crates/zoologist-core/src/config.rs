//! Configuration schema (plan §3.3), loading and validation.
//!
//! [`Config::load`] parses the TOML file and then runs [`Config::validate`], which collects
//! every problem instead of stopping at the first one. Checks that need the filesystem (model
//! files, the ffmpeg executable) happen when `zoologist run` starts, not here.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::{ConfigError, Label, redact_url};

/// The whole configuration file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub station: StationConfig,
    pub server: ServerConfig,
    pub video: VideoConfig,
    pub inference: InferenceConfig,
    /// Detector models by key; `inference.detector` picks one.
    pub models: BTreeMap<String, ModelConfig>,
    pub species: SpeciesConfig,
    pub motion: MotionConfig,
    pub tracking: TrackingConfig,
    pub recording: RecordingConfig,
    pub retention: RetentionConfig,
    pub reolink_hubs: Vec<HubConfig>,
    pub cameras: Vec<CameraConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StationConfig {
    pub name: String,
    /// IANA time zone for charts, date folders and Reolink Hub times.
    pub timezone: Tz,
    /// ISO-3166 alpha-3 country code for the species geofence, e.g. `"USA"`.
    pub country: Option<String>,
    /// State or province code for the species geofence, e.g. `"NY"`.
    pub admin1_region: Option<String>,
}

impl Default for StationConfig {
    fn default() -> Self {
        Self {
            name: "Zoologist".into(),
            timezone: Tz::UTC,
            country: None,
            admin1_region: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub cors_allow_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 8090)),
            data_dir: PathBuf::from("data"),
            static_dir: PathBuf::from("static"),
            cors_allow_origins: vec!["*".into()],
        }
    }
}

/// Which H.264 decoder to use (decided in plan Step 0.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderKind {
    /// Pure-Rust decoder, in process.
    #[default]
    Rust,
    /// The `ffmpeg` executable as a child process.
    Ffmpeg,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VideoConfig {
    pub decoder: DecoderKind,
    /// Only used when `decoder = "ffmpeg"`.
    pub ffmpeg_path: PathBuf,
    /// Frames wider than this are scaled down before analysis. The detector works on crops,
    /// so its cost barely depends on this; wider keeps distant animals large enough to find
    /// and name (a 1536×432 panorama at 640 wide leaves a fox a few pixels tall).
    pub analysis_max_width: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            decoder: DecoderKind::Rust,
            ffmpeg_path: PathBuf::from("ffmpeg"),
            analysis_max_width: 1536,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct InferenceConfig {
    /// Key into `[models.*]`.
    pub detector: String,
    /// Detector worker threads (each uses about one core while busy).
    pub workers: usize,
    /// Pending detect jobs; the oldest is dropped when full.
    pub queue_capacity: usize,
    pub max_regions_per_frame: usize,
    /// Seconds between detections on a still-active track when there is no motion.
    pub keepalive_seconds: f32,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            detector: "md-sorrel".into(),
            workers: 3,
            queue_capacity: 12,
            max_regions_per_frame: 2,
            keepalive_seconds: 2.0,
        }
    }
}

/// How to decode a detector's raw output tensor (see `docs/MODELS.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorOutput {
    /// `[1, N, 5 + C]`: cx, cy, w, h, objectness, class scores.
    Yolov5,
    /// `[1, 4 + C, N]`: cx, cy, w, h, class scores (no objectness).
    Yolov8,
    /// `[1, N, 6]`: x1, y1, x2, y2, score, class. NMS already applied.
    YoloE2e,
}

/// Which class list a detector was trained on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassSet {
    /// animal, person, vehicle.
    Megadetector,
    /// The 80 COCO classes.
    Coco,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub path: PathBuf,
    pub kind: DetectorOutput,
    pub classes: ClassSet,
    /// Square input size in pixels, a multiple of 32.
    pub input_size: u32,
    pub score_threshold: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeciesModel {
    #[default]
    Speciesnet,
    Bioclip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpeciesConfig {
    pub enabled: bool,
    pub model: SpeciesModel,
    pub path: PathBuf,
    pub labels: PathBuf,
    /// SpeciesNet taxonomy (every genus, family, … in the label format), for roll-ups.
    pub taxonomy: Option<PathBuf>,
    pub geofence: Option<PathBuf>,
    /// BioCLIP only: one species name per line.
    pub species_list: Option<PathBuf>,
    /// BioCLIP only: precomputed, L2-normalised text embeddings.
    pub text_embeddings: Option<PathBuf>,
    pub workers: usize,
    pub max_crops_per_event: usize,
    /// A species is named when its (vote-averaged) probability exceeds 0.8, or exceeds this
    /// while the detector is fairly sure it is an animal. Below that, the result is rolled up
    /// to genus, family, … if the combined probability exceeds this. SpeciesNet uses 0.65.
    pub min_score: f32,
    /// An "animal" that never moved and that the classifier cannot name is stored as motion:
    /// such events are almost always a stump, a rock or a shadow the detector keeps seeing.
    pub still_unnamed_as_motion: bool,
    /// Person events are shown to the classifier too, as a second opinion: when it is sure
    /// nothing is there (an insect in the infrared light, a gas cylinder), they become motion.
    pub check_people: bool,
    /// For live cameras reached through a Reolink Home Hub: while an animal is in view, take a
    /// few full-resolution snapshots of the camera's main stream and name the animal from those
    /// sharper views too.
    pub snapshots: bool,
}

impl Default for SpeciesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: SpeciesModel::Speciesnet,
            path: PathBuf::from("models/speciesnet.onnx"),
            labels: PathBuf::from("models/speciesnet_labels.txt"),
            taxonomy: None,
            geofence: None,
            species_list: None,
            text_embeddings: None,
            workers: 1,
            max_crops_per_event: 3,
            min_score: 0.65,
            still_unnamed_as_motion: true,
            check_people: true,
            snapshots: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MotionConfig {
    /// The Y plane is downscaled to this width before analysis.
    pub analysis_width: u32,
    /// Per-pixel brightness difference (0–255) that counts as change.
    pub threshold: u8,
    /// Minimum changed fraction of the frame for one motion box.
    pub contour_area: f32,
    /// Background learning rate per frame.
    pub frame_alpha: f32,
    /// More than this fraction changing at once (IR switch, lightning) resets the background.
    pub lightning_fraction: f32,
    /// Motion with no object for this long becomes a `motion` event.
    pub motion_event_min_seconds: f32,
    pub motion_event_cooldown_seconds: f32,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            analysis_width: 320,
            threshold: 25,
            contour_area: 0.002,
            frame_alpha: 0.02,
            lightning_fraction: 0.5,
            motion_event_min_seconds: 3.0,
            motion_event_cooldown_seconds: 120.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TrackingConfig {
    pub min_hits: u32,
    pub max_missed_seconds: f32,
    pub iou_match: f32,
    /// Median score over the track needed to become an event.
    pub min_event_score: f32,
    /// An object that has not moved for this long is "parked": its event ends and it starts no
    /// new events until it moves again (a parked car, a chair, a feeder). 0 turns this off.
    pub stationary_seconds: f32,
    /// How long a parked object is remembered after Zoologist stops seeing it, so that finding
    /// it again does not start an event.
    pub stationary_forget_minutes: u32,
    /// Objects of the labels in `require_movement` must move before they become an event: the
    /// centre must travel this fraction of the box's diagonal (or the box must clearly grow or
    /// shrink, keeping its shape). Parked cars that the detector finds again and again never
    /// move. 0 = off.
    pub min_movement: f32,
    /// Labels that must move to become an event. Vehicles by default: an animal may be first
    /// seen standing still, and it should still be reported.
    pub require_movement: Vec<Label>,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            min_hits: 3,
            max_missed_seconds: 5.0,
            iou_match: 0.3,
            min_event_score: 0.55,
            stationary_seconds: 60.0,
            stationary_forget_minutes: 30,
            min_movement: 0.2,
            require_movement: vec![Label::Vehicle],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RecordingConfig {
    pub segment_seconds: u32,
    pub keep_segments_hours: u32,
    pub pre_capture_seconds: f32,
    pub post_capture_seconds: f32,
    /// An event longer than this is ended and, if it goes on, continued as a new event, so no
    /// clip is longer than this (0 = no limit).
    pub max_event_minutes: f32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            segment_seconds: 10,
            keep_segments_hours: 6,
            pre_capture_seconds: 5.0,
            post_capture_seconds: 5.0,
            max_event_minutes: 5.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetentionConfig {
    pub clips_days: u32,
    pub clips_max_total_mb: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            clips_days: 30,
            clips_max_total_mb: 100_000,
        }
    }
}

/// Which Hub recording stream to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubStream {
    #[default]
    Sub,
    Main,
}

/// What to do with a Hub recording in which nothing was detected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoDetection {
    /// Keep it as one `motion` event.
    #[default]
    Motion,
    Discard,
}

/// A Reolink Home Hub whose recordings are imported for battery cameras.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    pub id: String,
    /// Must be `http://` (a pure-Rust build has no TLS client).
    pub url: String,
    pub user: String,
    pub password: String,
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u32,
    #[serde(default = "default_lookback_minutes")]
    pub lookback_minutes: u32,
    /// The recording stream that is analysed (normally the H.264 sub stream).
    #[serde(default)]
    pub analyse_stream: HubStream,
    #[serde(default)]
    pub no_detection: NoDetection,
    /// Clip to keep when the main recording is H.265: `sub` keeps the H.264 sub file instead.
    #[serde(default)]
    pub clip_codec_fallback: HubStream,
}

fn default_poll_seconds() -> u32 {
    30
}

fn default_lookback_minutes() -> u32 {
    30
}

/// How a camera gets into Zoologist (STACKS.md §0.5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraKind {
    /// Continuous live stream (Reolink wired, Wyze RTSP).
    #[default]
    Stream,
    /// Battery camera: recordings imported from a Reolink Home Hub.
    HubClips,
}

/// Stream protocol for `kind = "stream"` cameras.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Rtsp,
    /// Reolink HTTP-FLV.
    Flv,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: CameraKind,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// `kind = "stream"` only.
    pub transport: Option<Transport>,
    /// `kind = "stream"` only: low-resolution substream used for analysis.
    pub detect_url: Option<String>,
    /// `kind = "stream"` only: main stream that is recorded.
    pub record_url: Option<String>,
    #[serde(default = "default_detect_fps")]
    pub detect_fps: u32,
    #[serde(default = "yes")]
    pub record: bool,
    /// `kind = "hub_clips"`: the `[[reolink_hubs]]` id to import from. On a `kind = "stream"`
    /// camera reached through a Hub's RTSP: the Hub whose user and password go into the URLs.
    pub hub: Option<String>,
    /// `kind = "hub_clips"` only: the Hub API channel number (0-based).
    pub channel: Option<u8>,
    /// Which labels create events for this camera.
    #[serde(default = "all_labels")]
    pub labels: Vec<Label>,
    /// Polygons to ignore, each a flat list `[x1, y1, x2, y2, …]` in normalised coordinates.
    #[serde(default)]
    pub motion_mask: Vec<Vec<f32>>,
}

fn yes() -> bool {
    true
}

fn default_detect_fps() -> u32 {
    5
}

fn all_labels() -> Vec<Label> {
    Label::ALL.to_vec()
}

impl Config {
    /// Reads, parses and validates the configuration file.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut config = Config::parse(&text)?;
        config.validate()?;
        config.apply_hub_credentials();
        Ok(config)
    }

    /// Stream cameras reached through a Reolink Hub (`hub = "…"` on a `kind = "stream"` camera)
    /// get the Hub's user and password in their URLs, so the password is written only once.
    /// URLs that already contain credentials are left alone.
    pub fn apply_hub_credentials(&mut self) {
        for cam in &mut self.cameras {
            let Some(hub) = cam
                .hub
                .as_ref()
                .filter(|_| cam.kind == CameraKind::Stream)
                .and_then(|id| self.reolink_hubs.iter().find(|h| &h.id == id))
            else {
                continue;
            };
            for url in [&mut cam.detect_url, &mut cam.record_url]
                .into_iter()
                .flatten()
            {
                if let Some(with) = with_credentials(url, &hub.user, &hub.password) {
                    *url = with;
                }
            }
        }
    }

    /// Parses TOML text without validating it.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Enabled cameras only.
    pub fn enabled_cameras(&self) -> impl Iterator<Item = &CameraConfig> {
        self.cameras.iter().filter(|c| c.enabled)
    }

    /// Checks every rule from plan §3.3 and returns all problems at once.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        let mut err = |msg: String| errors.push(msg);

        // Models and inference.
        if !self.models.contains_key(&self.inference.detector) {
            err(format!(
                "inference.detector = {:?} has no matching [models.{}] section",
                self.inference.detector, self.inference.detector
            ));
        }
        check_range(&mut err, "inference.workers", self.inference.workers, 1, 8);
        check_range(
            &mut err,
            "video.analysis_max_width",
            self.video.analysis_max_width as usize,
            320,
            3840,
        );
        check_range(
            &mut err,
            "inference.queue_capacity",
            self.inference.queue_capacity,
            1,
            1000,
        );
        check_range(
            &mut err,
            "inference.max_regions_per_frame",
            self.inference.max_regions_per_frame,
            1,
            8,
        );
        for (key, model) in &self.models {
            if model.input_size == 0 || model.input_size % 32 != 0 {
                err(format!(
                    "models.{key}.input_size must be a positive multiple of 32, got {}",
                    model.input_size
                ));
            }
            check_unit(
                &mut err,
                &format!("models.{key}.score_threshold"),
                model.score_threshold,
            );
        }

        // Species.
        check_range(&mut err, "species.workers", self.species.workers, 1, 8);
        check_range(
            &mut err,
            "species.max_crops_per_event",
            self.species.max_crops_per_event,
            1,
            10,
        );
        check_unit(&mut err, "species.min_score", self.species.min_score);
        if self.species.enabled
            && self.species.model == SpeciesModel::Bioclip
            && (self.species.species_list.is_none() || self.species.text_embeddings.is_none())
        {
            err("species.model = \"bioclip\" needs species.species_list and species.text_embeddings".into());
        }

        // Motion and tracking.
        if !(64..=1920).contains(&self.motion.analysis_width) {
            err(format!(
                "motion.analysis_width must be 64..=1920, got {}",
                self.motion.analysis_width
            ));
        }
        check_unit(&mut err, "motion.contour_area", self.motion.contour_area);
        check_unit(&mut err, "motion.frame_alpha", self.motion.frame_alpha);
        check_unit(
            &mut err,
            "motion.lightning_fraction",
            self.motion.lightning_fraction,
        );
        check_unit(&mut err, "tracking.iou_match", self.tracking.iou_match);
        check_unit(
            &mut err,
            "tracking.min_event_score",
            self.tracking.min_event_score,
        );
        if !(0.0..=2.0).contains(&self.tracking.min_movement) {
            err("tracking.min_movement must be 0..=2".into());
        }
        if self.tracking.stationary_seconds < 0.0 {
            err("tracking.stationary_seconds must be 0 or more".into());
        }
        if self.tracking.min_hits == 0 {
            err("tracking.min_hits must be at least 1".into());
        }
        if self.recording.segment_seconds < 2 {
            err(format!(
                "recording.segment_seconds must be at least 2, got {}",
                self.recording.segment_seconds
            ));
        }
        let max_event = self.recording.max_event_minutes;
        if max_event < 0.0 {
            err("recording.max_event_minutes must be 0 (no limit) or more".into());
        } else if max_event > 0.0 && max_event < 0.5 {
            err(format!(
                "recording.max_event_minutes must be at least 0.5, got {max_event}"
            ));
        } else if max_event * 60.0 >= self.recording.keep_segments_hours as f32 * 3600.0 {
            err(format!(
                "recording.max_event_minutes ({max_event}) must be shorter than \
                 keep_segments_hours, or the start of a long event is gone before its clip is cut"
            ));
        }

        // Hubs.
        let mut hub_ids = HashSet::new();
        for hub in &self.reolink_hubs {
            if !is_valid_id(&hub.id) {
                err(format!(
                    "reolink_hubs id {:?} must match ^[a-z0-9_-]+$",
                    hub.id
                ));
            }
            if !hub_ids.insert(hub.id.as_str()) {
                err(format!("reolink_hubs id {:?} is used twice", hub.id));
            }
            if !hub.url.starts_with("http://") {
                err(format!(
                    "reolink_hubs {:?}: url must start with http:// (enable HTTP on the Hub; this \
                     build has no TLS client so it stays pure Rust), got {}",
                    hub.id,
                    redact_url(&hub.url)
                ));
            }
            if hub.poll_seconds < 5 {
                err(format!(
                    "reolink_hubs {:?}: poll_seconds must be at least 5",
                    hub.id
                ));
            }
        }

        // Cameras.
        let mut camera_ids = HashSet::new();
        for cam in &self.cameras {
            let id = &cam.id;
            if !is_valid_id(id) {
                err(format!("camera id {id:?} must match ^[a-z0-9_-]+$"));
            }
            if !camera_ids.insert(id.as_str()) {
                err(format!("camera id {id:?} is used twice"));
            }
            if cam.labels.is_empty() {
                err(format!("camera {id:?}: labels must not be empty"));
            }
            for (i, polygon) in cam.motion_mask.iter().enumerate() {
                if polygon.len() < 6 || polygon.len() % 2 != 0 {
                    err(format!(
                        "camera {id:?}: motion_mask[{i}] needs an even number of values, at least \
                         3 points (6 numbers)"
                    ));
                }
                if polygon.iter().any(|v| !(0.0..=1.0).contains(v)) {
                    err(format!(
                        "camera {id:?}: motion_mask[{i}] values must be within 0..1"
                    ));
                }
            }
            match cam.kind {
                CameraKind::Stream => validate_stream_camera(cam, &hub_ids, &mut err),
                CameraKind::HubClips => {
                    match &cam.hub {
                        None => err(format!("camera {id:?}: kind = \"hub_clips\" needs `hub`")),
                        Some(hub) if !hub_ids.contains(hub.as_str()) => err(format!(
                            "camera {id:?}: hub {hub:?} has no matching [[reolink_hubs]] entry"
                        )),
                        Some(_) => {}
                    }
                    if cam.channel.is_none() {
                        err(format!(
                            "camera {id:?}: kind = \"hub_clips\" needs `channel`"
                        ));
                    }
                    if cam.transport.is_some()
                        || cam.detect_url.is_some()
                        || cam.record_url.is_some()
                    {
                        err(format!(
                            "camera {id:?}: kind = \"hub_clips\" must not set transport, detect_url \
                             or record_url"
                        ));
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }
}

/// Percent-encodes a URL user or password.
fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `scheme://host/…` → `scheme://user:password@host/…`, or `None` if the URL already has
/// credentials (or is not a network URL).
fn with_credentials(url: &str, user: &str, password: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme == "file" {
        return None;
    }
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    if authority.contains('@') {
        return None;
    }
    Some(format!(
        "{scheme}://{}:{}@{rest}",
        encode_userinfo(user),
        encode_userinfo(password)
    ))
}

fn validate_stream_camera(
    cam: &CameraConfig,
    hub_ids: &HashSet<&str>,
    err: &mut impl FnMut(String),
) {
    let id = &cam.id;
    if cam.channel.is_some() {
        err(format!(
            "camera {id:?}: channel is only for kind = \"hub_clips\""
        ));
    }
    if let Some(hub) = &cam.hub
        && !hub_ids.contains(hub.as_str())
    {
        err(format!(
            "camera {id:?}: hub {hub:?} has no matching [[reolink_hubs]] entry"
        ));
    }
    if !(1..=15).contains(&cam.detect_fps) {
        err(format!(
            "camera {id:?}: detect_fps must be 1..=15, got {}",
            cam.detect_fps
        ));
    }
    let Some(transport) = cam.transport else {
        err(format!(
            "camera {id:?}: kind = \"stream\" needs transport = \"rtsp\" or \"flv\""
        ));
        return;
    };
    let allowed: &[&str] = match transport {
        Transport::Rtsp => &["rtsp://", "rtsps://", "file://"],
        Transport::Flv => &["http://", "file://"],
    };
    for (field, url) in [
        ("detect_url", &cam.detect_url),
        ("record_url", &cam.record_url),
    ] {
        match url {
            None => err(format!("camera {id:?}: kind = \"stream\" needs {field}")),
            Some(url) if !allowed.iter().any(|p| url.starts_with(p)) => err(format!(
                "camera {id:?}: {field} for transport {:?} must start with {}, got {}",
                transport,
                allowed.join(" or "),
                redact_url(url)
            )),
            Some(_) => {}
        }
    }
}

/// `^[a-z0-9_-]+$`
fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn check_unit(err: &mut impl FnMut(String), name: &str, value: f32) {
    if !(0.0..=1.0).contains(&value) {
        err(format!("{name} must be within 0..=1, got {value}"));
    }
}

fn check_range(err: &mut impl FnMut(String), name: &str, value: usize, min: usize, max: usize) {
    if !(min..=max).contains(&value) {
        err(format!("{name} must be {min}..={max}, got {value}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../config/zoologist.example.toml");

    fn example() -> Config {
        Config::parse(EXAMPLE).expect("example config parses")
    }

    /// Validates and returns the error messages (empty when valid).
    fn problems(config: &Config) -> Vec<String> {
        match config.validate() {
            Ok(()) => Vec::new(),
            Err(ConfigError::Invalid(list)) => list,
            Err(other) => panic!("unexpected error {other}"),
        }
    }

    fn assert_problem(config: &Config, needle: &str) {
        let list = problems(config);
        assert!(
            list.iter().any(|p| p.contains(needle)),
            "expected a problem containing {needle:?}, got {list:#?}"
        );
    }

    fn camera<'a>(config: &'a mut Config, id: &str) -> &'a mut CameraConfig {
        config
            .cameras
            .iter_mut()
            .find(|c| c.id == id)
            .expect("camera in example")
    }

    #[test]
    fn example_config_is_valid() {
        let config = example();
        assert_eq!(problems(&config), Vec::<String>::new());
        assert_eq!(config.station.timezone, chrono_tz::America::New_York);
        assert_eq!(config.cameras.len(), 5);
        assert_eq!(config.reolink_hubs.len(), 1);
        assert_eq!(config.cameras[0].transport, Some(Transport::Flv));
        assert_eq!(config.cameras[3].hub.as_deref(), Some("home-hub"));
        assert_eq!(config.cameras[4].kind, CameraKind::HubClips);
        assert_eq!(config.cameras[0].labels, Label::ALL.to_vec());
    }

    #[test]
    fn stream_cameras_can_use_hub_credentials() {
        let mut config = example();
        config.reolink_hubs[0].user = "zoo user".into();
        config.reolink_hubs[0].password = "p@ss:w/rd".into();
        let hub = config.reolink_hubs[0].id.clone();
        let cam = &mut config.cameras[1];
        cam.transport = Some(Transport::Rtsp);
        cam.detect_url = Some("rtsp://192.168.1.10:554/h264Preview_01_sub".into());
        cam.record_url = Some("rtsp://other:secret@192.168.1.10:554/h264Preview_01_main".into());
        cam.hub = Some(hub);
        assert_eq!(problems(&config), Vec::<String>::new());
        config.apply_hub_credentials();
        let cam = &config.cameras[1];
        assert_eq!(
            cam.detect_url.as_deref(),
            Some("rtsp://zoo%20user:p%40ss%3Aw%2Frd@192.168.1.10:554/h264Preview_01_sub")
        );
        // A URL with its own credentials keeps them.
        assert_eq!(
            cam.record_url.as_deref(),
            Some("rtsp://other:secret@192.168.1.10:554/h264Preview_01_main")
        );

        let mut bad = example();
        bad.cameras[1].hub = Some("nope".into());
        assert_problem(&bad, "hub \"nope\" has no matching");
        let mut bad = example();
        bad.cameras[1].channel = Some(0);
        assert_problem(&bad, "channel is only for");
    }

    #[test]
    fn analysis_width_must_be_sensible() {
        let mut config = example();
        assert_eq!(config.video.analysis_max_width, 1536);
        config.video.analysis_max_width = 100;
        assert_problem(&config, "video.analysis_max_width must be 320..=3840");
    }

    #[test]
    fn empty_file_uses_defaults_but_needs_a_detector_model() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.inference.workers, 3);
        assert_problem(&config, "inference.detector");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Config::parse("[station]\nnmae = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("nmae"), "{err}");
    }

    #[test]
    fn bad_timezone_is_rejected_at_parse_time() {
        assert!(Config::parse("[station]\ntimezone = \"Mars/Olympus\"\n").is_err());
    }

    #[test]
    fn camera_ids_must_be_valid_and_unique() {
        let mut config = example();
        camera(&mut config, "driveway").id = "Drive Way".into();
        assert_problem(&config, "must match ^[a-z0-9_-]+$");

        let mut config = example();
        let dup = config.cameras[0].clone();
        config.cameras.push(dup);
        assert_problem(&config, "is used twice");
    }

    #[test]
    fn stream_cameras_need_transport_and_urls() {
        let mut config = example();
        camera(&mut config, "driveway").transport = None;
        assert_problem(&config, "needs transport");

        let mut config = example();
        camera(&mut config, "backyard").record_url = None;
        assert_problem(&config, "needs record_url");
    }

    #[test]
    fn url_scheme_must_match_transport_and_is_redacted() {
        let mut config = example();
        camera(&mut config, "backyard").detect_url = Some("http://u:secret@cam/x".into());
        let list = problems(&config);
        assert!(
            list.iter().any(|p| p.contains("must start with rtsp://")),
            "{list:#?}"
        );
        assert!(list.iter().all(|p| !p.contains("secret")), "{list:#?}");

        let mut config = example();
        camera(&mut config, "driveway").record_url = Some("rtsp://cam/main".into());
        assert_problem(&config, "must start with http://");
    }

    #[test]
    fn detect_fps_range() {
        let mut config = example();
        camera(&mut config, "driveway").detect_fps = 0;
        assert_problem(&config, "detect_fps must be 1..=15");
        camera(&mut config, "driveway").detect_fps = 30;
        assert_problem(&config, "detect_fps must be 1..=15");
    }

    #[test]
    fn hub_cameras_need_an_existing_hub_and_channel_and_no_urls() {
        let mut config = example();
        camera(&mut config, "trail-argus").hub = Some("nope".into());
        assert_problem(&config, "has no matching [[reolink_hubs]]");

        let mut config = example();
        camera(&mut config, "trail-argus").channel = None;
        assert_problem(&config, "needs `channel`");

        let mut config = example();
        camera(&mut config, "trail-argus").detect_url = Some("rtsp://x/y".into());
        assert_problem(&config, "must not set transport");
    }

    #[test]
    fn hub_url_must_be_plain_http() {
        let mut config = example();
        config.reolink_hubs[0].url = "https://192.168.1.10".into();
        assert_problem(&config, "must start with http://");
    }

    #[test]
    fn thresholds_must_be_fractions() {
        let mut config = example();
        config.tracking.iou_match = 1.5;
        assert_problem(&config, "tracking.iou_match");
        let mut config = example();
        config.species.min_score = -0.1;
        assert_problem(&config, "species.min_score");
        let mut config = example();
        config.models.get_mut("md-sorrel").unwrap().score_threshold = 2.0;
        assert_problem(&config, "score_threshold");
    }

    #[test]
    fn workers_and_input_size_ranges() {
        let mut config = example();
        config.inference.workers = 0;
        assert_problem(&config, "inference.workers");
        let mut config = example();
        config.models.get_mut("md-sorrel").unwrap().input_size = 300;
        assert_problem(&config, "multiple of 32");
    }

    #[test]
    fn motion_mask_polygons_are_checked() {
        let mut config = example();
        camera(&mut config, "driveway").motion_mask = vec![vec![0.0, 0.0, 1.0, 0.0]];
        assert_problem(&config, "at least 3 points");
        camera(&mut config, "driveway").motion_mask = vec![vec![0.0, 0.0, 1.5, 0.0, 0.0, 1.0]];
        assert_problem(&config, "within 0..1");
    }

    #[test]
    fn all_problems_are_reported_together() {
        let mut config = example();
        config.inference.workers = 0;
        config.tracking.min_hits = 0;
        assert!(problems(&config).len() >= 2);
    }
}
