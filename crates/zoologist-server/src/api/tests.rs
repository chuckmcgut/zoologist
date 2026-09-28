use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{Duration, Utc};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use tower::ServiceExt;
use zoologist_core::{BBox, Config, Label, SpeciesGuess};
use zoologist_store::{ClipState, EventPatch, NewEvent, Store};

use super::router;
use crate::app::{ApiEvent, AppState};

struct Fixture {
    app: AppState,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.data_dir = dir.path().to_path_buf();
    config.server.static_dir = dir.path().join("static");
    std::fs::create_dir_all(&config.server.static_dir).unwrap();
    std::fs::write(config.server.static_dir.join("index.html"), "<h1>hi</h1>").unwrap();
    let store = Store::open(&dir.path().join("db.redb"), config.station.timezone).unwrap();
    Fixture {
        app: AppState::without_pipeline(config, store),
        _dir: dir,
    }
}

fn new_event(label: Label, minutes_ago: i64) -> NewEvent {
    NewEvent {
        camera_id: "yard".into(),
        label,
        raw_class: None,
        started_at: Utc::now() - Duration::minutes(minutes_ago),
        top_score: 0.8,
        median_score: 0.7,
        best_bbox: Some(BBox {
            x1: 0.1,
            y1: 0.1,
            x2: 0.3,
            y2: 0.3,
        }),
        snapshot_path: None,
        thumb_path: None,
    }
}

/// Inserts an ended fox event with a clip and snapshot on disk; returns its id.
fn insert_fox(app: &AppState) -> u64 {
    let e = app
        .store
        .insert_event(&new_event(Label::Animal, 30))
        .unwrap();
    std::fs::create_dir_all(app.data_dir.join("clips")).unwrap();
    std::fs::write(app.data_dir.join("clips/1.mp4"), vec![7u8; 5000]).unwrap();
    std::fs::write(app.data_dir.join("clips/1.jpg"), b"\xff\xd8jpeg").unwrap();
    let patch = EventPatch {
        ended_at: Some(Utc::now() - Duration::minutes(29)),
        clip_state: Some(ClipState::Ready),
        clip_path: Some(Some("clips/1.mp4".into())),
        clip_bytes: Some(Some(5000)),
        snapshot_path: Some(Some("clips/1.jpg".into())),
        species: Some(SpeciesGuess {
            scientific_name: "Vulpes vulpes".into(),
            common_name: "red fox".into(),
            score: 0.9,
            model_id: "speciesnet".into(),
            candidates: vec![("red fox".into(), 0.9)],
        }),
        ..Default::default()
    };
    app.store.update_event(e.id, &patch).unwrap();
    e.id
}

async fn get(app: &AppState, uri: &str) -> (StatusCode, serde_json::Value) {
    let res = router(app.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn health_and_cameras() {
    let f = fixture();
    let (status, body) = get(&f.app, "/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["uptime_s"].is_i64());
    assert!(body["cpu"]["avx2"].is_boolean());
    assert_eq!(body["decoder"], "rust");
    let (status, body) = get(&f.app, "/api/v1/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["station"]["timezone"], "UTC");
    let (status, body) = get(&f.app, "/api/v1/cameras").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array());
}

#[tokio::test]
async fn health_says_why_species_are_not_named() {
    let mut f = fixture();
    let (_, body) = get(&f.app, "/api/v1/health").await;
    assert!(body["species_problem"].is_null());
    f.app.species_problem = Some("cannot load species model: models/x.txt: not found".into());
    let (_, body) = get(&f.app, "/api/v1/health").await;
    assert_eq!(
        body["species_problem"],
        "cannot load species model: models/x.txt: not found"
    );
    assert!(body["species"].is_null());
}

#[tokio::test]
async fn latest_frame_of_unknown_camera_is_404() {
    let f = fixture();
    let (status, body) = get(&f.app, "/api/v1/cameras/nope/latest.jpg").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn events_list_filters_and_pages() {
    let f = fixture();
    let fox = insert_fox(&f.app);
    for i in 0..5 {
        f.app
            .store
            .insert_event(&new_event(Label::Person, i))
            .unwrap();
    }
    let (status, body) = get(&f.app, "/api/v1/events?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], 6, "newest first");
    assert_eq!(items[0]["active"], true);
    let before = body["next_before_id"].as_u64().unwrap();
    let (_, body) = get(
        &f.app,
        &format!("/api/v1/events?limit=10&before_id={before}"),
    )
    .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 4);

    let (_, body) = get(&f.app, "/api/v1/events?label=animal").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], fox);
    assert_eq!(
        items[0]["clip_url"],
        format!("/api/v1/events/{fox}/clip.mp4")
    );
    assert_eq!(items[0]["thumb_url"], serde_json::Value::Null);
    assert_eq!(items[0]["species"]["common_name"], "red fox");

    let (_, body) = get(&f.app, "/api/v1/events?species=VULPES%20vulpes").await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (_, body) = get(&f.app, "/api/v1/events?order=asc&limit=1").await;
    assert_eq!(body["items"][0]["id"], fox);
}

#[tokio::test]
async fn bad_parameters_are_400() {
    let f = fixture();
    for uri in [
        "/api/v1/events?limit=0",
        "/api/v1/events?limit=abc",
        "/api/v1/events?label=cat",
        "/api/v1/events?order=up",
        "/api/v1/events?after_id=x",
        "/api/v1/events/abc",
        "/api/v1/stats/labels?window=2h",
        "/api/v1/stats/hourly?date=2026-13-01",
    ] {
        let (status, body) = get(&f.app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn missing_things_are_404() {
    let f = fixture();
    let id = f
        .app
        .store
        .insert_event(&new_event(Label::Person, 1))
        .unwrap()
        .id;
    for uri in [
        "/api/v1/events/99".to_string(),
        "/api/v1/events/99/clip.mp4".to_string(),
        format!("/api/v1/events/{id}/clip.mp4"),
        format!("/api/v1/events/{id}/snapshot.jpg"),
        "/api/v1/nothing".to_string(),
    ] {
        let (status, body) = get(&f.app, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn clip_supports_range_requests() {
    let f = fixture();
    let fox = insert_fox(&f.app);
    let res = router(f.app.clone())
        .oneshot(
            Request::get(format!("/api/v1/events/{fox}/clip.mp4"))
                .header(header::RANGE, "bytes=100-199")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(res.headers()[header::CONTENT_RANGE], "bytes 100-199/5000");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 100);

    let res = router(f.app.clone())
        .oneshot(
            Request::get(format!("/api/v1/events/{fox}/thumb.jpg"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "thumb falls back to the snapshot"
    );
    assert_eq!(res.headers()[header::CONTENT_TYPE], "image/jpeg");
}

#[tokio::test]
async fn stats_routes() {
    let f = fixture();
    insert_fox(&f.app);
    f.app
        .store
        .insert_event(&new_event(Label::Person, 1))
        .unwrap();
    f.app
        .store
        .insert_event(&new_event(Label::Animal, 2))
        .unwrap();

    let (status, body) = get(&f.app, "/api/v1/stats/labels?window=1h").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["window"], "1h");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "every label, zeros included");
    assert_eq!(items[0], serde_json::json!({"label": "person", "count": 1}));
    assert_eq!(items[2], serde_json::json!({"label": "animal", "count": 2}));

    let (_, body) = get(&f.app, "/api/v1/stats/species").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|i| i["common_name"] == "red fox"));
    assert!(items.iter().any(|i| i["common_name"].is_null()));

    let (_, body) = get(&f.app, "/api/v1/stats/labels?camera=elsewhere").await;
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["count"] == 0)
    );

    let (status, body) = get(&f.app, "/api/v1/stats/hourly").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hours"].as_array().unwrap().len(), 24);
}

#[tokio::test]
async fn static_ui_is_served() {
    let f = fixture();
    let res = router(f.app.clone())
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"<h1>hi</h1>");
}

/// Without an `index.html` in `static_dir` (e.g. a container run from another directory), the
/// dashboard built into the program is served.
#[tokio::test]
async fn the_built_in_dashboard_is_served_when_static_dir_has_none() {
    let mut f = fixture();
    let mut config = (*f.app.config).clone();
    config.server.static_dir = "does/not/exist".into();
    f.app.config = std::sync::Arc::new(config);
    for (uri, status, content_type, contains) in [
        ("/", StatusCode::OK, "text/html", "<title>Zoologist</title>"),
        (
            "/index.html",
            StatusCode::OK,
            "text/html",
            "<title>Zoologist</title>",
        ),
        ("/app.js", StatusCode::OK, "text/javascript", "loadEvents"),
        ("/style.css", StatusCode::OK, "text/css", "dialog"),
        ("/favicon.svg", StatusCode::OK, "image/svg+xml", "<svg"),
        ("/../Cargo.toml", StatusCode::NOT_FOUND, "text/plain", ""),
    ] {
        let res = router(f.app.clone())
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), status, "{uri}");
        let ct = res.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with(content_type), "{uri}: {ct}");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains(contains), "{uri}");
    }
}

#[tokio::test]
async fn the_window_limits_the_event_list() {
    let f = fixture();
    for minutes_ago in [10, 3 * 60, 3 * 24 * 60] {
        f.app
            .store
            .insert_event(&new_event(Label::Person, minutes_ago))
            .unwrap();
    }
    let count = |body: serde_json::Value| body["items"].as_array().unwrap().len();
    let (_, all) = get(&f.app, "/api/v1/events").await;
    assert_eq!(count(all), 3, "no window: everything");
    for (window, expected) in [("1h", 1), ("6h", 2), ("24h", 2), ("7d", 3)] {
        let (status, body) = get(&f.app, &format!("/api/v1/events?window={window}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(count(body), expected, "{window}");
    }
    let (status, _) = get(&f.app, "/api/v1/events?window=2h").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn send(
    app: &AppState,
    method: &str,
    uri: &str,
    content_type: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let res = router(app.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn an_event_can_be_marked_wrong_and_the_mark_taken_back() {
    let f = fixture();
    let e = f
        .app
        .store
        .insert_event(&new_event(Label::Animal, 5))
        .unwrap();
    let other = f
        .app
        .store
        .insert_event(&new_event(Label::Animal, 6))
        .unwrap();
    let uri = format!("/api/v1/events/{}/feedback", e.id);
    let json = "application/json";

    let (status, body) = send(
        &f.app,
        "POST",
        &uri,
        json,
        r#"{"actual":"nothing","note":" dark stump "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["feedback"]["actual"].is_null(),
        "nothing there: {body}"
    );
    assert_eq!(body["feedback"]["note"], "dark stump");
    assert_eq!(body["label"], "animal", "the event itself is unchanged");

    let (_, wrong) = get(&f.app, "/api/v1/events?wrong=true").await;
    let ids: Vec<u64> = wrong["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![e.id], "only the marked one, not {}", other.id);

    let (status, body) = send(
        &f.app,
        "POST",
        &uri,
        json,
        r#"{"actual":"animal","species":"American crow"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["feedback"]["actual"], "animal");
    assert_eq!(body["feedback"]["species"], "American crow");

    let (status, _) = send(&f.app, "POST", &uri, json, r#"{"actual":"dragon"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A form another web page could post without asking: refused.
    let (status, _) = send(
        &f.app,
        "POST",
        &uri,
        "text/plain",
        r#"{"actual":"nothing"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let (status, body) = send(&f.app, "DELETE", &uri, json, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("feedback").is_none_or(|v| v.is_null()), "{body}");
    let (_, wrong) = get(&f.app, "/api/v1/events?wrong=true").await;
    assert!(wrong["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn reclassify_needs_an_animal_with_a_clip_and_the_models() {
    let f = fixture();
    let person = f
        .app
        .store
        .insert_event(&new_event(Label::Person, 5))
        .unwrap();
    let json = "application/json";
    let (status, _) = send(
        &f.app,
        "POST",
        &format!("/api/v1/events/{}/reclassify", person.id),
        json,
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "not an animal");
    let fox = insert_fox(&f.app);
    let (status, body) = send(
        &f.app,
        "POST",
        &format!("/api/v1/events/{fox}/reclassify"),
        json,
        r#"{"store":true}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no models in this test: {body}"
    );
}

/// Reads SSE frames from a response body until `n` data frames arrived.
async fn read_sse(body: Body, n: usize) -> Vec<(String, String, serde_json::Value)> {
    let mut stream = body.into_data_stream();
    let mut text = String::new();
    let mut out = Vec::new();
    while out.len() < n {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("SSE message in time")
            .expect("stream open")
            .unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = text.find("\n\n") {
            let frame: String = text.drain(..end + 2).collect();
            let field = |name: &str| {
                frame
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                    .unwrap_or_default()
                    .to_string()
            };
            let data = field("data");
            if !data.is_empty() {
                out.push((
                    field("event"),
                    field("id"),
                    serde_json::from_str(&data).unwrap(),
                ));
            }
        }
    }
    out
}

#[tokio::test]
async fn stream_sends_live_events() {
    let f = fixture();
    let res = router(f.app.clone())
        .oneshot(Request::get("/api/v1/stream").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
    let record = f
        .app
        .store
        .insert_event(&new_event(Label::Vehicle, 0))
        .unwrap();
    f.app.publish(ApiEvent::Started(record.clone()));
    f.app.publish(ApiEvent::Ended(record));
    let got = read_sse(res.into_body(), 2).await;
    assert_eq!(got[0].0, "started");
    assert_eq!(got[0].1, "1");
    assert_eq!(got[0].2["label"], "vehicle");
    assert_eq!(got[1].0, "ended");
}

#[tokio::test]
async fn stream_replays_after_last_event_id() {
    let f = fixture();
    for i in 0..3 {
        f.app
            .store
            .insert_event(&new_event(Label::Person, i))
            .unwrap();
    }
    let res = router(f.app.clone())
        .oneshot(
            Request::get("/api/v1/stream")
                .header("Last-Event-ID", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let live = f
        .app
        .store
        .insert_event(&new_event(Label::Animal, 0))
        .unwrap();
    f.app.publish(ApiEvent::Started(live));
    let got = read_sse(res.into_body(), 3).await;
    let ids: Vec<&str> = got.iter().map(|g| g.1.as_str()).collect();
    assert_eq!(ids, ["2", "3", "4"]);
    assert_eq!(got[0].0, "started", "open events replay as started");
}
