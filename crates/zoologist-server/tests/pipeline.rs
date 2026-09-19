//! End-to-end: a fox walks through a file-backed camera; the pipeline must store an animal event
//! with a snapshot, a playable clip and the species "red fox" (plan Step 7.1 acceptance).
//!
//! Needs the exported models in `models/` (see `docs/MODELS.md`); skipped when they are missing.

use std::path::{Path, PathBuf};

use zoologist_core::{Config, Label};
use zoologist_server::pipeline::{Pipeline, RunOptions};
use zoologist_store::{ClipState, EventQuery};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fox_walk_becomes_a_red_fox_event() {
    let models = repo().join("models");
    let needed = [
        "md_v1000_sorrel_320.onnx",
        "speciesnet.onnx",
        "speciesnet_labels.txt",
    ];
    if let Some(missing) = needed.iter().find(|f| !models.join(f).exists()) {
        eprintln!("skipping: models/{missing} not found (see docs/MODELS.md)");
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let video = repo().join("tools/fixtures/fox_walk_640x360_10fps.h264");
    let m = models.display();
    let config = Config::parse(&format!(
        r#"
[server]
data_dir = "{data}"

[station]
country = "USA"
admin1_region = "NY"

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

[[cameras]]
id = "yard"
name = "Yard"
transport = "rtsp"
detect_url = "file://{video}?fps=10"
record_url = "file://{video}?fps=10"
"#,
        data = data.path().display(),
        video = video.display(),
    ))
    .expect("config");

    let opts = RunOptions {
        fast_files: true,
        allow_slow_cpu: true,
    };
    let mut pipeline = Pipeline::start(config, &opts).await.expect("start");
    let store = pipeline.app.store.clone();
    pipeline.sources_ended().await;
    pipeline.shutdown().await;

    let page = store
        .call(|s| {
            s.list_events(&EventQuery {
                label: Some(Label::Animal),
                ..Default::default()
            })
        })
        .await
        .unwrap();
    let fox = page.items.first().expect("an animal event");
    assert!(fox.ended_at.is_some(), "event was not closed");
    let species = fox.species.as_ref().expect("species");
    assert_eq!(
        species.scientific_name.to_lowercase(),
        "vulpes vulpes",
        "{species:?}"
    );
    assert_eq!(fox.clip_state, ClipState::Ready, "{fox:?}");

    for rel in [&fox.snapshot_path, &fox.thumb_path, &fox.clip_path] {
        let rel = rel.as_deref().expect("path set");
        let meta = std::fs::metadata(data.path().join(rel)).expect(rel);
        assert!(meta.len() > 1000, "{rel} is too small");
    }
    let clip = data.path().join(fox.clip_path.as_deref().unwrap());
    let (_, samples) = zoologist_video::mp4r::read_mp4_index(&clip).expect("clip parses");
    assert!(
        samples.len() >= 20,
        "clip has only {} frames",
        samples.len()
    );
}
