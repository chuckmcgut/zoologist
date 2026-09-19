use std::path::Path;

use chrono::{Duration, TimeZone, Utc};
use zoologist_core::{Config, Label};
use zoologist_store::{ClipState, EventPatch, NewEvent, SegmentRecord, Store};

use super::run_once;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 19, 12, 0, 0).unwrap()
}

fn setup() -> (tempfile::TempDir, Store, Config) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db.redb"), chrono_tz::UTC).unwrap();
    let mut config = Config::default();
    config.recording.keep_segments_hours = 6;
    config.retention.clips_days = 30;
    config.retention.clips_max_total_mb = 1000;
    (dir, store, config)
}

fn write(dir: &Path, rel: &str, bytes: usize) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![0u8; bytes]).unwrap();
}

/// Stores a 10 s segment of camera `yard` that started `hours_ago`, with its files.
fn segment(dir: &Path, store: &Store, hours_ago: i64) -> SegmentRecord {
    let started_at = now() - Duration::hours(hours_ago);
    let name = format!(
        "recordings/yard/{}/{}",
        started_at.format("%Y%m%d"),
        started_at.format("%H%M%S")
    );
    let seg = SegmentRecord {
        path: format!("{name}.mp4"),
        index_path: format!("{name}.idx"),
        started_at,
        ended_at: started_at + Duration::seconds(10),
        bytes: 100,
    };
    write(dir, &seg.path, 100);
    write(dir, &seg.index_path, 10);
    store.insert_segment("yard", &seg).unwrap();
    seg
}

/// Stores an ended event that started `hours_ago`, with a clip of `clip_bytes` and pictures.
fn event(dir: &Path, store: &Store, label: Label, hours_ago: i64, clip_bytes: usize) -> u64 {
    let started_at = now() - Duration::hours(hours_ago);
    let e = store
        .insert_event(&NewEvent {
            camera_id: "yard".into(),
            label,
            raw_class: None,
            started_at,
            top_score: 0.9,
            median_score: 0.8,
            best_bbox: None,
            snapshot_path: None,
            thumb_path: None,
        })
        .unwrap();
    let date = started_at.format("%Y-%m-%d");
    let (clip, snap, thumb) = (
        format!("clips/{date}/{}.mp4", e.id),
        format!("snapshots/{date}/{}.jpg", e.id),
        format!("thumbs/{date}/{}.jpg", e.id),
    );
    write(dir, &clip, clip_bytes);
    write(dir, &snap, 50);
    write(dir, &thumb, 20);
    store
        .update_event(
            e.id,
            &EventPatch {
                ended_at: Some(started_at + Duration::seconds(20)),
                clip_state: Some(ClipState::Ready),
                clip_path: Some(Some(clip)),
                clip_bytes: Some(Some(clip_bytes as u64)),
                snapshot_path: Some(Some(snap)),
                thumb_path: Some(Some(thumb)),
                ..Default::default()
            },
        )
        .unwrap();
    e.id
}

#[test]
fn old_segments_are_deleted_and_recent_ones_kept() {
    let (dir, store, config) = setup();
    let old = segment(dir.path(), &store, 7);
    let recent = segment(dir.path(), &store, 2);
    let report = run_once(&store, &config, dir.path(), now(), false).unwrap();
    assert_eq!(report.segments_deleted, 1);
    assert!(!dir.path().join(&old.path).exists());
    assert!(!dir.path().join(&old.index_path).exists());
    assert!(dir.path().join(&recent.path).exists());
    let left = store.segments_before(now()).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].1.started_at, recent.started_at);
    assert!(dir.path().join("recordings").exists());
}

#[test]
fn a_pending_clip_keeps_the_segments_it_needs() {
    let (dir, store, config) = setup();
    let needed = segment(dir.path(), &store, 7);
    let pending = event(dir.path(), &store, Label::Animal, 7, 100);
    store
        .update_event(
            pending,
            &EventPatch {
                clip_state: Some(ClipState::Pending),
                clip_path: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    let older = segment(dir.path(), &store, 8);
    let report = run_once(&store, &config, dir.path(), now(), false).unwrap();
    assert_eq!(report.segments_deleted, 1, "{:?}", report.actions);
    assert!(dir.path().join(&needed.path).exists());
    assert!(!dir.path().join(&older.path).exists());
}

#[test]
fn expired_events_lose_their_media_but_stay() {
    let (dir, store, config) = setup();
    let old = event(dir.path(), &store, Label::Person, 31 * 24, 100);
    let fresh = event(dir.path(), &store, Label::Person, 24, 100);
    let before = store.get_event(old).unwrap().unwrap();
    let report = run_once(&store, &config, dir.path(), now(), false).unwrap();
    assert_eq!(report.events_purged, 1);
    let after = store.get_event(old).unwrap().expect("event kept");
    assert_eq!(after.clip_state, ClipState::Purged);
    assert_eq!(after.clip_path, None);
    assert_eq!(after.snapshot_path, None);
    assert_eq!(after.thumb_path, None);
    for rel in [before.clip_path, before.snapshot_path, before.thumb_path] {
        assert!(!dir.path().join(rel.unwrap()).exists());
    }
    assert_eq!(
        store.get_event(fresh).unwrap().unwrap().clip_state,
        ClipState::Ready
    );
    // A second pass has nothing left to do.
    let again = run_once(&store, &config, dir.path(), now(), false).unwrap();
    assert_eq!(again.events_purged, 0);
}

#[test]
fn clips_over_the_size_cap_go_oldest_first_motion_first() {
    let (dir, store, mut config) = setup();
    config.retention.clips_max_total_mb = 2;
    let mb = 1024 * 1024;
    // Same day (hours 30 and 29 ago = 2026-09-18), then a newer day.
    let old_animal = event(dir.path(), &store, Label::Animal, 30, mb);
    let old_motion = event(dir.path(), &store, Label::Motion, 29, mb);
    let new_person = event(dir.path(), &store, Label::Person, 2, mb);
    let report = run_once(&store, &config, dir.path(), now(), false).unwrap();
    assert_eq!(report.clips_trimmed, 1, "{:?}", report.actions);
    let state = |id| store.get_event(id).unwrap().unwrap();
    assert_eq!(
        state(old_motion).clip_state,
        ClipState::Purged,
        "motion goes first within a day"
    );
    assert!(
        state(old_motion).snapshot_path.is_some(),
        "pictures are kept"
    );
    assert_eq!(state(old_animal).clip_state, ClipState::Ready);
    assert_eq!(state(new_person).clip_state, ClipState::Ready);
}

#[test]
fn dry_run_changes_nothing() {
    let (dir, store, config) = setup();
    let seg = segment(dir.path(), &store, 7);
    let old = event(dir.path(), &store, Label::Person, 31 * 24, 100);
    let report = run_once(&store, &config, dir.path(), now(), true).unwrap();
    assert_eq!(report.segments_deleted, 1);
    assert_eq!(report.events_purged, 1);
    assert!(
        report.actions.iter().any(|a| a.contains(&seg.path)),
        "{:?}",
        report.actions
    );
    assert!(report.bytes_freed >= 100 + 10 + 100 + 50 + 20);
    assert!(dir.path().join(&seg.path).exists());
    assert_eq!(store.segments_before(now()).unwrap().len(), 1);
    assert_eq!(
        store.get_event(old).unwrap().unwrap().clip_state,
        ClipState::Ready
    );
}

#[test]
fn empty_directories_are_removed() {
    let (dir, store, config) = setup();
    std::fs::create_dir_all(dir.path().join("clips/2026-01-01")).unwrap();
    std::fs::create_dir_all(dir.path().join("thumbs/2026-01-02/nested")).unwrap();
    write(dir.path(), "snapshots/2026-09-19/1.jpg", 10);
    let report = run_once(&store, &config, dir.path(), now(), false).unwrap();
    assert_eq!(report.dirs_removed, 3, "{:?}", report.actions);
    assert!(!dir.path().join("clips/2026-01-01").exists());
    assert!(!dir.path().join("thumbs/2026-01-02").exists());
    assert!(
        dir.path().join("clips").exists(),
        "top-level directories stay"
    );
    assert!(dir.path().join("snapshots/2026-09-19/1.jpg").exists());
}
