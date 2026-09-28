//! Cameras, events and their media.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use zoologist_core::Label;
use zoologist_core::config::CameraKind;
use zoologist_core::yuv::i420_to_rgb_full;
use zoologist_store::{EventPatch, EventQuery, EventRecord, Feedback, MAX_PAGE, Order};
use zoologist_video::mp4w::codec_string;
use zoologist_video::snapshot::encode_jpeg;

use crate::live::viewer_stream;

use super::{ApiError, ApiResult, EventJson};
use crate::app::{ApiEvent, AppState};

/// Events per page when `limit` is not given.
const DEFAULT_LIMIT: usize = 50;

/// `GET /cameras`
pub async fn cameras(State(app): State<AppState>) -> Json<serde_json::Value> {
    let list: Vec<_> = app
        .config
        .cameras
        .iter()
        .filter(|c| c.enabled)
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "kind": match c.kind {
                    CameraKind::Stream => "stream",
                    CameraKind::HubClips => "hub_clips",
                },
                "labels": c.labels,
                "record": c.record && c.kind == CameraKind::Stream,
                "hub": c.hub,
            })
        })
        .collect();
    Json(serde_json::Value::Array(list))
}

/// `GET /cameras/{id}/latest.jpg`: the newest analysed frame.
pub async fn latest_jpg(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let camera = app
        .camera(&id)
        .ok_or_else(|| ApiError::not_found(format!("no live camera {id:?}")))?;
    let frame = camera
        .latest
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| ApiError::not_found("no frame yet"))?;
    let jpeg = tokio::task::spawn_blocking(move || {
        encode_jpeg(&i420_to_rgb_full(&frame), frame.width, frame.height, 80)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .ok_or_else(|| ApiError::not_found("cannot encode frame"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        jpeg,
    )
        .into_response())
}

/// `GET /cameras/{id}/live.mp4`: the camera's live video as fragmented MP4, for the browser's
/// MediaSource. The `X-Codec` header carries the codec string (e.g. `avc1.64001f`).
pub async fn live_mp4(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let camera = app
        .camera(&id)
        .ok_or_else(|| ApiError::not_found(format!("no live camera {id:?}")))?;
    let (info, gop, rx) = camera
        .live
        .subscribe()
        .ok_or_else(|| ApiError::not_found("no video from the camera yet"))?;
    let codec = codec_string(&info).ok_or_else(|| {
        ApiError::bad_request("this camera sends H.265, which browsers cannot play live")
    })?;
    let stream = viewer_stream(info, gop, rx)
        .map_err(|e| ApiError::bad_request(format!("cannot start live view: {e}")))?;
    let shutdown = app.shutdown.clone();
    let stream = stream.take_until(async move { shutdown.cancelled().await });
    Ok((
        [
            (header::CONTENT_TYPE, "video/mp4".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::HeaderName::from_static("x-codec"), codec),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    after_id: Option<String>,
    before_id: Option<String>,
    limit: Option<String>,
    order: Option<String>,
    camera: Option<String>,
    label: Option<String>,
    species: Option<String>,
    /// `1h`, `6h`, `24h`, `7d` or `30d`: only events that started within it (default: all).
    window: Option<String>,
    /// `true`: only events someone marked as wrong.
    wrong: Option<String>,
}

fn parse_id(name: &str, value: Option<&str>) -> ApiResult<Option<u64>> {
    value
        .map(|v| {
            v.parse()
                .map_err(|_| ApiError::bad_request(format!("{name} must be a number")))
        })
        .transpose()
}

fn empty_to_none(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

/// `GET /events`
pub async fn list(
    State(app): State<AppState>,
    Query(p): Query<ListParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = match p.limit.as_deref() {
        None => DEFAULT_LIMIT,
        Some(v) => match v.parse::<usize>() {
            Ok(n) if (1..=MAX_PAGE).contains(&n) => n,
            _ => {
                return Err(ApiError::bad_request(format!(
                    "limit must be 1..={MAX_PAGE}"
                )));
            }
        },
    };
    let order = match p.order.as_deref() {
        None | Some("desc") => Order::Desc,
        Some("asc") => Order::Asc,
        Some(_) => return Err(ApiError::bad_request("order must be asc or desc")),
    };
    let label = empty_to_none(p.label)
        .map(|l| l.parse::<Label>().map_err(ApiError::bad_request))
        .transpose()?;
    let query = EventQuery {
        after_id: parse_id("after_id", p.after_id.as_deref())?,
        before_id: parse_id("before_id", p.before_id.as_deref())?,
        limit,
        order,
        camera: empty_to_none(p.camera),
        label,
        species: empty_to_none(p.species),
        since: empty_to_none(p.window)
            .map(|w| super::parse_window(Some(&w)).map(|(_, since)| since))
            .transpose()?,
        marked_wrong: matches!(p.wrong.as_deref(), Some("true" | "1")),
    };
    let page = app.store.call(move |s| s.list_events(&query)).await?;
    let items: Vec<EventJson> = page.items.iter().map(EventJson::new).collect();
    Ok(Json(serde_json::json!({
        "items": items,
        "next_after_id": page.next_after_id,
        "next_before_id": page.next_before_id,
    })))
}

async fn load(app: &AppState, id: &str) -> ApiResult<EventRecord> {
    let id: u64 = id
        .parse()
        .map_err(|_| ApiError::bad_request("event id must be a number"))?;
    app.store
        .call(move |s| s.get_event(id))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("no event {id}")))
}

/// `GET /events/{id}`
pub async fn get_one(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = load(&app, &id).await?;
    Ok(Json(
        serde_json::to_value(EventJson::new(&record)).unwrap_or_default(),
    ))
}

/// Stores a changed event and tells the dashboards; returns it as JSON.
async fn save(app: &AppState, id: u64, patch: EventPatch) -> ApiResult<serde_json::Value> {
    let record = app
        .store
        .call(move |s| s.update_event(id, &patch))
        .await?
        .ok_or_else(|| ApiError::not_found("no such event"))?;
    let json = serde_json::to_value(EventJson::new(&record)).unwrap_or_default();
    app.publish(ApiEvent::Updated(record));
    Ok(json)
}

/// Body of `POST /events/{id}/feedback`.
#[derive(Debug, Deserialize)]
pub struct FeedbackBody {
    /// `person`, `vehicle`, `animal`, `motion`, or `nothing` for a false alarm.
    actual: String,
    species: Option<String>,
    note: Option<String>,
}

/// `POST /events/{id}/feedback`: marks an event as wrong and says what it really was.
pub async fn set_feedback(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<FeedbackBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = load(&app, &id).await?;
    let actual = match body.actual.as_str() {
        "nothing" => None,
        other => Some(other.parse::<Label>().map_err(|_| {
            ApiError::bad_request("actual must be person, vehicle, animal, motion or nothing")
        })?),
    };
    let text = |v: Option<String>| {
        v.map(|s| s.trim().chars().take(200).collect::<String>())
            .filter(|s| !s.is_empty())
    };
    let feedback = Feedback {
        actual,
        species: text(body.species),
        note: text(body.note),
        at: chrono::Utc::now(),
    };
    tracing::info!(event = record.id, actual = %body.actual, "marked wrong");
    let patch = EventPatch {
        feedback: Some(Some(feedback)),
        ..Default::default()
    };
    Ok(Json(save(&app, record.id, patch).await?))
}

/// `DELETE /events/{id}/feedback`: takes the "wrong" mark back.
pub async fn clear_feedback(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = load(&app, &id).await?;
    let patch = EventPatch {
        feedback: Some(None),
        ..Default::default()
    };
    Ok(Json(save(&app, record.id, patch).await?))
}

/// Body of `POST /events/{id}/reclassify`.
#[derive(Debug, Default, Deserialize)]
pub struct ReclassifyBody {
    /// Store the answer (otherwise only return it).
    #[serde(default)]
    store: bool,
}

/// One reclassification at a time, so live cameras keep most of the CPU.
static RECLASSIFY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// `POST /events/{id}/reclassify`: names an animal event's animal again from its clip.
pub async fn reclassify(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReclassifyBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = load(&app, &id).await?;
    if record.label != Label::Animal {
        return Err(ApiError::bad_request(
            "only animal events can be classified",
        ));
    }
    if record.clip_path.is_none() || record.ended_at.is_none() {
        return Err(ApiError::bad_request("the event has no clip yet"));
    }
    let unavailable = |why: String| ApiError(axum::http::StatusCode::SERVICE_UNAVAILABLE, why);
    let detector = app
        .detector
        .clone()
        .ok_or_else(|| unavailable("the detector is not loaded".into()))?;
    let species = app.species.clone().ok_or_else(|| {
        unavailable(
            app.species_problem
                .clone()
                .unwrap_or_else(|| "the species classifier is turned off".into()),
        )
    })?;
    let _slot = RECLASSIFY
        .acquire()
        .await
        .map_err(|_| unavailable("shutting down".into()))?;
    let config = app.config.clone();
    let event = record.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::reclassify::reclassify_one(&config, &detector, &species, &event, true)
    })
    .await
    .map_err(|e| ApiError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        ApiError(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e:#}"),
        )
    })?;
    tracing::info!(
        event = record.id,
        ?outcome,
        store = body.store,
        "reclassified"
    );
    if body.store
        && let Some(patch) = outcome.patch()
    {
        save(&app, record.id, patch).await?;
    }
    Ok(Json(serde_json::to_value(&outcome).unwrap_or_default()))
}

/// Serves a file of an event (with Range support), or 404 if it is missing.
async fn serve_file(
    app: &AppState,
    rel: Option<&str>,
    mime: &str,
    req: Request,
) -> ApiResult<Response> {
    let rel = rel.ok_or_else(|| ApiError::not_found("not available"))?;
    let path = app.data_dir.join(rel);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(ApiError::not_found("file is missing"));
    }
    let response = ServeFile::new_with_mime(path, &mime.parse().expect("valid mime"))
        .oneshot(req)
        .await
        .map_err(|_| ApiError::not_found("cannot read file"))?;
    Ok(response.map(Body::new))
}

/// `GET /events/{id}/clip.mp4`
pub async fn clip(
    State(app): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let record = load(&app, &id).await?;
    serve_file(&app, record.clip_path.as_deref(), "video/mp4", req).await
}

/// `GET /events/{id}/snapshot.jpg`
pub async fn snapshot(
    State(app): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let record = load(&app, &id).await?;
    serve_file(&app, record.snapshot_path.as_deref(), "image/jpeg", req).await
}

/// `GET /events/{id}/thumb.jpg` (falls back to the snapshot for events without a box).
pub async fn thumb(
    State(app): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let record = load(&app, &id).await?;
    let rel = record
        .thumb_path
        .as_deref()
        .or(record.snapshot_path.as_deref());
    serve_file(&app, rel, "image/jpeg", req).await
}
