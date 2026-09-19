use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::TimeZone;
use chrono_tz::America::New_York;

use super::*;

#[test]
fn hub_times_use_the_station_time_zone() {
    let v = json!({"year": 2026, "mon": 9, "day": 19, "hour": 8, "min": 30, "sec": 5});
    let t = parse_hub_time(&v, New_York).unwrap();
    assert_eq!(t, Utc.with_ymd_and_hms(2026, 9, 19, 12, 30, 5).unwrap()); // EDT = UTC-4
    assert_eq!(hub_time(t, New_York), v);
    // Winter: EST = UTC-5.
    let w = json!({"year": 2026, "mon": 1, "day": 2, "hour": 23, "min": 59, "sec": 59});
    assert_eq!(
        parse_hub_time(&w, New_York).unwrap(),
        Utc.with_ymd_and_hms(2026, 1, 3, 4, 59, 59).unwrap()
    );
    // The skipped hour of spring forward does not exist.
    let gap = json!({"year": 2026, "mon": 3, "day": 8, "hour": 2, "min": 30, "sec": 0});
    assert_eq!(parse_hub_time(&gap, New_York), None);
    assert_eq!(parse_hub_time(&json!({"year": 2026}), New_York), None);
}

#[test]
fn searches_are_split_at_local_midnight() {
    // 22:00 on the 18th to 02:00 on the 19th, New York time.
    let from = Utc.with_ymd_and_hms(2026, 9, 19, 2, 0, 0).unwrap();
    let to = Utc.with_ymd_and_hms(2026, 9, 19, 6, 0, 0).unwrap();
    let days = split_days(from, to, New_York);
    assert_eq!(days.len(), 2);
    let local = |t: DateTime<Utc>| t.with_timezone(&New_York).format("%d %H:%M:%S").to_string();
    assert_eq!(
        (local(days[0].0), local(days[0].1)),
        ("18 22:00:00".into(), "18 23:59:59".into())
    );
    assert_eq!(
        (local(days[1].0), local(days[1].1)),
        ("19 00:00:00".into(), "19 01:59:59".into())
    );
    assert!(split_days(to, to, New_York).is_empty());
}

#[test]
fn tokens_and_passwords_are_removed() {
    let v = json!([{"cmd": "Login", "value": {"Token": {"leaseTime": 3600, "name": "abc123"}}},
                   {"User": {"userName": "z", "password": "secret"}}]);
    let text = redact(v).to_string();
    assert!(
        !text.contains("abc123") && !text.contains("secret"),
        "{text}"
    );
    assert!(text.contains("3600"));
    assert_eq!(
        scrub("GET http://hub/cgi-bin/api.cgi?cmd=Download&token=abc123&x=1 failed"),
        "GET http://hub/cgi-bin/api.cgi?cmd=Download&token=REDACTED&x=1 failed"
    );
}

#[test]
fn search_answers_are_parsed() {
    let value = json!({"SearchResult": {"channel": 0, "File": [
        {"name": "Mp4Record/2026-09-19/RecS03_20260919_083005_083041.mp4", "type": "sub",
         "size": 1234567, "width": 640, "height": 360, "frameRate": 15,
         "StartTime": {"year": 2026, "mon": 9, "day": 19, "hour": 8, "min": 30, "sec": 5},
         "EndTime": {"year": 2026, "mon": 9, "day": 19, "hour": 8, "min": 30, "sec": 41}},
        {"name": "bad", "StartTime": {}}
    ]}});
    let files = parse_search(&value, "sub", New_York);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].size, 1234567);
    assert_eq!(files[0].stream, "sub");
    assert_eq!((files[0].end - files[0].start).num_seconds(), 36);
}

/// A fake Hub: logins hand out numbered tokens; `expire` makes the next call answer
/// "please login first" once.
#[derive(Clone, Default)]
struct Fake {
    logins: Arc<AtomicUsize>,
    expire: Arc<Mutex<bool>>,
    last_search: Arc<Mutex<Option<serde_json::Value>>>,
}

fn answer(cmd: &str, value: serde_json::Value) -> Response {
    axum::Json(json!([{"cmd": cmd, "code": 0, "value": value}])).into_response()
}

fn error(cmd: &str, code: i64, detail: &str) -> Response {
    axum::Json(json!([{"cmd": cmd, "code": 1, "error": {"rspCode": code, "detail": detail}}]))
        .into_response()
}

async fn api(
    State(fake): State<Fake>,
    Query(q): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let cmd = q.get("cmd").cloned().unwrap_or_default();
    if cmd == "Login" {
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if req[0]["param"]["User"]["password"] != "pw" {
            return error(&cmd, -7, "login failed");
        }
        let n = fake.logins.fetch_add(1, Ordering::SeqCst) + 1;
        return answer(
            &cmd,
            json!({"Token": {"leaseTime": 3600, "name": format!("tok{n}")}}),
        );
    }
    let expected = format!("tok{}", fake.logins.load(Ordering::SeqCst));
    let expired = std::mem::take(&mut *fake.expire.lock().unwrap());
    if expired || q.get("token") != Some(&expected) {
        return error(&cmd, -6, "please login first");
    }
    match cmd.as_str() {
        "GetChannelstatus" => answer(
            &cmd,
            json!({"count": 2, "status": [
            {"channel": 0, "name": "Trail", "online": 1, "sleep": 1, "uid": "X"},
            {"channel": 1, "name": "Garden", "online": 0, "sleep": 0, "uid": "Y"}]}),
        ),
        "Search" => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            *fake.last_search.lock().unwrap() = Some(req[0]["param"]["Search"].clone());
            answer(
                &cmd,
                json!({"SearchResult": {"File": [
                {"name": "Mp4Record/2026-09-19/RecS03_a.mp4", "type": "sub", "size": 100000,
                 "StartTime": {"year": 2026, "mon": 9, "day": 19, "hour": 8, "min": 30, "sec": 5},
                 "EndTime": {"year": 2026, "mon": 9, "day": 19, "hour": 8, "min": 30, "sec": 41}}]}}),
            )
        }
        "Logout" => answer(&cmd, json!({})),
        _ => error(&cmd, -9, "not supported"),
    }
}

async fn download(State(fake): State<Fake>, Query(q): Query<HashMap<String, String>>) -> Response {
    let expected = format!("tok{}", fake.logins.load(Ordering::SeqCst));
    if q.get("token") != Some(&expected) {
        return error("Download", -6, "please login first");
    }
    if q.get("source").map(String::as_str) != Some("Mp4Record/2026-09-19/RecS03_a.mp4") {
        return error("Download", -12, "file not found");
    }
    (
        [(header::CONTENT_TYPE, "application/octet-stream")],
        vec![7u8; 100_000],
    )
        .into_response()
}

async fn dispatch(
    state: State<Fake>,
    q: Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    if q.get("cmd").map(String::as_str) == Some("Download") {
        download(state, q).await
    } else {
        api(state, q, body).await
    }
}

/// Starts the fake Hub on a background runtime; returns its base URL.
fn start_fake(fake: Fake) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = Router::new()
                .route("/cgi-bin/api.cgi", post(dispatch).get(dispatch))
                .with_state(fake);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

#[test]
fn talks_to_a_fake_hub() {
    let fake = Fake::default();
    let base = start_fake(fake.clone());

    let mut bad = HubClient::new(&base, "zoologist", "wrong");
    let err = bad.channels().unwrap_err();
    assert!(matches!(err, HubError::Api { code: -7, .. }), "{err}");

    let mut hub = HubClient::new(&base, "zoologist", "pw");
    let dir = tempfile::tempdir().unwrap();
    hub.record_to(dir.path()).unwrap();
    let channels = hub.channels().unwrap();
    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].name, "Trail");
    assert!(channels[0].online && !channels[1].online);
    assert!(channels[0].sleeping && !channels[1].sleeping);
    assert_eq!(fake.logins.load(Ordering::SeqCst), 1);

    // The Hub forgets the token: the client logs in again and retries once.
    *fake.expire.lock().unwrap() = true;
    let from = Utc.with_ymd_and_hms(2026, 9, 19, 12, 0, 0).unwrap();
    let files = hub
        .search(0, "sub", from, from + chrono::Duration::hours(1), New_York)
        .unwrap();
    assert_eq!(fake.logins.load(Ordering::SeqCst), 2);
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].start,
        Utc.with_ymd_and_hms(2026, 9, 19, 12, 30, 5).unwrap()
    );
    let sent = fake.last_search.lock().unwrap().clone().unwrap();
    assert_eq!(
        sent["StartTime"]["hour"], 8,
        "search times are sent in Hub (local) time"
    );
    assert_eq!(sent["streamType"], "sub");

    let dest = dir.path().join("clip.mp4");
    assert_eq!(hub.download(&files[0], &dest).unwrap(), 100_000);
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), 100_000);
    let missing = HubFile {
        name: "missing.mp4".into(),
        ..files[0].clone()
    };
    let err = hub
        .download(&missing, &dir.path().join("x.mp4"))
        .unwrap_err();
    assert!(err.to_string().contains("file not found"), "{err}");
    assert!(!dir.path().join("x.mp4").exists());

    // Recorded answers have no token in them.
    let login = std::fs::read_to_string(dir.path().join("Login.json")).unwrap();
    assert!(
        login.contains("REDACTED") && !login.contains("tok"),
        "{login}"
    );
    assert!(dir.path().join("GetChannelstatus.json").exists());
}
