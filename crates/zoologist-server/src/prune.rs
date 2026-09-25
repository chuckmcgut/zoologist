//! `zoologist prune`: deletes chosen events together with their clip, snapshot and thumbnail,
//! e.g. a flood of false events from before a fix. Without `yes` it only lists what it would
//! delete. Run it with Zoologist stopped: the database can only be opened once.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use zoologist_core::{Config, Label};
use zoologist_store::{EventRecord, Store};

/// Which events to delete: all of these must match.
pub struct PruneFilter {
    pub cameras: Vec<String>,
    pub labels: Vec<Label>,
    /// Only events whose local (station) date is before this one.
    pub before: NaiveDate,
}

impl PruneFilter {
    fn matches(&self, e: &EventRecord) -> bool {
        self.cameras.contains(&e.camera_id)
            && self.labels.contains(&e.label)
            && e.local_date < self.before
            && e.ended_at.is_some()
    }
}

/// What a prune deleted (or, without `yes`, would delete).
#[derive(Debug, Default, PartialEq)]
pub struct PruneReport {
    pub events: usize,
    pub files: usize,
    pub bytes: u64,
}

pub fn prune(config: &Config, filter: &PruneFilter, yes: bool) -> Result<PruneReport> {
    for camera in &filter.cameras {
        if !config.cameras.iter().any(|c| &c.id == camera) {
            bail!("no camera {camera:?} in the config");
        }
    }
    if filter.cameras.is_empty() || filter.labels.is_empty() {
        bail!("give at least one --camera and one --label");
    }
    let dir = &config.server.data_dir;
    let store = Store::open(&dir.join("zoologist.redb"), config.station.timezone)
        .context("cannot open the database (stop Zoologist first)")?;
    prune_store(&store, dir, filter, yes)
}

pub fn prune_store(
    store: &Store,
    data_dir: &Path,
    filter: &PruneFilter,
    yes: bool,
) -> Result<PruneReport> {
    let mut report = PruneReport::default();
    let chosen: Vec<EventRecord> = store
        .events_between(DateTime::<Utc>::UNIX_EPOCH, None)?
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect();
    for e in &chosen {
        for rel in [&e.clip_path, &e.snapshot_path, &e.thumb_path]
            .into_iter()
            .flatten()
        {
            let path = data_dir.join(rel);
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            report.files += 1;
            report.bytes += meta.len();
            if yes {
                std::fs::remove_file(&path)
                    .with_context(|| format!("cannot delete {}", path.display()))?;
            }
        }
        if yes {
            store.delete_event(e.id)?;
        }
        report.events += 1;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use zoologist_store::{EventPatch, NewEvent};

    use super::*;

    /// Stores an ended event with a clip and a snapshot on disk; returns its id.
    fn event(store: &Store, dir: &Path, camera: &str, label: Label, day: u32) -> u64 {
        let started_at = Utc.with_ymd_and_hms(2026, 9, day, 18, 0, 0).unwrap();
        let name = format!("{camera}-{label}-{day}");
        let e = store
            .insert_event(&NewEvent {
                camera_id: camera.into(),
                label,
                raw_class: None,
                started_at,
                top_score: 0.9,
                median_score: 0.9,
                best_bbox: None,
                snapshot_path: Some(format!("snapshots/{name}.jpg")),
                thumb_path: None,
            })
            .unwrap();
        for rel in [format!("snapshots/{name}.jpg"), format!("clips/{name}.mp4")] {
            std::fs::create_dir_all(dir.join(&rel).parent().unwrap()).unwrap();
            std::fs::write(dir.join(&rel), [0u8; 100]).unwrap();
        }
        let patch = EventPatch {
            ended_at: Some(started_at + chrono::Duration::seconds(30)),
            clip_path: Some(Some(format!("clips/{name}.mp4"))),
            ..Default::default()
        };
        store.update_event(e.id, &patch).unwrap();
        e.id
    }

    #[test]
    fn deletes_only_the_chosen_events_and_their_files() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("zoologist.redb"), "UTC".parse().unwrap()).unwrap();
        let old_truck = event(&store, dir.path(), "yard", Label::Vehicle, 22);
        let old_person = event(&store, dir.path(), "yard", Label::Person, 22);
        let today_truck = event(&store, dir.path(), "yard", Label::Vehicle, 24);
        let other_camera = event(&store, dir.path(), "road", Label::Vehicle, 22);
        let filter = PruneFilter {
            cameras: vec!["yard".into()],
            labels: vec![Label::Vehicle, Label::Motion],
            before: NaiveDate::from_ymd_opt(2026, 9, 24).unwrap(),
        };

        let dry = prune_store(&store, dir.path(), &filter, false).unwrap();
        let expected = PruneReport {
            events: 1,
            files: 2,
            bytes: 200,
        };
        assert_eq!(dry, expected);
        assert!(
            store.get_event(old_truck).unwrap().is_some(),
            "a dry run changes nothing"
        );
        assert!(dir.path().join("clips/yard-vehicle-22.mp4").exists());

        let done = prune_store(&store, dir.path(), &filter, true).unwrap();
        assert_eq!(done, expected);
        assert!(store.get_event(old_truck).unwrap().is_none());
        assert!(!dir.path().join("clips/yard-vehicle-22.mp4").exists());
        assert!(!dir.path().join("snapshots/yard-vehicle-22.jpg").exists());
        for kept in [old_person, today_truck, other_camera] {
            assert!(store.get_event(kept).unwrap().is_some());
        }
        assert!(dir.path().join("clips/yard-person-22.mp4").exists());
    }
}
