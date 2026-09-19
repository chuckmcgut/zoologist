use chrono::{Duration, TimeZone};
use zoologist_core::SpeciesGuess;

use super::*;

fn tz() -> Tz {
    "America/New_York".parse().unwrap()
}

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("zoologist.redb"), tz()).unwrap();
    (dir, store)
}

fn at(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 18, h, m, 0).unwrap()
}

fn new_event(camera: &str, label: Label, started_at: DateTime<Utc>) -> NewEvent {
    NewEvent {
        camera_id: camera.into(),
        label,
        raw_class: Some(label.as_str().into()),
        started_at,
        top_score: 0.8,
        median_score: 0.7,
        best_bbox: None,
        snapshot_path: None,
        thumb_path: None,
    }
}

fn species(common: &str, score: f32) -> SpeciesGuess {
    SpeciesGuess {
        scientific_name: format!("{common} sci"),
        common_name: common.into(),
        score,
        model_id: "speciesnet".into(),
        candidates: vec![(common.into(), score)],
    }
}

#[test]
fn ids_increase_and_local_time_is_filled_in() {
    let (_dir, store) = open();
    let a = store
        .insert_event(&new_event("drive", Label::Person, at(14, 0)))
        .unwrap();
    let b = store
        .insert_event(&new_event("drive", Label::Animal, at(3, 30)))
        .unwrap();
    assert_eq!((a.id, b.id), (1, 2));
    // 14:00 UTC is 10:00 EDT; 03:30 UTC is 23:30 EDT the previous day.
    assert_eq!(
        (a.local_date, a.local_hour),
        (NaiveDate::from_ymd_opt(2026, 9, 18).unwrap(), 10)
    );
    assert_eq!(
        (b.local_date, b.local_hour),
        (NaiveDate::from_ymd_opt(2026, 9, 17).unwrap(), 23)
    );
    assert_eq!(a.clip_state, ClipState::Pending);
    assert_eq!(store.get_event(1).unwrap().unwrap(), a);
    assert!(store.get_event(99).unwrap().is_none());
}

#[test]
fn ids_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.redb");
    {
        let store = Store::open(&path, tz()).unwrap();
        store
            .insert_event(&new_event("c", Label::Person, at(1, 0)))
            .unwrap();
    }
    let store = Store::open(&path, tz()).unwrap();
    let e = store
        .insert_event(&new_event("c", Label::Person, at(2, 0)))
        .unwrap();
    assert_eq!(e.id, 2);
}

#[test]
fn patches_update_and_clear_fields() {
    let (_dir, store) = open();
    let e = store
        .insert_event(&new_event("c", Label::Animal, at(12, 0)))
        .unwrap();
    let patch = EventPatch {
        ended_at: Some(at(12, 1)),
        species: Some(species("Red Fox", 0.9)),
        clip_path: Some(Some("clips/1.mp4".into())),
        clip_bytes: Some(Some(1234)),
        clip_state: Some(ClipState::Ready),
        ..Default::default()
    };
    let updated = store.update_event(e.id, &patch).unwrap().unwrap();
    assert_eq!(updated.ended_at, Some(at(12, 1)));
    assert_eq!(updated.species.as_ref().unwrap().common_name, "Red Fox");
    assert_eq!(updated.clip_path.as_deref(), Some("clips/1.mp4"));
    assert_eq!(updated.top_score, e.top_score, "untouched fields stay");

    let purge = EventPatch {
        clip_path: Some(None),
        clip_bytes: Some(None),
        clip_state: Some(ClipState::Purged),
        ..Default::default()
    };
    let purged = store.update_event(e.id, &purge).unwrap().unwrap();
    assert_eq!((purged.clip_path, purged.clip_bytes), (None, None));
    assert!(store.update_event(42, &purge).unwrap().is_none());
}

#[test]
fn pagination_in_both_directions() {
    let (_dir, store) = open();
    for i in 0..7 {
        store
            .insert_event(&new_event("c", Label::Person, at(10, i)))
            .unwrap();
    }
    let ids = |p: &Page| p.items.iter().map(|e| e.id).collect::<Vec<_>>();

    let q = EventQuery {
        limit: 3,
        ..Default::default()
    };
    let first = store.list_events(&q).unwrap();
    assert_eq!(ids(&first), vec![7, 6, 5]);
    let second = store
        .list_events(&EventQuery {
            before_id: first.next_before_id,
            ..q.clone()
        })
        .unwrap();
    assert_eq!(ids(&second), vec![4, 3, 2]);

    let asc = EventQuery {
        limit: 3,
        order: Order::Asc,
        after_id: Some(2),
        ..Default::default()
    };
    let page = store.list_events(&asc).unwrap();
    assert_eq!(ids(&page), vec![3, 4, 5]);
    assert_eq!(page.next_after_id, Some(5));
    let rest = store
        .list_events(&EventQuery {
            after_id: Some(5),
            ..asc
        })
        .unwrap();
    assert_eq!(ids(&rest), vec![6, 7]);
    let empty = store
        .list_events(&EventQuery {
            after_id: Some(7),
            order: Order::Asc,
            ..Default::default()
        })
        .unwrap();
    assert!(empty.items.is_empty() && empty.next_after_id.is_none());
}

#[test]
fn filters_by_camera_label_and_species() {
    let (_dir, store) = open();
    store
        .insert_event(&new_event("drive", Label::Person, at(10, 0)))
        .unwrap();
    let fox = store
        .insert_event(&new_event("yard", Label::Animal, at(10, 1)))
        .unwrap();
    store
        .insert_event(&new_event("yard", Label::Vehicle, at(10, 2)))
        .unwrap();
    store
        .update_event(
            fox.id,
            &EventPatch {
                species: Some(species("Red Fox", 0.8)),
                ..Default::default()
            },
        )
        .unwrap();
    let count = |q: EventQuery| store.list_events(&q).unwrap().items.len();
    assert_eq!(
        count(EventQuery {
            camera: Some("yard".into()),
            ..Default::default()
        }),
        2
    );
    assert_eq!(
        count(EventQuery {
            label: Some(Label::Person),
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        count(EventQuery {
            species: Some("red fox".into()),
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        count(EventQuery {
            species: Some("Red Fox sci".into()),
            ..Default::default()
        }),
        1
    );
    assert_eq!(
        count(EventQuery {
            species: Some("Coyote".into()),
            ..Default::default()
        }),
        0
    );
}

#[test]
fn label_and_species_stats() {
    let (_dir, store) = open();
    for (i, label) in [Label::Person, Label::Animal, Label::Animal, Label::Motion]
        .into_iter()
        .enumerate()
    {
        store
            .insert_event(&new_event("c", label, at(9, i as u32)))
            .unwrap();
    }
    let deer = store
        .insert_event(&new_event("c", Label::Animal, at(9, 10)))
        .unwrap();
    store
        .update_event(
            deer.id,
            &EventPatch {
                species: Some(species("White-tailed Deer", 0.95)),
                clip_state: Some(ClipState::Ready),
                ..Default::default()
            },
        )
        .unwrap();
    // Before the window: not counted.
    store
        .insert_event(&new_event("c", Label::Person, at(1, 0)))
        .unwrap();

    let labels = store.stats_by_label(at(8, 0), None).unwrap();
    assert_eq!(
        labels,
        vec![(Label::Animal, 3), (Label::Person, 1), (Label::Motion, 1)]
    );
    assert!(
        store
            .stats_by_label(at(8, 0), Some("other"))
            .unwrap()
            .is_empty()
    );

    let species_stats = store.stats_by_species(at(8, 0), None).unwrap();
    assert_eq!(species_stats.len(), 2);
    assert_eq!(species_stats[0].common_name, None); // two unidentified animals
    assert_eq!(species_stats[0].count, 2);
    assert_eq!(
        species_stats[1].common_name.as_deref(),
        Some("White-tailed Deer")
    );
    assert_eq!(species_stats[1].best_event_id, deer.id);
}

#[test]
fn species_names_group_ignoring_case() {
    let (_dir, store) = open();
    for (i, name) in ["Red fox", "red fox", "Coyote"].into_iter().enumerate() {
        let e = store
            .insert_event(&new_event("c", Label::Animal, at(9, i as u32)))
            .unwrap();
        let patch = EventPatch {
            species: Some(species(name, 0.9)),
            ..Default::default()
        };
        store.update_event(e.id, &patch).unwrap();
    }
    let stats = store.stats_by_species(at(8, 0), None).unwrap();
    assert_eq!(stats.len(), 2);
    assert_eq!(stats[0].common_name.as_deref(), Some("Red fox"));
    assert_eq!(stats[0].count, 2);
}

#[test]
fn hourly_buckets_use_the_station_day() {
    let (_dir, store) = open();
    // Sept 18 local (EDT, UTC-4): 00:30 local = 04:30 UTC, 23:30 local = 03:30 UTC next day.
    store
        .insert_event(&new_event("c", Label::Person, at(4, 30)))
        .unwrap();
    store
        .insert_event(&new_event(
            "c",
            Label::Animal,
            Utc.with_ymd_and_hms(2026, 9, 19, 3, 30, 0).unwrap(),
        ))
        .unwrap();
    // 03:30 UTC Sept 18 is still Sept 17 locally.
    store
        .insert_event(&new_event("c", Label::Vehicle, at(3, 30)))
        .unwrap();
    let day = NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
    let hours = store.stats_hourly(day, None).unwrap();
    assert_eq!(hours[0].person, 1);
    assert_eq!(hours[23].animal, 1);
    let total: u64 = hours
        .iter()
        .map(|h| h.person + h.vehicle + h.animal + h.motion)
        .sum();
    assert_eq!(total, 2);
}

#[test]
fn hourly_buckets_across_the_dst_change() {
    let (_dir, store) = open();
    // 2026-11-01 01:30 happens twice in New York; both are hour 1 of that day.
    let first = Utc.with_ymd_and_hms(2026, 11, 1, 5, 30, 0).unwrap(); // 01:30 EDT
    let second = Utc.with_ymd_and_hms(2026, 11, 1, 6, 30, 0).unwrap(); // 01:30 EST
    store
        .insert_event(&new_event("c", Label::Motion, first))
        .unwrap();
    store
        .insert_event(&new_event("c", Label::Motion, second))
        .unwrap();
    let hours = store
        .stats_hourly(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), None)
        .unwrap();
    assert_eq!(hours[1].motion, 2);
}

#[test]
fn dangling_events_are_closed_on_restart() {
    let (_dir, store) = open();
    let open_event = store
        .insert_event(&new_event("c", Label::Person, at(5, 0)))
        .unwrap();
    let done = store
        .insert_event(&new_event("c", Label::Person, at(6, 0)))
        .unwrap();
    store
        .update_event(
            done.id,
            &EventPatch {
                ended_at: Some(at(6, 1)),
                clip_state: Some(ClipState::Ready),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(store.close_dangling_events().unwrap(), 1);
    let closed = store.get_event(open_event.id).unwrap().unwrap();
    assert_eq!(closed.ended_at, Some(closed.started_at));
    assert_eq!(closed.clip_state, ClipState::Failed);
    assert_eq!(store.close_dangling_events().unwrap(), 0);
}

#[test]
fn segments_overlapping_a_window() {
    let (_dir, store) = open();
    for i in 0..6 {
        let start = at(12, 0) + Duration::seconds(10 * i);
        store
            .insert_segment(
                "cam",
                &SegmentRecord {
                    path: format!("rec/{i}.mp4"),
                    index_path: format!("rec/{i}.idx"),
                    started_at: start,
                    ended_at: start + Duration::seconds(10),
                    bytes: 100,
                },
            )
            .unwrap();
    }
    let paths = |v: Vec<SegmentRecord>| v.into_iter().map(|s| s.path).collect::<Vec<_>>();
    // 12:00:15 – 12:00:32 touches segments 1, 2 and 3.
    let found = store
        .segments_between(
            "cam",
            at(12, 0) + Duration::seconds(15),
            at(12, 0) + Duration::seconds(32),
        )
        .unwrap();
    assert_eq!(paths(found), vec!["rec/1.mp4", "rec/2.mp4", "rec/3.mp4"]);
    assert!(
        store
            .segments_between("other", at(11, 0), at(13, 0))
            .unwrap()
            .is_empty()
    );

    let old = store
        .segments_before(at(12, 0) + Duration::seconds(20))
        .unwrap();
    assert_eq!(old.len(), 2);
    store.delete_segment("cam", at(12, 0)).unwrap();
    assert_eq!(store.segments_before(at(13, 0)).unwrap().len(), 5);
}

#[test]
fn hub_imports_are_remembered() {
    let (_dir, store) = open();
    assert!(
        !store
            .hub_import_seen("hub", "Rec_20260918_120000.mp4")
            .unwrap()
    );
    store
        .record_hub_import("hub", "Rec_20260918_120000.mp4", Some(7))
        .unwrap();
    store
        .record_hub_import("hub", "Rec_20260918_130000.mp4", None)
        .unwrap();
    assert!(
        store
            .hub_import_seen("hub", "Rec_20260918_120000.mp4")
            .unwrap()
    );
    assert!(
        store
            .hub_import_seen("hub", "Rec_20260918_130000.mp4")
            .unwrap()
    );
    assert!(
        !store
            .hub_import_seen("other-hub", "Rec_20260918_120000.mp4")
            .unwrap()
    );
}

#[tokio::test]
async fn call_runs_on_a_blocking_thread() {
    let (_dir, store) = open();
    let e = store
        .call(|s| s.insert_event(&new_event("c", Label::Person, at(10, 0))))
        .await
        .unwrap();
    assert_eq!(e.id, 1);
}
