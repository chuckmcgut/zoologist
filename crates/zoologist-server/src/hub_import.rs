//! Reolink Home Hub importer (plan Step 7.2): events for battery cameras, found by analysing
//! each recording the Hub makes. Battery cameras are never streamed, so they are never woken.
//!
//! One thread per Hub. Every `poll_seconds`, for each `kind = "hub_clips"` camera of that Hub:
//! search the Hub for new recordings, download each finished one (the H.264 sub stream), and run
//! it through the same decoder, motion detection, detector, tracker and event code as a live
//! camera, timed from the recording's start. The downloaded file is the clip of every event
//! found in it. Detection jobs use the pool's low-priority queue, so live cameras never wait.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zoologist_core::config::{CameraConfig, CameraKind, HubConfig, HubStream, NoDetection};
use zoologist_core::{Config, Frame, Label, local_date_hour};
use zoologist_store::{ClipState, EventPatch, NewEvent, Store};
use zoologist_video::clips::write_snapshot;
use zoologist_video::decode::DecodeWorker;
use zoologist_video::mp4r::{read_mp4_index, read_samples};
use zoologist_video::reolink_hub::{HubClient, HubFile};
use zoologist_video::stream::{AccessUnit, Codec, StreamItem};
use zoologist_vision::events::{EventKey, EventUpdate};
use zoologist_vision::pool::DetectorHandle;

use crate::analysis::{AnalysisOptions, CameraUpdate, spawn_analysis};
use crate::app::{ApiEvent, AppState};
use crate::tools::decoder_choice;
use crate::writer::{HubClip, HubClips};

/// Recordings that ended less recently than this may still be written by the Hub.
const SETTLE: chrono::Duration = chrono::Duration::seconds(10);
/// Battery clips start with the subject in view: the detector sees the whole frame this long.
const TILE_SECONDS: i64 = 2;
/// Wait this long after a failed poll before trying again (on top of `poll_seconds`).
const ERROR_BACKOFF: Duration = Duration::from_secs(60);

/// Tidies the events found in one battery-camera recording (a few seconds of one PIR trigger):
///
/// - motion events are dropped when the recording also has object events: the motion was the
///   object, seen before its track was confirmed;
/// - several tracks of the same label become one event (earliest start, latest end, best
///   score, the best crops of all of them): at a few frames per second a close, fast object
///   often breaks its track, and one trigger rarely shows two separate objects of one kind.
pub fn merge_recording_events(updates: Vec<EventUpdate>, max_crops: usize) -> Vec<EventUpdate> {
    let mut labels: HashMap<EventKey, Label> = HashMap::new();
    let mut scores: HashMap<EventKey, f32> = HashMap::new();
    for u in &updates {
        match u {
            EventUpdate::Started {
                key, label, score, ..
            } => {
                labels.insert(key.clone(), *label);
                scores.entry(key.clone()).or_insert(*score);
            }
            EventUpdate::Ended { key, top_score, .. } => {
                scores.insert(key.clone(), *top_score);
            }
            EventUpdate::Updated { .. } => {}
        }
    }
    let has_objects = labels.values().any(|l| *l != Label::Motion);
    // The track kept for each object label: the one with the best score.
    let mut primary: HashMap<Label, EventKey> = HashMap::new();
    for (key, label) in &labels {
        if *label == Label::Motion {
            continue;
        }
        let score = scores.get(key).copied().unwrap_or(0.0);
        let better = primary
            .get(label)
            .is_none_or(|p| score > scores.get(p).copied().unwrap_or(0.0));
        if better {
            primary.insert(*label, key.clone());
        }
    }
    let keep = |key: &EventKey| match labels.get(key) {
        Some(Label::Motion) => !has_objects,
        Some(label) => primary.get(label) == Some(key),
        None => true,
    };

    // What each label's merged event spans.
    struct Span {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        top: f32,
        crops: Vec<zoologist_vision::tracker::BestCrop>,
    }
    let mut spans: HashMap<Label, Span> = HashMap::new();
    let mut starts: HashMap<EventKey, DateTime<Utc>> = HashMap::new();
    for u in &updates {
        if let EventUpdate::Started {
            key, started_at, ..
        } = u
        {
            starts.insert(key.clone(), *started_at);
        }
    }
    for u in &updates {
        if let EventUpdate::Ended {
            key,
            ended_at,
            top_score,
            crops,
            ..
        } = u
            && let Some(label) = labels.get(key).filter(|l| **l != Label::Motion)
        {
            let start = starts.get(key).copied().unwrap_or(*ended_at);
            let span = spans.entry(*label).or_insert(Span {
                start,
                end: *ended_at,
                top: *top_score,
                crops: Vec::new(),
            });
            span.start = span.start.min(start);
            span.end = span.end.max(*ended_at);
            span.top = span.top.max(*top_score);
            span.crops.extend(crops.iter().cloned());
        }
    }

    updates
        .into_iter()
        .filter(|u| keep(u.key()))
        .map(|u| {
            let label = labels.get(u.key()).copied();
            match (u, label.and_then(|l| spans.get_mut(&l))) {
                (
                    EventUpdate::Started {
                        key,
                        label,
                        raw_class,
                        score,
                        snapshot,
                        bbox,
                        ..
                    },
                    Some(span),
                ) => EventUpdate::Started {
                    key,
                    label,
                    raw_class,
                    started_at: span.start,
                    score,
                    snapshot,
                    bbox,
                },
                (
                    EventUpdate::Ended {
                        key, median_score, ..
                    },
                    Some(span),
                ) => {
                    let mut crops = std::mem::take(&mut span.crops);
                    crops.sort_by(|a, b| b.quality.total_cmp(&a.quality));
                    crops.truncate(max_crops.max(1));
                    EventUpdate::Ended {
                        key,
                        ended_at: span.end,
                        top_score: span.top,
                        median_score,
                        crops,
                    }
                }
                (u, _) => u,
            }
        })
        .collect()
}

/// What the importer of one Hub is doing, for `/health` and the UI.
#[derive(Clone, Debug, Default, Serialize)]
pub struct HubStatus {
    /// `starting`, `ok` or `error`.
    pub state: String,
    pub last_poll_at: Option<DateTime<Utc>>,
    pub last_import_at: Option<DateTime<Utc>>,
    /// Recordings imported since startup.
    pub imported: u64,
    /// Recordings found but not imported yet.
    pub pending_files: usize,
    pub errors: u64,
    pub last_error: Option<String>,
    /// Per camera: when its newest recording was imported.
    pub cameras: HashMap<String, DateTime<Utc>>,
}

/// One Hub's importer, as the API sees it.
#[derive(Clone)]
pub struct HubRuntime {
    pub id: String,
    pub status: Arc<RwLock<HubStatus>>,
}

/// Everything an importer thread needs.
pub struct Importer {
    pub hub: HubConfig,
    pub config: Arc<Config>,
    pub data_dir: PathBuf,
    pub store: Store,
    pub detector: DetectorHandle,
    pub updates: mpsc::Sender<CameraUpdate>,
    pub hub_clips: HubClips,
    pub status: Arc<RwLock<HubStatus>>,
    pub cancel: CancellationToken,
    /// For events created directly (recordings in which nothing was detected).
    pub app: Option<AppState>,
}

impl Importer {
    fn cameras(&self) -> Vec<CameraConfig> {
        self.config
            .enabled_cameras()
            .filter(|c| c.kind == CameraKind::HubClips && c.hub.as_deref() == Some(&self.hub.id))
            .cloned()
            .collect()
    }

    fn set_status(&self, f: impl FnOnce(&mut HubStatus)) {
        f(&mut self.status.write().unwrap_or_else(|e| e.into_inner()));
    }

    /// Sleeps up to `d`, returning early (true) when cancelled.
    fn sleep(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if self.cancel.is_cancelled() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(250).min(until - Instant::now()));
        }
        self.cancel.is_cancelled()
    }

    /// Runs until cancelled.
    pub fn run(self) {
        let cameras = self.cameras();
        if cameras.is_empty() {
            return;
        }
        self.set_status(|s| s.state = "starting".into());
        let mut client = HubClient::new(&self.hub.url, &self.hub.user, &self.hub.password);
        let mut last_end: HashMap<String, DateTime<Utc>> = HashMap::new();
        let poll = Duration::from_secs(u64::from(self.hub.poll_seconds.max(5)));
        tracing::info!(hub = %self.hub.id, cameras = cameras.len(), "Hub importer started");
        loop {
            let result = self.poll(&mut client, &cameras, &mut last_end);
            match &result {
                Ok(()) => self.set_status(|s| {
                    s.state = "ok".into();
                    s.last_poll_at = Some(Utc::now());
                }),
                Err(e) => {
                    tracing::warn!(hub = %self.hub.id, "Hub import failed: {e:#}");
                    self.set_status(|s| {
                        s.state = "error".into();
                        s.errors += 1;
                        s.last_error = Some(format!("{e:#}"));
                        s.last_poll_at = Some(Utc::now());
                    });
                }
            }
            let wait = if result.is_ok() {
                poll
            } else {
                poll + ERROR_BACKOFF
            };
            if self.sleep(wait) {
                break;
            }
        }
        client.logout();
        tracing::info!(hub = %self.hub.id, "Hub importer stopped");
    }

    /// One pass over every camera of the Hub.
    fn poll(
        &self,
        client: &mut HubClient,
        cameras: &[CameraConfig],
        last_end: &mut HashMap<String, DateTime<Utc>>,
    ) -> Result<()> {
        let now = Utc::now();
        let tz = self.config.station.timezone;
        let lookback = now - chrono::Duration::minutes(i64::from(self.hub.lookback_minutes));
        let stream = match self.hub.analyse_stream {
            HubStream::Sub => "sub",
            HubStream::Main => "main",
        };
        let mut todo = Vec::new();
        for cam in cameras {
            let channel = cam.channel.context("hub_clips camera without channel")?;
            let from = last_end.get(&cam.id).map_or(lookback, |t| {
                (*t - chrono::Duration::minutes(1)).max(lookback)
            });
            for file in client.search(channel, stream, from, now, tz)? {
                if file.end > now - SETTLE
                    || self.store.hub_import_seen(&self.hub.id, &file.name)?
                {
                    continue;
                }
                todo.push((cam, file));
            }
        }
        todo.sort_by_key(|(_, f)| f.start);
        self.set_status(|s| s.pending_files = todo.len());
        for (i, (cam, file)) in todo.iter().enumerate() {
            if self.cancel.is_cancelled() {
                break;
            }
            let event = self
                .import(client, cam, file)
                .with_context(|| format!("importing {} of camera {}", file.name, cam.id))?;
            self.store
                .record_hub_import(&self.hub.id, &file.name, event)?;
            last_end.insert(cam.id.clone(), file.end);
            let done = Utc::now();
            self.set_status(|s| {
                s.imported += 1;
                s.pending_files = todo.len() - i - 1;
                s.last_import_at = Some(done);
                s.cameras.insert(cam.id.clone(), file.end);
            });
        }
        Ok(())
    }

    /// Downloads, analyses and stores one recording. Returns what to record in `HUB_IMPORTS`:
    /// the motion event's id, `Some(0)` when the analysis created the events, `None` when
    /// nothing was kept.
    fn import(
        &self,
        client: &mut HubClient,
        cam: &CameraConfig,
        file: &HubFile,
    ) -> Result<Option<u64>> {
        let tz = self.config.station.timezone;
        let (date, _) = local_date_hour(file.start, tz);
        let rel = format!(
            "clips/hub/{}/{date}/{}.mp4",
            cam.id,
            file.start.with_timezone(&tz).format("%H%M%S")
        );
        let path = self.data_dir.join(&rel);
        let started = Instant::now();
        let bytes = client.download(file, &path)?;
        let (info, entries) = read_mp4_index(&path)
            .with_context(|| format!("reading the downloaded recording {}", path.display()))?;
        if info.codec != Codec::H264 {
            let _ = std::fs::remove_file(&path);
            bail!(
                "the {} stream is {:?}; set analyse_stream = \"sub\" (H.264) for this Hub",
                file.stream,
                info.codec
            );
        }
        let samples = read_samples(&path, &entries)?;
        self.hub_clips.add(
            &cam.id,
            HubClip {
                start: file.start,
                end: file.end,
                path: rel.clone(),
                bytes,
            },
        );

        let (started_events, first_frame) = self.analyse(cam, file, info, samples)?;
        tracing::info!(
            camera = %cam.id,
            file = %file.name,
            events = started_events,
            secs = format!("{:.1}", started.elapsed().as_secs_f32()),
            "imported Hub recording"
        );
        if started_events > 0 {
            return Ok(Some(0));
        }
        match self.hub.no_detection {
            NoDetection::Discard => {
                let _ = std::fs::remove_file(&path);
                Ok(None)
            }
            NoDetection::Motion => self
                .motion_event(cam, file, &rel, bytes, first_frame)
                .map(Some),
        }
    }

    /// Feeds the recording through the live analysis code. Returns how many events started,
    /// and the first decoded frame.
    fn analyse(
        &self,
        cam: &CameraConfig,
        file: &HubFile,
        info: zoologist_video::stream::StreamInfo,
        samples: Vec<zoologist_video::mp4w::Sample>,
    ) -> Result<(usize, Option<Frame>)> {
        let (frames_tx, frames_rx) = mpsc::channel::<Frame>(4);
        let (local_tx, mut local_rx) = mpsc::channel::<CameraUpdate>(64);
        let options = AnalysisOptions {
            background: true,
            tiles_until: Some(file.start + chrono::Duration::seconds(TILE_SECONDS)),
        };
        let analysis = spawn_analysis(
            cam.clone(),
            self.config.clone(),
            frames_rx,
            Some(self.detector.clone()),
            local_tx,
            Arc::default(),
            Arc::default(),
            options,
        )?;

        // Decode on this thread's helper while this thread forwards event updates, so neither
        // side can block the other.
        let mut worker =
            DecodeWorker::new(cam.id.clone(), decoder_choice(&self.config), cam.detect_fps)
                .with_max_width(self.config.video.analysis_max_width);
        let start = file.start;
        let feeder = std::thread::Builder::new()
            .name(format!("hub-decode-{}", cam.id))
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

        // A recording is short: collect its events, tidy them up, then store them.
        let mut updates = Vec::new();
        while let Some((_, update)) = local_rx.blocking_recv() {
            updates.push(update);
        }
        let first = feeder.join().ok().flatten();
        let _ = analysis.join();
        let updates = merge_recording_events(updates, self.config.species.max_crops_per_event);
        let started = updates
            .iter()
            .filter(|u| matches!(u, EventUpdate::Started { .. }))
            .count();
        for update in updates {
            if self
                .updates
                .blocking_send((cam.id.clone(), update))
                .is_err()
            {
                break;
            }
        }
        Ok((started, first))
    }

    /// A `motion` event spanning the whole recording, for recordings in which nothing was
    /// detected (the camera's PIR still saw something).
    fn motion_event(
        &self,
        cam: &CameraConfig,
        file: &HubFile,
        clip: &str,
        bytes: u64,
        frame: Option<Frame>,
    ) -> Result<u64> {
        let record = self.store.insert_event(&NewEvent {
            camera_id: cam.id.clone(),
            label: Label::Motion,
            raw_class: Some("pir".into()),
            started_at: file.start,
            top_score: 0.0,
            median_score: 0.0,
            best_bbox: None,
            snapshot_path: None,
            thumb_path: None,
        })?;
        let (date, _) = local_date_hour(file.start, self.config.station.timezone);
        let snapshot = frame.and_then(|f| {
            let rel = format!("snapshots/{date}/{}.jpg", record.id);
            write_snapshot(&f, None, &self.data_dir.join(&rel))
                .ok()
                .map(|()| rel)
        });
        let patch = EventPatch {
            ended_at: Some(file.end),
            snapshot_path: Some(snapshot),
            clip_state: Some(ClipState::Ready),
            clip_path: Some(Some(clip.to_string())),
            clip_bytes: Some(Some(bytes)),
            ..Default::default()
        };
        if let Some(updated) = self.store.update_event(record.id, &patch)?
            && let Some(app) = &self.app
        {
            app.publish(ApiEvent::Started(updated.clone()));
            app.publish(ApiEvent::Ended(updated));
        }
        Ok(record.id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;
    use zoologist_core::BBox;
    use zoologist_vision::events::EventSource;
    use zoologist_vision::tracker::BestCrop;

    use super::*;

    fn t(s: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, 19, 11, 0).unwrap() + chrono::Duration::seconds(s)
    }

    fn frame() -> Frame {
        Frame {
            camera_id: "cam".into(),
            seq: 0,
            captured_at: t(0),
            width: 16,
            height: 16,
            i420: Arc::new(vec![0; 16 * 16 * 3 / 2]),
        }
    }

    fn key(n: u64, motion: bool) -> EventKey {
        EventKey {
            camera_id: "cam".into(),
            source: if motion {
                EventSource::Motion(n)
            } else {
                EventSource::Track(n)
            },
        }
    }

    fn event(n: u64, label: Label, from: i64, to: i64, score: f32) -> Vec<EventUpdate> {
        let k = key(n, label == Label::Motion);
        let crop = BestCrop {
            frame: frame(),
            bbox: BBox::new(0.1, 0.1, 0.5, 0.5),
            score,
            quality: score,
        };
        vec![
            EventUpdate::Started {
                key: k.clone(),
                label,
                raw_class: None,
                started_at: t(from),
                score,
                snapshot: frame(),
                bbox: None,
            },
            EventUpdate::Ended {
                key: k,
                ended_at: t(to),
                top_score: score,
                median_score: score,
                crops: if label == Label::Motion {
                    vec![]
                } else {
                    vec![crop]
                },
            },
        ]
    }

    #[test]
    fn broken_tracks_become_one_event_and_motion_goes() {
        let mut ups = event(1, Label::Motion, 0, 9, 0.0);
        ups.extend(event(2, Label::Vehicle, 1, 3, 0.9));
        ups.extend(event(3, Label::Vehicle, 4, 6, 0.95));
        ups.extend(event(4, Label::Vehicle, 7, 8, 0.8));
        ups.extend(event(5, Label::Person, 2, 5, 0.9));
        let out = merge_recording_events(ups, 3);
        let started: Vec<_> = out
            .iter()
            .filter_map(|u| match u {
                EventUpdate::Started {
                    key,
                    label,
                    started_at,
                    ..
                } => Some((key.clone(), *label, *started_at)),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 2, "one vehicle, one person, no motion");
        let vehicle = started.iter().find(|s| s.1 == Label::Vehicle).unwrap();
        assert_eq!(vehicle.0, key(3, false), "the best-scoring track is kept");
        assert_eq!(vehicle.2, t(1), "from the first track's start");
        let ended = out
            .iter()
            .find_map(|u| match u {
                EventUpdate::Ended {
                    key,
                    ended_at,
                    top_score,
                    crops,
                    ..
                } if *key == vehicle.0 => Some((*ended_at, *top_score, crops.len())),
                _ => None,
            })
            .unwrap();
        assert_eq!(ended, (t(8), 0.95, 3));
    }

    #[test]
    fn motion_only_recordings_keep_their_motion() {
        let out = merge_recording_events(event(1, Label::Motion, 0, 9, 0.0), 3);
        assert_eq!(out.len(), 2);
    }
}
