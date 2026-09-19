//! Chart data: counts per label, per species and per hour.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use zoologist_core::{Label, local_date_hour};

use super::{ApiError, ApiResult};
use crate::app::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct StatsParams {
    window: Option<String>,
    camera: Option<String>,
    date: Option<String>,
}

/// The windows the UI offers, with their length.
const WINDOWS: [(&str, i64); 5] = [
    ("1h", 1),
    ("6h", 6),
    ("24h", 24),
    ("7d", 7 * 24),
    ("30d", 30 * 24),
];

/// Parses `window` (default 24h) into its name and start time.
fn window(p: &StatsParams) -> ApiResult<(&'static str, DateTime<Utc>)> {
    let name = p.window.as_deref().unwrap_or("24h");
    let (name, hours) = WINDOWS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| ApiError::bad_request("window must be one of 1h, 6h, 24h, 7d, 30d"))?;
    Ok((name, Utc::now() - chrono::Duration::hours(*hours)))
}

fn camera(p: &StatsParams) -> Option<String> {
    p.camera.clone().filter(|c| !c.is_empty())
}

/// `GET /stats/labels`: every label (in display order, zeros included).
pub async fn labels(
    State(app): State<AppState>,
    Query(p): Query<StatsParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let (name, since) = window(&p)?;
    let cam = camera(&p);
    let counts = app
        .store
        .call(move |s| s.stats_by_label(since, cam.as_deref()))
        .await?;
    let items: Vec<_> = Label::ALL
        .iter()
        .map(|label| {
            let count = counts
                .iter()
                .find(|(l, _)| l == label)
                .map_or(0, |(_, c)| *c);
            serde_json::json!({ "label": label, "count": count })
        })
        .collect();
    Ok(Json(serde_json::json!({ "window": name, "items": items })))
}

/// `GET /stats/species`: animal species, most seen first. `common_name: null` groups animals
/// without a species.
pub async fn species(
    State(app): State<AppState>,
    Query(p): Query<StatsParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let (name, since) = window(&p)?;
    let cam = camera(&p);
    let stats = app
        .store
        .call(move |s| s.stats_by_species(since, cam.as_deref()))
        .await?;
    let items: Vec<_> = stats
        .iter()
        .map(|s| {
            serde_json::json!({
                "common_name": s.common_name,
                "scientific_name": s.scientific_name,
                "count": s.count,
                "last_seen": s.last_seen,
                "best_event_id": s.best_event_id,
                "best_score": s.best_score,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "window": name, "items": items })))
}

/// `GET /stats/hourly?date=YYYY-MM-DD` (default: today in the station time zone).
pub async fn hourly(
    State(app): State<AppState>,
    Query(p): Query<StatsParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let date = match p.date.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|_| ApiError::bad_request("date must be YYYY-MM-DD"))?,
        None => local_date_hour(Utc::now(), app.config.station.timezone).0,
    };
    let cam = camera(&p);
    let hours = app
        .store
        .call(move |s| s.stats_hourly(date, cam.as_deref()))
        .await?;
    let hours: Vec<_> = hours
        .iter()
        .enumerate()
        .map(|(hour, h)| {
            serde_json::json!({
                "hour": hour,
                "person": h.person,
                "vehicle": h.vehicle,
                "animal": h.animal,
                "motion": h.motion,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "date": date, "hours": hours })))
}
