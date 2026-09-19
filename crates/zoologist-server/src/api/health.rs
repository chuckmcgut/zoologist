//! `GET /health`: is everything running, and how fast.

use axum::Json;
use axum::extract::State;
use chrono::Utc;
use zoologist_core::config::DecoderKind;
use zoologist_video::stream::StreamState;

use crate::app::AppState;

const MB: u64 = 1024 * 1024;

fn state_name(state: &StreamState) -> String {
    format!("{state:?}")
        .split('(')
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

/// CPU features the detector relies on (always true off x86).
fn cpu() -> serde_json::Value {
    #[cfg(target_arch = "x86_64")]
    let (avx2, fma) = (
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("fma"),
    );
    #[cfg(not(target_arch = "x86_64"))]
    let (avx2, fma) = (true, true);
    serde_json::json!({ "avx2": avx2, "fma": fma })
}

/// `GET /config`: what the UI needs to know about the station.
pub async fn config(State(app): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "station": {
            "name": app.config.station.name,
            "timezone": app.config.station.timezone.name(),
        },
        "clips_days": app.config.retention.clips_days,
    }))
}

pub async fn health(State(app): State<AppState>) -> Json<serde_json::Value> {
    let pool = app.detector.as_ref().map(|d| d.stats());
    let cameras: Vec<_> = app
        .cameras
        .iter()
        .map(|c| {
            let detect = c.detect.read().unwrap_or_else(|e| e.into_inner()).clone();
            let record = c.record.read().unwrap_or_else(|e| e.into_inner()).clone();
            let decode = c.decode.read().unwrap_or_else(|e| e.into_inner()).clone();
            let analysis = c.analysis.read().unwrap_or_else(|e| e.into_inner()).clone();
            let last_segment_at = *c.last_segment_at.read().unwrap_or_else(|e| e.into_inner());
            let detector_drops = pool
                .as_ref()
                .and_then(|p| p.drops.get(&c.id).copied())
                .unwrap_or(0);
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "detect": {
                    "state": state_name(&detect.state),
                    "error": detect.last_error,
                    "fps": detect.fps_measured,
                    "analysed_fps": analysis.analysed_fps,
                    "decode_ms": decode.mean_decode_ms,
                    "reconnects": detect.reconnects,
                    "last_frame_at": analysis.last_frame_at,
                },
                "record": {
                    "state": state_name(&record.state),
                    "error": record.last_error,
                    "last_segment_at": last_segment_at,
                },
                "drops": {
                    "decoded_frames": decode.dropped,
                    "detector_jobs": detector_drops,
                },
            })
        })
        .collect();
    let hubs: Vec<_> = app
        .hubs
        .iter()
        .map(|h| {
            let status = h.status.read().unwrap_or_else(|e| e.into_inner()).clone();
            let mut v = serde_json::to_value(status).unwrap_or_default();
            v["id"] = serde_json::json!(h.id);
            v
        })
        .collect();
    let disk = app.disk.read().unwrap_or_else(|e| e.into_inner()).clone();
    Json(serde_json::json!({
        "station": app.config.station.name,
        "uptime_s": (Utc::now() - app.started_at).num_seconds(),
        "cpu": cpu(),
        "decoder": match app.config.video.decoder {
            DecoderKind::Rust => "rust",
            DecoderKind::Ffmpeg => "ffmpeg",
        },
        "cameras": cameras,
        "hubs": hubs,
        "detector": pool.map(|p| serde_json::json!({
            "id": app.detector_id,
            "workers": p.workers,
            "busy_workers": p.busy_workers,
            "mean_ms": p.mean_infer_ms,
            "p95_ms": p.p95_infer_ms,
            "queue_depth": p.queue_depth,
        })),
        "species": app.species.as_ref().map(|s| serde_json::json!({
            "id": zoologist_vision::species::MODEL_ID,
            "queue_depth": s.queue_depth(),
            "dropped": s.dropped(),
        })),
        "disk": {
            "recordings_mb": disk.recordings_bytes / MB,
            "clips_mb": disk.clips_bytes / MB,
            "free_mb": disk.free_bytes / MB,
            "measured_at": disk.measured_at,
        },
    }))
}
