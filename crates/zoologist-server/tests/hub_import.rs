//! End-to-end: a fake Reolink Hub has one recording (the fox fixture as fragmented MP4, the way
//! the Hub sends it). The importer must download it, analyse it and store a red fox event whose
//! clip is the downloaded file (plan Step 7.2).
//!
//! Needs ffmpeg (to make the fragmented MP4) and the models; skipped otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde_json::json;
use zoologist_core::{Config, Label};
use zoologist_server::pipeline::{Pipeline, RunOptions};
use zoologist_store::{ClipState, EventQuery};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[derive(Clone)]
struct FakeHub {
    clip: Arc<Vec<u8>>,
    start: DateTime<Utc>,
    downloads: Arc<AtomicUsize>,
}

fn hub_time(t: DateTime<Utc>) -> serde_json::Value {
    json!({"year": t.year(), "mon": t.month(), "day": t.day(),
           "hour": t.hour(), "min": t.minute(), "sec": t.second()})
}

async fn api(
    State(hub): State<FakeHub>,
    Query(q): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let cmd = q.get("cmd").cloned().unwrap_or_default();
    let ok = |value: serde_json::Value| {
        axum::Json(json!([{"cmd": cmd, "code": 0, "value": value}])).into_response()
    };
    match cmd.as_str() {
        // Classic login: no digest challenge is offered.
        "Login" => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            if req[0]["param"]["User"]["password"] == "pw" {
                ok(json!({"Token": {"name": "tok", "leaseTime": 3600}}))
            } else {
                axum::Json(json!([{"cmd": "Login", "code": 1, "error": {"rspCode": -7, "detail": "login failed"}}]))
                    .into_response()
            }
        }
        "Search" => {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let files = if req[0]["param"]["Search"]["channel"] == 3 {
                vec![json!({
                    "name": "1-0-fox", "type": "sub", "size": 1048576,
                    "StartTime": hub_time(hub.start),
                    "EndTime": hub_time(hub.start + chrono::Duration::seconds(10)),
                })]
            } else {
                Vec::new()
            };
            ok(json!({"SearchResult": {"channel": 3, "File": files}}))
        }
        "Download" | "download" => {
            hub.downloads.fetch_add(1, Ordering::SeqCst);
            (
                [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                hub.clip.as_ref().clone(),
            )
                .into_response()
        }
        _ => ok(json!({})),
    }
}

fn start_fake(hub: FakeHub) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let app = Router::new()
                .route("/cgi-bin/api.cgi", post(api).get(api))
                .with_state(hub);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    format!("http://{}", rx.recv().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn battery_camera_recording_becomes_a_red_fox_event() {
    let models = repo().join("models");
    let needed = [
        "md_v1000_sorrel_320.onnx",
        "speciesnet.onnx",
        "speciesnet_labels.txt",
    ];
    if let Some(missing) = needed.iter().find(|f| !models.join(f).exists()) {
        eprintln!("skipping: models/{missing} not found");
        return;
    }
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let clip = data.path().join("fox-frag.mp4");
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-r", "10", "-f", "h264", "-i"])
        .arg(repo().join("tools/fixtures/fox_walk_640x360_10fps.h264"))
        .args([
            "-c",
            "copy",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
        ])
        .arg(&clip)
        .status()
        .unwrap();
    assert!(status.success());

    let start = (Utc::now() - chrono::Duration::minutes(2))
        .with_nanosecond(0)
        .unwrap();
    let fake = FakeHub {
        clip: Arc::new(std::fs::read(&clip).unwrap()),
        start,
        downloads: Arc::default(),
    };
    let url = start_fake(fake.clone());
    let m = models.display();
    let config = Config::parse(&format!(
        r#"
[station]
timezone = "UTC"
country = "USA"
admin1_region = "NY"

[server]
data_dir = "{data}"

[inference]
detector = "md-sorrel"
workers = 2

[models.md-sorrel]
path = "{m}/md_v1000_sorrel_320.onnx"
kind = "yolov8"
classes = "megadetector"
input_size = 320
score_threshold = 0.35

[species]
path = "{m}/speciesnet.onnx"
labels = "{m}/speciesnet_labels.txt"

[[reolink_hubs]]
id = "hub"
url = "{url}"
user = "zoologist"
password = "pw"
poll_seconds = 5

[[cameras]]
id = "garden"
name = "Garden"
kind = "hub_clips"
hub = "hub"
channel = 3
"#,
        data = data.path().display(),
    ))
    .expect("config");
    config.validate().expect("valid config");

    let opts = RunOptions {
        fast_files: false,
        allow_slow_cpu: true,
    };
    let pipeline = Pipeline::start(config, &opts).await.expect("start");
    let store = pipeline.app.store.clone();
    // Wait for the fox, with its species, to be stored.
    let mut fox = None;
    for _ in 0..240 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let page = store
            .call(|s| {
                s.list_events(&EventQuery {
                    label: Some(Label::Animal),
                    ..Default::default()
                })
            })
            .await
            .unwrap();
        if let Some(e) = page.items.into_iter().find(|e| e.species.is_some()) {
            fox = Some(e);
            break;
        }
    }
    let health = pipeline.app.hubs[0].status.read().unwrap().clone();
    pipeline.shutdown().await;

    let fox =
        fox.unwrap_or_else(|| panic!("no animal event with a species; hub status {health:?}"));
    assert_eq!(fox.camera_id, "garden");
    let species = fox.species.as_ref().unwrap();
    assert_eq!(
        species.scientific_name.to_lowercase(),
        "vulpes vulpes",
        "{species:?}"
    );
    // Event times come from the recording, not from when it was imported.
    assert!(fox.started_at >= start && fox.started_at <= start + chrono::Duration::seconds(10));
    assert_eq!(fox.clip_state, ClipState::Ready);
    let clip_path = data.path().join(fox.clip_path.as_deref().unwrap());
    assert_eq!(std::fs::read(&clip_path).unwrap(), fake.clip.as_ref()[..]);
    assert!(fox.snapshot_path.is_some());
    assert_eq!(
        fake.downloads.load(Ordering::SeqCst),
        1,
        "imported exactly once"
    );
    assert_eq!(health.imported, 1, "{health:?}");
    assert!(
        store.hub_import_seen("hub", "1-0-fox").unwrap(),
        "the import is remembered"
    );
}
