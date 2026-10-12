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
                // The live view can switch to a sharper stream than the one it shows.
                "full": c.full_stream_url().is_some() || battery_live(&app, c),
                // Whether the live view works, and after how many seconds it stops by itself.
                "live": c.kind == CameraKind::Stream || battery_live(&app, c),
                "live_seconds": battery_live(&app, c).then_some(BATTERY_LIVE.as_secs()),
                "full_rotate": c.full_rotate,
                // Areas where motion is ignored: polygons as flat [x1, y1, x2, y2, …] lists, 0..1.
                "motion_mask": c.motion_mask,
                "hub": c.hub,
            })
        })
        .collect();
    Json(serde_json::Value::Array(list))
}

/// Whether this is a battery camera whose Hub can show it live.
fn battery_live(app: &AppState, camera: &zoologist_core::config::CameraConfig) -> bool {
    app.config.hub_live_url(camera, false).is_some()
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
///
/// `?full=1` asks for the camera's full-resolution stream instead (see [`direct_live`]). A
/// battery camera behind a Reolink Hub is woken for the viewer, and its stream ends by itself
/// after [`BATTERY_LIVE`].
pub async fn live_mp4(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LiveParams>,
) -> ApiResult<Response> {
    let full = params.full.is_some_and(|v| v != "0" && v != "false");
    let config = app.config.cameras.iter().find(|c| c.id == id && c.enabled);
    if let Some(url) = config.and_then(|c| app.config.hub_live_url(c, full)) {
        let name = if full { "live-full" } else { "live" };
        return direct_live(&app, &id, name, url, BATTERY_START, Some(BATTERY_LIVE)).await;
    }
    if full {
        let url = config.and_then(|c| c.full_stream_url()).ok_or_else(|| {
            ApiError::not_found("this camera has no separate full-resolution stream")
        })?;
        return direct_live(&app, &id, "full", url, FULL_START, None).await;
    }
    let camera = app
        .camera(&id)
        .ok_or_else(|| ApiError::not_found(format!("no live camera {id:?}")))?;
    let (info, gop, rx) = camera
        .live
        .subscribe()
        .ok_or_else(|| ApiError::not_found("no video from the camera yet"))?;
    let codec = codec_string(&info).ok_or_else(|| {
        ApiError::bad_request("the camera's stream cannot be described to a browser")
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
pub struct LiveParams {
    full: Option<String>,
}

/// How long a battery camera may take to wake up and send video.
const BATTERY_START: std::time::Duration = std::time::Duration::from_secs(30);
/// A battery camera's live view ends after this long: it is awake, and draining, all the while.
const BATTERY_LIVE: std::time::Duration = std::time::Duration::from_secs(120);

/// Streams opened for one viewer at once: each is its own connection to the camera (or its Hub).
static FULL_VIEWERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// How long the full-resolution stream may take to start.
const FULL_START: std::time::Duration = std::time::Duration::from_secs(15);

/// A stream that Zoologist neither records nor analyses (a camera's full-resolution stream,
/// often H.265, or a battery camera's live picture): it is opened at `url` for this viewer and
/// closed when they leave, or after `limit`.
async fn direct_live(
    app: &AppState,
    id: &str,
    name: &str,
    url: String,
    start: std::time::Duration,
    limit: Option<std::time::Duration>,
) -> ApiResult<Response> {
    use axum::http::StatusCode;
    use zoologist_video::stream::StreamItem;
    let permit = FULL_VIEWERS.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "two of these live streams are open already".into(),
        )
    })?;
    let cancel = app.shutdown.child_token();
    let stop = cancel.clone().drop_guard();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let status = zoologist_video::source::SharedStatus::default();
    zoologist_video::source::spawn_rtsp_stream(
        format!("{id}/{name}"),
        url,
        tx,
        status.clone(),
        cancel,
    );
    let first = tokio::time::timeout(start, async {
        loop {
            match rx.recv().await {
                Some(StreamItem::Info(info)) => return Some(info),
                Some(StreamItem::Unit(_)) => {}
                None => return None,
            }
        }
    })
    .await;
    let Ok(Some(info)) = first else {
        let why = status
            .read()
            .ok()
            .and_then(|s| s.last_error.clone())
            .unwrap_or_else(|| format!("no video within {} s", start.as_secs()));
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!("the stream did not start: {why}"),
        ));
    };
    let codec = codec_string(&info).ok_or_else(|| {
        ApiError::bad_request("the camera's stream cannot be described to a browser")
    })?;
    let stream = crate::live::direct_stream(info, rx, (stop, permit))
        .map_err(|e| ApiError::bad_request(format!("cannot start live view: {e}")))?;
    let stream = stream.take_until(async move {
        match limit {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    });
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

/// `DELETE /events/{id}`: removes the event with its pictures and its clip.
pub async fn delete(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let record = load(&app, &id).await?;
    if record.ended_at.is_none() {
        return Err(ApiError::bad_request(
            "the event is still going on: delete it when it has ended",
        ));
    }
    let id = record.id;
    // A Hub recording is the clip of every event found in it: it stays while another event
    // still shows it.
    let (clip, at) = (record.clip_path.clone(), record.started_at);
    let hour = chrono::Duration::hours(1);
    let clip_shared = app
        .store
        .call(move |s| {
            let near = s.events_between(at - hour, Some(at + hour))?;
            Ok(clip.is_some() && near.iter().any(|e| e.id != id && e.clip_path == clip))
        })
        .await?;
    if !app.store.call(move |s| s.delete_event(id)).await? {
        return Err(ApiError::not_found(format!("no event {id}")));
    }
    let clip = record.clip_path.as_ref().filter(|_| !clip_shared);
    for rel in [
        clip,
        record.snapshot_path.as_ref(),
        record.thumb_path.as_ref(),
        record.sharp_path.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        let _ = tokio::fs::remove_file(app.data_dir.join(rel)).await;
    }
    let _ = tokio::fs::remove_file(crate::hub_main::cached(&app.data_dir, id)).await;
    tracing::info!(event = id, camera = %record.camera_id, label = %record.label, "event deleted by the user");
    app.publish(ApiEvent::Removed(record));
    Ok(Json(serde_json::json!({ "deleted": id })))
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

/// `GET /events/{id}/main.mp4`: the full-resolution recording of a battery camera's event,
/// fetched from the Hub when first asked for (see [`crate::hub_main`]).
pub async fn main_recording(
    State(app): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    use crate::hub_main::MainError;
    let record = load(&app, &id).await?;
    let path = crate::hub_main::fetch(&app, &record).await.map_err(|e| {
        let status = match e {
            MainError::NotOnHub(_) | MainError::Gone => axum::http::StatusCode::NOT_FOUND,
            MainError::Hub(_) => axum::http::StatusCode::BAD_GATEWAY,
        };
        ApiError(status, e.to_string())
    })?;
    let response = ServeFile::new_with_mime(path, &"video/mp4".parse().expect("valid mime"))
        .oneshot(req)
        .await
        .map_err(|_| ApiError::not_found("cannot read file"))?;
    Ok(response.map(Body::new))
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

/// `GET /events/{id}/sharp.jpg`: the animal's full-resolution close-up, when there is one.
pub async fn sharp(
    State(app): State<AppState>,
    Path(id): Path<String>,
    req: Request,
) -> ApiResult<Response> {
    let record = load(&app, &id).await?;
    serve_file(&app, record.sharp_path.as_deref(), "image/jpeg", req).await
}
