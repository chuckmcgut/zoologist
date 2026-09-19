//! The HTTP API under `/api/v1` and the web UI's static files (plan Phase 9).

mod events;
mod health;
mod stats;
mod stream;
#[cfg(test)]
mod tests;

use axum::Json;
use axum::Router;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;
use zoologist_store::{ClipState, EventRecord, StoreError};

use crate::app::AppState;

/// Builds the whole HTTP service: the API plus the UI from `server.static_dir`.
pub fn router(app: AppState) -> Router {
    let api = Router::new()
        .route("/health", get(health::health))
        .route("/config", get(health::config))
        .route("/cameras", get(events::cameras))
        .route("/cameras/{id}/latest.jpg", get(events::latest_jpg))
        .route("/cameras/{id}/live.mp4", get(events::live_mp4))
        .route("/events", get(events::list))
        .route("/events/{id}", get(events::get_one))
        .route("/events/{id}/clip.mp4", get(events::clip))
        .route("/events/{id}/snapshot.jpg", get(events::snapshot))
        .route("/events/{id}/thumb.jpg", get(events::thumb))
        .route("/stats/labels", get(stats::labels))
        .route("/stats/species", get(stats::species))
        .route("/stats/hourly", get(stats::hourly))
        .route("/stream", get(stream::stream))
        .fallback(|| async { ApiError::not_found("no such API route") });

    let cors = cors_layer(&app.config.server.cors_allow_origins);
    // Clips are already compressed and must keep byte ranges intact; images and SSE are
    // excluded by the default predicate.
    let compress = CompressionLayer::new()
        .compress_when(DefaultPredicate::new().and(NotForContentType::const_new("video/")));
    let ui = ServeDir::new(&app.config.server.static_dir).append_index_html_on_directories(true);
    Router::new()
        .nest("/api/v1", api)
        .fallback_service(ui)
        .layer(compress)
        .layer(cors)
        .with_state(app)
}

fn cors_layer(origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods([axum::http::Method::GET]);
    if origins.iter().any(|o| o == "*") {
        return layer.allow_origin(AllowOrigin::any());
    }
    let list: Vec<HeaderValue> = origins.iter().filter_map(|o| o.parse().ok()).collect();
    layer.allow_origin(AllowOrigin::list(list))
}

/// An API error: `{"error": "..."}` with a status code.
#[derive(Debug)]
pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        tracing::warn!("store error: {e}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "database error".into())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// An event as the API returns it: the stored record plus links to its media.
#[derive(Serialize)]
pub struct EventJson<'a> {
    #[serde(flatten)]
    pub record: &'a EventRecord,
    pub clip_url: Option<String>,
    pub snapshot_url: Option<String>,
    pub thumb_url: Option<String>,
    /// True while the event has not ended.
    pub active: bool,
}

impl<'a> EventJson<'a> {
    pub fn new(record: &'a EventRecord) -> Self {
        let url = |what: &str| format!("/api/v1/events/{}/{what}", record.id);
        let clip_ready = record.clip_state == ClipState::Ready && record.clip_path.is_some();
        EventJson {
            record,
            clip_url: clip_ready.then(|| url("clip.mp4")),
            snapshot_url: record.snapshot_path.as_ref().map(|_| url("snapshot.jpg")),
            thumb_url: record.thumb_path.as_ref().map(|_| url("thumb.jpg")),
            active: record.ended_at.is_none(),
        }
    }
}

/// Serves the HTTP API on `server.bind` until `app.shutdown` is cancelled.
pub async fn serve(app: AppState) -> anyhow::Result<()> {
    let bind = app.config.server.bind;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("cannot listen on {bind}: {e}"))?;
    tracing::info!("web UI and API on http://{bind}");
    let shutdown = app.shutdown.clone();
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}
