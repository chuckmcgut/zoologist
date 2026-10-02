//! Turning event updates into stored events, pictures, clips and species (plan Step 7.1).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use zoologist_core::config::CameraKind;
use zoologist_core::{BBox, Frame, Label, local_date_hour};
use zoologist_store::{ClipState, EventPatch, EventQuery, NewEvent, SegmentRecord};
use zoologist_video::clips::{SegmentFile, build_clip, write_snapshot, write_thumb};
use zoologist_vision::events::{EventKey, EventUpdate};
use zoologist_vision::species::{
    Check, SpeciesAnswer, SpeciesCrop, SpeciesJob, settle_person, settle_still_animal,
};

use crate::analysis::CameraUpdate;
use crate::app::{ApiEvent, AppState};
use crate::snapshots::{self, Snap, Snapshotter};

/// Pictures of a live event are refreshed at most this often.
const PICTURE_INTERVAL: Duration = Duration::from_secs(2);
/// At most this many clips are built at once.
const CLIP_JOBS: usize = 2;

/// Set when a camera's recorder has written its last segment (the source ended).
pub type RecorderDone = HashMap<String, Arc<AtomicBool>>;

/// A downloaded Hub recording that serves as the clip of every event found in it.
#[derive(Clone, Debug)]
pub struct HubClip {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Relative to the data directory.
    pub path: String,
    pub bytes: u64,
}

/// Clip files of imported Hub recordings, by camera. The importer adds one before analysing
/// a recording; the writer links each event of that recording to it.
#[derive(Clone, Default)]
pub struct HubClips(Arc<std::sync::Mutex<HashMap<String, Vec<HubClip>>>>);

impl HubClips {
    pub fn add(&self, camera: &str, clip: HubClip) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let list = map.entry(camera.to_string()).or_default();
        // Events end at most a few seconds after their recording; older entries are done.
        let cutoff = clip.start - chrono::Duration::hours(1);
        list.retain(|c| c.end > cutoff);
        list.push(clip);
    }

    /// The recording that contains `t` (with 2 s of slack either side).
    pub fn find(&self, camera: &str, t: DateTime<Utc>) -> Option<HubClip> {
        let slack = chrono::Duration::seconds(2);
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(camera)?
            .iter()
            .rev()
            .find(|c| c.start - slack <= t && t <= c.end + slack)
            .cloned()
    }
}

/// Boxes overlapping by this much are the same spot.
const SAME_SPOT_IOU: f32 = 0.5;

/// True when event `id`'s box is where someone marked another event of the same camera as
/// "nothing there" (the "Wrong" button).
async fn marked_nothing_spot(app: &AppState, camera_id: &str, id: u64) -> bool {
    let camera = camera_id.to_string();
    let found = app
        .store
        .call(move |s| {
            let Some(bbox) = s.get_event(id)?.and_then(|e| e.best_bbox) else {
                return Ok(false);
            };
            let marks = s.list_events(&EventQuery {
                camera: Some(camera),
                marked_wrong: true,
                limit: zoologist_store::MAX_PAGE,
                ..Default::default()
            })?;
            Ok(marks.items.iter().any(|e| {
                e.id != id
                    && e.feedback.as_ref().is_some_and(|f| f.actual.is_none())
                    && e.best_bbox.is_some_and(|b| b.iou(&bbox) >= SAME_SPOT_IOU)
            }))
        })
        .await;
    found.unwrap_or(false)
}

/// A person or an animal that turned out to be nothing: the event becomes plain motion. A camera
/// whose `labels` leave out motion gets no motion events this way either: there the event is
/// removed, with its pictures and its clip.
async fn became_motion(app: &AppState, camera_id: &str, id: u64) {
    let camera = app.config.cameras.iter().find(|c| c.id == camera_id);
    if camera.is_none_or(|c| c.labels.contains(&Label::Motion)) {
        let patch = EventPatch {
            label: Some(Label::Motion),
            ..Default::default()
        };
        if let Ok(Some(record)) = app.store.call(move |s| s.update_event(id, &patch)).await {
            app.publish(ApiEvent::Updated(record));
        }
        return;
    }
    let removed = app
        .store
        .call(move |s| {
            let record = s.get_event(id)?;
            if record.is_some() {
                s.delete_event(id)?;
            }
            Ok(record)
        })
        .await;
    let Ok(Some(record)) = removed else {
        return;
    };
    // A Hub recording is the clip of every event found in it, so it stays.
    let own_clip = camera.is_some_and(|c| c.kind == CameraKind::Stream);
    let clip = record.clip_path.as_ref().filter(|_| own_clip);
    for rel in [
        clip,
        record.snapshot_path.as_ref(),
        record.thumb_path.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let _ = tokio::fs::remove_file(app.data_dir.join(rel)).await;
    }
    tracing::info!(camera = %camera_id, event = id, "not an event on this camera: removed");
    app.publish(ApiEvent::Removed(record));
}

/// What every update of the writer needs besides the event itself.
struct Shared {
    clip_slots: Arc<Semaphore>,
    snapshotter: Snapshotter,
    recorders: RecorderDone,
    hub_clips: HubClips,
}

struct Open {
    id: u64,
    label: Label,
    started_at: DateTime<Utc>,
    last_picture: Instant,
    top_score: f32,
    /// Full-resolution snapshots being taken while an animal is in view.
    snaps: Vec<tokio::task::JoinHandle<Option<Snap>>>,
    last_snap: Option<Instant>,
}

/// Starts a snapshot of an animal on a live Hub camera, if allowed now.
fn maybe_snap(
    app: &AppState,
    snapshotter: &Snapshotter,
    camera_id: &str,
    o: &mut Open,
    bbox: BBox,
) {
    if o.label != Label::Animal
        || !app.config.species.snapshots
        || app.species.is_none()
        || o.snaps.len() >= snapshots::MAX_PER_EVENT
        || o.last_snap
            .is_some_and(|t| t.elapsed() < snapshots::INTERVAL)
    {
        return;
    }
    let Some(camera) = app.config.cameras.iter().find(|c| c.id == camera_id) else {
        return;
    };
    if snapshots::snapshot_channel(camera).is_none() {
        return;
    }
    o.last_snap = Some(Instant::now());
    let (app, snapshotter, camera) = (app.clone(), snapshotter.clone(), camera.clone());
    o.snaps.push(tokio::spawn(async move {
        snapshotter.take(&app, &camera, bbox).await
    }));
}

/// Consumes event updates until the channel closes, then waits for pending clip and species
/// jobs. Returns when everything is written.
pub async fn run_writer(
    app: AppState,
    mut updates: mpsc::Receiver<CameraUpdate>,
    recorders: RecorderDone,
    hub_clips: HubClips,
) {
    let mut open: HashMap<EventKey, Open> = HashMap::new();
    let mut jobs = JoinSet::new();
    let shared = Shared {
        clip_slots: Arc::new(Semaphore::new(CLIP_JOBS)),
        snapshotter: Snapshotter::default(),
        recorders,
        hub_clips,
    };
    while let Some((camera_id, update)) = updates.recv().await {
        let result = handle(&app, &mut open, &mut jobs, &shared, &camera_id, update).await;
        if let Err(e) = result {
            tracing::warn!(camera = %camera_id, "could not store event: {e:#}");
        }
    }
    while jobs.join_next().await.is_some() {}
}

fn rel_path(kind: &str, started_at: DateTime<Utc>, app: &AppState, id: u64, ext: &str) -> String {
    let (date, _) = local_date_hour(started_at, app.config.station.timezone);
    format!("{kind}/{date}/{id}.{ext}")
}

/// Writes the snapshot and (when there is a box) the thumbnail of an event.
async fn write_pictures(
    app: &AppState,
    id: u64,
    started_at: DateTime<Utc>,
    frame: Frame,
    bbox: Option<BBox>,
) -> (String, Option<String>) {
    let snap = rel_path("snapshots", started_at, app, id, "jpg");
    let thumb = bbox.map(|_| rel_path("thumbs", started_at, app, id, "jpg"));
    let (dir, s, t) = (app.data_dir.clone(), snap.clone(), thumb.clone());
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        write_snapshot(&frame, bbox.as_ref(), &dir.join(&s))?;
        if let (Some(t), Some(b)) = (t, bbox) {
            write_thumb(&frame, &b, &dir.join(t))?;
        }
        Ok(())
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        tracing::warn!(event = id, "could not write snapshot: {result:?}");
    }
    (snap, thumb)
}

async fn handle(
    app: &AppState,
    open: &mut HashMap<EventKey, Open>,
    jobs: &mut JoinSet<()>,
    shared: &Shared,
    camera_id: &str,
    update: EventUpdate,
) -> anyhow::Result<()> {
    let Shared {
        clip_slots,
        snapshotter,
        recorders,
        hub_clips,
    } = shared;
    match update {
        EventUpdate::Started {
            key,
            label,
            raw_class,
            started_at,
            score,
            snapshot,
            bbox,
        } => {
            let new = NewEvent {
                camera_id: camera_id.to_string(),
                label,
                raw_class,
                started_at,
                top_score: score,
                median_score: score,
                best_bbox: bbox,
                snapshot_path: None,
                thumb_path: None,
            };
            let record = app.store.call(move |s| s.insert_event(&new)).await?;
            let (snap, thumb) = write_pictures(app, record.id, started_at, snapshot, bbox).await;
            let patch = EventPatch {
                snapshot_path: Some(Some(snap)),
                thumb_path: Some(thumb),
                ..Default::default()
            };
            let id = record.id;
            if let Some(record) = app.store.call(move |s| s.update_event(id, &patch)).await? {
                tracing::info!(camera = %camera_id, event = id, label = %label, "event started");
                app.publish(ApiEvent::Started(record));
            }
            let mut o = Open {
                id,
                label,
                started_at,
                last_picture: Instant::now(),
                top_score: score,
                snaps: Vec::new(),
                last_snap: None,
            };
            if let Some(bbox) = bbox {
                maybe_snap(app, snapshotter, camera_id, &mut o, bbox);
            }
            open.insert(key, o);
        }
        EventUpdate::Updated {
            key,
            top_score,
            best,
        } => {
            let Some(o) = open.get_mut(&key) else {
                return Ok(());
            };
            let mut patch = EventPatch::default();
            if top_score > o.top_score {
                o.top_score = top_score;
                patch.top_score = Some(top_score);
            }
            if let Some(best) = &best {
                // A better view: the animal is well in view now.
                maybe_snap(app, snapshotter, camera_id, o, best.bbox);
            }
            if let Some(best) = best
                && o.last_picture.elapsed() >= PICTURE_INTERVAL
            {
                o.last_picture = Instant::now();
                write_pictures(app, o.id, o.started_at, best.frame, Some(best.bbox)).await;
                patch.best_bbox = Some(best.bbox);
            }
            if patch != EventPatch::default() {
                let id = o.id;
                if let Some(record) = app.store.call(move |s| s.update_event(id, &patch)).await? {
                    app.publish(ApiEvent::Updated(record));
                }
            }
        }
        EventUpdate::Ended {
            key,
            ended_at,
            top_score,
            median_score,
            crops,
            moved,
        } => {
            let Some(mut o) = open.remove(&key) else {
                return Ok(());
            };
            let camera = app.config.cameras.iter().find(|c| c.id == camera_id);
            let recording = camera.is_some_and(|c| c.kind == CameraKind::Stream && c.record);
            // Imported Hub recordings are their own clip.
            let hub_clip = camera
                .filter(|c| c.kind == CameraKind::HubClips)
                .and_then(|_| hub_clips.find(camera_id, o.started_at));
            let mut patch = EventPatch {
                ended_at: Some(ended_at),
                top_score: Some(top_score.max(o.top_score)),
                median_score: Some(median_score),
                clip_state: (!recording).then_some(ClipState::Failed),
                ..Default::default()
            };
            if let Some(clip) = hub_clip {
                patch.clip_state = Some(ClipState::Ready);
                patch.clip_path = Some(Some(clip.path));
                patch.clip_bytes = Some(Some(clip.bytes));
            }
            let id = o.id;
            if let Some(record) = app.store.call(move |s| s.update_event(id, &patch)).await? {
                tracing::info!(camera = %camera_id, event = id, "event ended");
                app.publish(ApiEvent::Ended(record));
            }
            if recording {
                let rec = &app.config.recording;
                let ms = |s: f32| chrono::Duration::milliseconds((s * 1000.0) as i64);
                let from = o.started_at - ms(rec.pre_capture_seconds);
                let to = ended_at + ms(rec.post_capture_seconds);
                let done = recorders.get(camera_id).cloned().unwrap_or_default();
                jobs.spawn(clip_job(
                    app.clone(),
                    clip_slots.clone(),
                    id,
                    camera_id.to_string(),
                    from,
                    to,
                    done,
                ));
            }
            // A spot someone marked "nothing there": the chair or the stump again.
            if matches!(o.label, Label::Person | Label::Animal)
                && !moved
                && app.config.species.learn_from_marks
                && marked_nothing_spot(app, camera_id, id).await
            {
                tracing::info!(event = id, label = %o.label, "still, in a spot marked as nothing: motion");
                became_motion(app, camera_id, id).await;
                return Ok(());
            }
            let check = o.label == Label::Animal
                || (o.label == Label::Person && app.config.species.check_people);
            if check
                && let Some(species) = &app.species
                && !crops.is_empty()
            {
                let check = if o.label == Label::Person {
                    Check::Person
                } else {
                    Check::Animal
                };
                let mut crops: Vec<SpeciesCrop> = crops
                    .into_iter()
                    .map(|c| SpeciesCrop {
                        frame: c.frame,
                        bbox: c.bbox,
                        quality: c.quality,
                    })
                    .collect();
                let snaps = std::mem::take(&mut o.snaps);
                let species = species.clone();
                let detector = app.detector.clone();
                let app = app.clone();
                let still_as_motion = app.config.species.still_unnamed_as_motion;
                let person = o.label == Label::Person;
                let camera_id = camera_id.to_string();
                jobs.spawn(async move {
                    // Sharp views from full-resolution snapshots, where the detector finds the
                    // animal again.
                    let mut sharp = 0;
                    for snap in snaps {
                        if let (Ok(Some(snap)), Some(detector)) = (snap.await, &detector)
                            && let Some(crop) = snapshots::locate(detector, snap).await
                        {
                            crops.push(crop);
                            sharp += 1;
                        }
                    }
                    if sharp > 0 {
                        tracing::info!(event = id, sharp, "sharp views from snapshots");
                    }
                    let (tx, rx) = oneshot::channel();
                    species.submit(SpeciesJob {
                        check,
                        crops,
                        detector_score: top_score,
                        reply: tx,
                    });
                    let answer = if person {
                        // A second opinion on a person: only "nothing there" changes it.
                        match rx.await.ok().and_then(settle_person) {
                            Some(label) => Ok(SpeciesAnswer::NotAnimal(label)),
                            None => return,
                        }
                    } else {
                        rx.await.map(|a| settle_still_animal(a, moved, still_as_motion))
                    };
                    let patch = match answer {
                        Ok(SpeciesAnswer::Species(guess)) => {
                            tracing::info!(event = id, species = %guess.common_name, score = guess.score, "species");
                            EventPatch {
                                species: Some(guess),
                                ..Default::default()
                            }
                        }
                        // The detector called a person or a vehicle an animal, or it never moved
                        // and could not be named.
                        Ok(SpeciesAnswer::NotAnimal(label)) => {
                            tracing::info!(event = id, label = %label, moved, person, "relabelled");
                            if label == Label::Motion {
                                became_motion(&app, &camera_id, id).await;
                                return;
                            }
                            EventPatch {
                                label: Some(label),
                                ..Default::default()
                            }
                        }
                        _ => return,
                    };
                    if let Ok(Some(record)) = app.store.call(move |s| s.update_event(id, &patch)).await {
                        app.publish(ApiEvent::Updated(record));
                    }
                });
            }
        }
    }
    Ok(())
}

/// Waits until the recording covers `[from, to]` (or the camera's recorder has stopped, or a
/// timeout passes), then cuts the clip.
async fn clip_job(
    app: AppState,
    slots: Arc<Semaphore>,
    id: u64,
    camera: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    recorder_done: Arc<AtomicBool>,
) {
    let segment = Duration::from_secs(u64::from(app.config.recording.segment_seconds));
    let deadline = Instant::now() + segment * 3 + Duration::from_secs(30);
    let segments: Vec<SegmentRecord> = loop {
        let cam = camera.clone();
        let found = app
            .store
            .call(move |s| s.segments_between(&cam, from, to))
            .await
            .unwrap_or_default();
        let covered = found.iter().any(|s| s.ended_at >= to);
        if covered || recorder_done.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break found;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let Ok(_permit) = slots.acquire().await else {
        return;
    };
    let started = segments.first().map_or(from, |s| s.started_at);
    let out_rel = rel_path("clips", from.max(started), &app, id, "mp4");
    let files: Vec<SegmentFile> = segments
        .iter()
        .map(|s| SegmentFile {
            path: s.path.clone(),
            index_path: s.index_path.clone(),
        })
        .collect();
    let (dir, out) = (app.data_dir.clone(), out_rel.clone());
    let result =
        tokio::task::spawn_blocking(move || build_clip(&dir, &files, from, to, &dir.join(out)))
            .await;
    let built = matches!(result, Ok(Ok(_))).then(|| app.data_dir.join(&out_rel));
    let patch = match result {
        Ok(Ok(clip)) => EventPatch {
            clip_path: Some(Some(out_rel)),
            clip_bytes: Some(Some(clip.bytes)),
            clip_state: Some(ClipState::Ready),
            ..Default::default()
        },
        other => {
            tracing::warn!(event = id, camera = %camera, "no clip: {other:?}");
            EventPatch {
                clip_state: Some(ClipState::Failed),
                ..Default::default()
            }
        }
    };
    match app.store.call(move |s| s.update_event(id, &patch)).await {
        Ok(Some(record)) => app.publish(ApiEvent::Updated(record)),
        // The event was removed while its clip was cut (see `became_motion`).
        Ok(None) => {
            if let Some(path) = built {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
        Err(_) => {}
    }
}

#[cfg(test)]
mod spot_tests {
    use chrono::Utc;
    use zoologist_core::Config;
    use zoologist_core::config::CameraConfig;
    use zoologist_store::{Feedback, Store};

    use super::*;

    fn event(app: &AppState, camera: &str, bbox: BBox) -> u64 {
        let new = NewEvent {
            camera_id: camera.into(),
            label: Label::Person,
            raw_class: None,
            started_at: Utc::now(),
            top_score: 0.7,
            median_score: 0.7,
            best_bbox: Some(bbox),
            snapshot_path: None,
            thumb_path: None,
        };
        app.store.insert_event(&new).unwrap().id
    }

    fn mark(app: &AppState, id: u64, actual: Option<Label>) {
        let patch = EventPatch {
            feedback: Some(Some(Feedback {
                actual,
                species: None,
                note: Some("upside-down camp chair".into()),
                at: Utc::now(),
            })),
            ..Default::default()
        };
        app.store.update_event(id, &patch).unwrap();
    }

    /// The owner's camp chair: marked "nothing" once, then found again in the same box.
    #[tokio::test]
    async fn a_spot_marked_nothing_is_recognised_again() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let store = Store::open(&dir.path().join("db.redb"), config.station.timezone).unwrap();
        let app = AppState::without_pipeline(config, store);
        let chair = BBox::new(0.638, 0.556, 0.668, 0.720);
        let first = event(&app, "container", chair);
        let again = event(&app, "container", BBox::new(0.637, 0.564, 0.668, 0.759));
        let elsewhere = event(&app, "container", BBox::new(0.2, 0.5, 0.25, 0.7));
        let other_camera = event(&app, "nc200", chair);
        assert!(
            !marked_nothing_spot(&app, "container", again).await,
            "nothing marked yet"
        );
        mark(&app, first, None);
        assert!(marked_nothing_spot(&app, "container", again).await);
        assert!(!marked_nothing_spot(&app, "container", elsewhere).await);
        assert!(
            !marked_nothing_spot(&app, "nc200", other_camera).await,
            "other camera"
        );
        assert!(
            !marked_nothing_spot(&app, "container", first).await,
            "not its own mark"
        );
        // "It was a person" is not a spot to ignore.
        mark(&app, first, Some(Label::Person));
        assert!(!marked_nothing_spot(&app, "container", again).await);
    }

    /// The owner's Container camera makes no motion events (sun and shadows all day). A
    /// "person" there that turns out to be the power meter is not kept as motion: it goes, with
    /// its pictures and its clip. On a camera with motion events it stays, as motion.
    #[tokio::test]
    async fn a_demoted_event_is_removed_where_motion_is_not_wanted() {
        let dir = tempfile::tempdir().unwrap();
        let camera = |id: &str, labels: &[&str]| -> CameraConfig {
            serde_json::from_value(serde_json::json!({
                "id": id, "name": id, "labels": labels,
                "detect_url": "rtsp://10.0.0.1/sub", "record_url": "rtsp://10.0.0.1/main"
            }))
            .unwrap()
        };
        let mut config = Config::default();
        config.server.data_dir = dir.path().to_path_buf();
        config.cameras = vec![
            camera("container", &["person", "vehicle", "animal"]),
            camera("yard", &["person", "vehicle", "animal", "motion"]),
        ];
        let store = Store::open(&dir.path().join("db.redb"), config.station.timezone).unwrap();
        let app = AppState::without_pipeline(config, store);
        let mut live = app.events.subscribe();
        let bbox = BBox::new(0.57, 0.47, 0.59, 0.68);

        let meter = event(&app, "container", bbox);
        let files = ["clips/1.mp4", "snapshots/1.jpg", "thumbs/1.jpg"];
        for rel in files {
            let path = app.data_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
        let patch = EventPatch {
            clip_path: Some(Some(files[0].into())),
            snapshot_path: Some(Some(files[1].into())),
            thumb_path: Some(Some(files[2].into())),
            ..Default::default()
        };
        app.store.update_event(meter, &patch).unwrap();
        became_motion(&app, "container", meter).await;
        assert!(app.store.get_event(meter).unwrap().is_none());
        for rel in files {
            assert!(!app.data_dir.join(rel).exists(), "{rel} is gone");
        }
        assert!(matches!(live.try_recv(), Ok(ApiEvent::Removed(r)) if r.id == meter));

        let stump = event(&app, "yard", bbox);
        became_motion(&app, "yard", stump).await;
        let kept = app.store.get_event(stump).unwrap().expect("kept");
        assert_eq!(kept.label, Label::Motion);
        assert!(matches!(live.try_recv(), Ok(ApiEvent::Updated(r)) if r.id == stump));
    }
}
