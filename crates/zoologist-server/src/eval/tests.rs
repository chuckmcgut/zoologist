use chrono::{Duration, Utc};
use zoologist_core::{BBox, Config, Label, SpeciesGuess};
use zoologist_store::{ClipState, EventPatch, EventRecord, Feedback, NewEvent, Store};

use super::*;
use crate::app::AppState;

fn found(label: Label, species: Option<&str>) -> Found {
    Found {
        label,
        detected: label,
        species: species.map(Into::into),
        note: None,
        start_s: 0.0,
        duration_s: Some(5.0),
        score: 0.9,
    }
}

fn record(id: u64, label: Label, species: Option<&str>, feedback: Option<Feedback>) -> EventRecord {
    let at = Utc::now();
    EventRecord {
        id,
        camera_id: "yard".into(),
        label,
        raw_class: None,
        started_at: at,
        ended_at: Some(at),
        local_date: at.date_naive(),
        local_hour: 0,
        top_score: 0.9,
        median_score: 0.8,
        best_bbox: Some(BBox {
            x1: 0.1,
            y1: 0.1,
            x2: 0.3,
            y2: 0.3,
        }),
        species: species.map(guess),
        snapshot_path: None,
        thumb_path: None,
        sharp_path: None,
        clip_path: None,
        clip_bytes: None,
        clip_state: ClipState::Ready,
        feedback,
    }
}

fn guess(name: &str) -> SpeciesGuess {
    SpeciesGuess {
        scientific_name: String::new(),
        common_name: name.into(),
        score: 0.9,
        model_id: "speciesnet".into(),
        candidates: vec![],
    }
}

fn mark(actual: Option<Label>, species: Option<&str>) -> Option<Feedback> {
    Some(Feedback {
        actual,
        species: species.map(Into::into),
        note: None,
        at: Utc::now(),
    })
}

#[test]
fn marked_nothing_passes_only_without_objects() {
    let expect = Expect::for_event(&record(1, Label::Person, None, mark(None, None)));
    assert_eq!(expect, Expect::Nothing);
    assert!(expect.met_by(&[], true));
    assert!(expect.met_by(&[found(Label::Motion, None)], true));
    assert!(!expect.met_by(&[found(Label::Person, None)], true));
    assert!(
        !expect.met_by(&[found(Label::Animal, None)], true),
        "any object"
    );
}

#[test]
fn a_corrected_label_must_replace_the_wrong_one() {
    let expect = Expect::for_event(&record(
        1,
        Label::Animal,
        None,
        mark(Some(Label::Person), None),
    ));
    assert!(expect.met_by(&[found(Label::Person, None)], true));
    assert!(!expect.met_by(&[found(Label::Animal, None)], true));
    assert!(
        !expect.met_by(
            &[found(Label::Person, None), found(Label::Animal, None)],
            true
        ),
        "the wrong label is still there"
    );
}

#[test]
fn a_corrected_species_is_matched_loosely_and_only_when_named() {
    let e = record(
        1,
        Label::Animal,
        Some("gray fox"),
        mark(Some(Label::Animal), Some("Deer")),
    );
    let expect = Expect::for_event(&e);
    assert!(expect.met_by(&[found(Label::Animal, Some("white-tailed deer"))], true));
    assert!(!expect.met_by(&[found(Label::Animal, Some("gray fox"))], true));
    assert!(
        expect.met_by(&[found(Label::Animal, None)], false),
        "without the classifier only the label counts"
    );
}

#[test]
fn a_wrong_name_without_the_right_one_must_go() {
    let e = record(
        1,
        Label::Animal,
        Some("gray fox"),
        mark(Some(Label::Animal), None),
    );
    let expect = Expect::for_event(&e);
    assert!(!expect.met_by(&[found(Label::Animal, Some("gray fox"))], true));
    assert!(expect.met_by(&[found(Label::Animal, Some("red fox"))], true));
}

#[test]
fn unmarked_events_expect_what_was_said() {
    let fox = Expect::for_event(&record(1, Label::Animal, Some("red fox"), None));
    assert!(fox.met_by(&[found(Label::Animal, Some("red fox"))], true));
    assert!(!fox.met_by(&[found(Label::Motion, None)], true));
    let motion = Expect::for_event(&record(2, Label::Motion, None, None));
    assert_eq!(motion, Expect::Nothing);
}

#[test]
fn an_event_never_matches_its_own_mark() {
    let mut corpus = BTreeMap::new();
    let chair = record(1, Label::Person, None, mark(None, None));
    let bbox = chair.best_bbox.unwrap();
    corpus.insert(1, chair);
    assert!(!in_marked_spot(&corpus, 1, "yard", &bbox));
    assert!(in_marked_spot(&corpus, 2, "yard", &bbox));
    assert!(
        !in_marked_spot(&corpus, 2, "drive", &bbox),
        "marks are per camera"
    );
    let elsewhere = BBox {
        x1: 0.6,
        y1: 0.6,
        x2: 0.9,
        y2: 0.9,
    };
    assert!(!in_marked_spot(&corpus, 2, "yard", &elsewhere));
}

/// Stores an ended event on `camera`, `hours_ago`, with a clip file unless `clip` is false.
fn stored(app: &AppState, camera: &str, label: Label, hours_ago: i64, clip: bool) -> u64 {
    let started_at = Utc::now() - Duration::hours(hours_ago);
    let e = app
        .store
        .insert_event(&NewEvent {
            camera_id: camera.into(),
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
    let rel = format!("clips/{}.mp4", e.id);
    let mut patch = EventPatch {
        ended_at: Some(started_at + Duration::seconds(20)),
        clip_state: Some(ClipState::Failed),
        ..Default::default()
    };
    if clip {
        std::fs::create_dir_all(app.data_dir.join("clips")).unwrap();
        std::fs::write(app.data_dir.join(&rel), vec![e.id as u8; 3000]).unwrap();
        patch.clip_state = Some(ClipState::Ready);
        patch.clip_path = Some(Some(rel));
        patch.clip_bytes = Some(Some(3000));
    }
    app.store.update_event(e.id, &patch).unwrap();
    e.id
}

fn set_mark(app: &AppState, id: u64, feedback: Option<Feedback>) {
    let patch = EventPatch {
        feedback: Some(feedback),
        ..Default::default()
    };
    app.store.update_event(id, &patch).unwrap();
}

#[test]
fn corpus_copies_marks_and_a_spread_of_settled_events() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.data_dir = dir.path().join("data");
    std::fs::create_dir_all(&config.server.data_dir).unwrap();
    let store = Store::open(&config.server.data_dir.join("db.redb"), chrono_tz::UTC).unwrap();
    let app = AppState::without_pipeline(config, store);

    let chair = stored(&app, "yard", Label::Person, 48, true);
    set_mark(&app, chair, mark(None, None));
    let no_clip = stored(&app, "yard", Label::Animal, 48, false);
    set_mark(&app, no_clip, mark(None, None));
    let fox = stored(&app, "yard", Label::Animal, 30, true);
    let car = stored(&app, "drive", Label::Vehicle, 30, true);
    let car2 = stored(&app, "drive", Label::Vehicle, 40, true);
    let fresh = stored(&app, "yard", Label::Person, 1, true);

    // A real server: the export goes over HTTP.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router(app.clone());
    runtime.spawn(async move { axum::serve(listener, router).await });

    let out = dir.path().join("corpus");
    let report = export(&server, &out, 2, 1000).unwrap();
    assert_eq!(report.marked_new, 1);
    assert_eq!(report.no_clip, 1);
    assert_eq!(report.unmarked_new, 2);
    let corpus = load_corpus(&out).unwrap();
    assert!(corpus.contains_key(&no_clip), "its spot still counts");
    assert!(!out.join(format!("{no_clip}.mp4")).exists());
    assert_eq!(
        std::fs::read(out.join(format!("{chair}.mp4"))).unwrap(),
        vec![chair as u8; 3000]
    );
    // One per camera and label before a second of the same kind; never the fresh one.
    assert!(corpus.contains_key(&fox));
    assert!(corpus.contains_key(&car) ^ corpus.contains_key(&car2));
    assert!(!corpus.contains_key(&fresh));

    // Again: a new mark is added, a taken-back one refreshed, the sample is kept.
    set_mark(&app, fox, mark(Some(Label::Person), None));
    set_mark(&app, chair, None);
    let again = export(&server, &out, 2, 1000).unwrap();
    assert_eq!(again.refreshed, 2, "{again:?}");
    assert_eq!(
        again.unmarked_new, 0,
        "the car and the chair are the sample now"
    );
    let corpus = load_corpus(&out).unwrap();
    assert!(corpus[&chair].feedback.is_none());
    assert_eq!(
        corpus[&fox].feedback.as_ref().unwrap().actual,
        Some(Label::Person)
    );
}

#[test]
fn repeated_finds_are_counted() {
    let person = found(Label::Person, None);
    let fox = found(Label::Animal, Some("red fox"));
    assert_eq!(
        summarise(&[&person, &fox, &person, &person]),
        "person ×3, animal (red fox)"
    );
}
