//! Chart data: counts per label, per species and per hour, and all of them in one answer.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use zoologist_core::{Label, local_date_hour};
use zoologist_store::{HourCounts, SpeciesStat};

use super::{ApiError, ApiResult};
use crate::app::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct StatsParams {
    window: Option<String>,
    camera: Option<String>,
    date: Option<String>,
}

/// Parses `window` (default 24h) into its name and start time.
fn window(p: &StatsParams) -> ApiResult<(&'static str, DateTime<Utc>)> {
    super::parse_window(p.window.as_deref())
}

fn camera(p: &StatsParams) -> Option<String> {
    p.camera.clone().filter(|c| !c.is_empty())
}

fn date(app: &AppState, p: &StatsParams) -> ApiResult<NaiveDate> {
    match p.date.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|_| ApiError::bad_request("date must be YYYY-MM-DD")),
        None => Ok(today(app)),
    }
}

/// Today in the station time zone.
fn today(app: &AppState) -> NaiveDate {
    local_date_hour(Utc::now(), app.config.station.timezone).0
}

fn labels_json(window: &str, counts: &[(Label, u64)]) -> serde_json::Value {
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
    serde_json::json!({ "window": window, "items": items })
}

fn species_json(window: &str, stats: &[SpeciesStat]) -> serde_json::Value {
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
    serde_json::json!({ "window": window, "items": items })
}

fn hourly_json(date: NaiveDate, hours: &[HourCounts; 24]) -> serde_json::Value {
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
    serde_json::json!({ "date": date, "hours": hours })
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
    Ok(Json(labels_json(name, &counts)))
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
    Ok(Json(species_json(name, &stats)))
}

/// `GET /stats/hourly?date=YYYY-MM-DD` (default: today in the station time zone).
pub async fn hourly(
    State(app): State<AppState>,
    Query(p): Query<StatsParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let date = date(&app, &p)?;
    let cam = camera(&p);
    let hours = app
        .store
        .call(move |s| s.stats_hourly(date, cam.as_deref()))
        .await?;
    Ok(Json(hourly_json(date, &hours)))
}

/// `GET /stats/overview`: everything the dashboard's charts show, in one answer: `labels`,
/// `species` and `hourly` as their own routes give them, and `today`, each camera's events of
/// today per label (never limited by `camera`).
pub async fn overview(
    State(app): State<AppState>,
    Query(p): Query<StatsParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let (name, since) = window(&p)?;
    let date = date(&app, &p)?;
    let today = today(&app);
    let cam = camera(&p);
    let ids: Vec<String> = app.config.cameras.iter().map(|c| c.id.clone()).collect();
    let (labels, species, hours, cameras) = app
        .store
        .call(move |s| {
            let mut cameras = serde_json::Map::new();
            for id in ids {
                let mut total = HourCounts::default();
                for h in s.stats_hourly(today, Some(&id))? {
                    total.person += h.person;
                    total.vehicle += h.vehicle;
                    total.animal += h.animal;
                    total.motion += h.motion;
                }
                let counts = serde_json::json!({
                    "person": total.person,
                    "vehicle": total.vehicle,
                    "animal": total.animal,
                    "motion": total.motion,
                });
                cameras.insert(id, counts);
            }
            Ok((
                s.stats_by_label(since, cam.as_deref())?,
                s.stats_by_species(since, cam.as_deref())?,
                s.stats_hourly(date, cam.as_deref())?,
                cameras,
            ))
        })
        .await?;
    Ok(Json(serde_json::json!({
        "labels": labels_json(name, &labels),
        "species": species_json(name, &species),
        "hourly": hourly_json(date, &hours),
        "today": { "date": today, "cameras": cameras },
    })))
}
