//! Animal sightings for other programs (a home dashboard): which animals were seen and how often.
//! Names, counts and times only: no people, vehicles or motion, and no cameras, event ids,
//! pictures or clips.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use zoologist_core::{Label, local_date_hour};
use zoologist_store::EventRecord;

use super::{ApiError, ApiResult};
use crate::app::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct AnimalParams {
    window: Option<String>,
    date: Option<String>,
}

/// The animal an event counts as: `None` if it is not an animal, `Some(None)` for an animal
/// without a name, `Some(Some((common, scientific)))` otherwise.
///
/// A "Wrong" mark goes before what Zoologist stored: an event marked as nothing, or as a person,
/// is left out; one marked as an animal counts under the species typed in with the mark (which
/// has no scientific name), or without a name.
fn animal(e: &EventRecord) -> Option<Option<(String, String)>> {
    match &e.feedback {
        Some(f) if f.actual != Some(Label::Animal) => None,
        Some(f) => Some(f.species.clone().map(|name| (name, String::new()))),
        None if e.label != Label::Animal => None,
        None => Some(
            e.species
                .as_ref()
                .map(|s| (s.common_name.clone(), s.scientific_name.clone())),
        ),
    }
}

/// Sightings of one species.
struct Sightings {
    common_name: String,
    scientific_name: String,
    last_seen: DateTime<Utc>,
    by_hour: [u64; 24],
}

/// Counts per species (names grouped ignoring case, most seen first) and the number of animals
/// without a name.
fn tally(events: &[EventRecord]) -> (Vec<Sightings>, u64) {
    let mut by_name = HashMap::<String, Sightings>::new();
    let mut unidentified = 0;
    for e in events {
        let Some(name) = animal(e) else { continue };
        let Some((common, scientific)) = name else {
            unidentified += 1;
            continue;
        };
        let s = by_name
            .entry(common.to_lowercase())
            .or_insert_with(|| Sightings {
                common_name: common,
                scientific_name: String::new(),
                last_seen: e.started_at,
                by_hour: [0; 24],
            });
        if s.scientific_name.is_empty() {
            s.scientific_name = scientific;
        }
        s.last_seen = s.last_seen.max(e.started_at);
        s.by_hour[e.local_hour as usize % 24] += 1;
    }
    let mut out: Vec<Sightings> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        let count = |s: &Sightings| s.by_hour.iter().sum::<u64>();
        count(b)
            .cmp(&count(a))
            .then_with(|| a.common_name.cmp(&b.common_name))
    });
    (out, unidentified)
}

#[derive(Serialize)]
struct RecentSpecies {
    common_name: String,
    scientific_name: String,
    count: u64,
    last_seen: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct Recent {
    window: &'static str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    total: u64,
    unidentified: u64,
    species: Vec<RecentSpecies>,
}

/// `GET /animals/recent`: the animals seen within `window` (default 24h), most seen first.
pub async fn recent(
    State(app): State<AppState>,
    Query(p): Query<AnimalParams>,
) -> ApiResult<Json<Recent>> {
    let (window, since) = super::parse_window(p.window.as_deref())?;
    let until = Utc::now();
    let events = app.store.call(move |s| s.events_since(since)).await?;
    let (species, unidentified) = tally(&events);
    let species: Vec<RecentSpecies> = species
        .into_iter()
        .map(|s| RecentSpecies {
            count: s.by_hour.iter().sum(),
            common_name: s.common_name,
            scientific_name: s.scientific_name,
            last_seen: s.last_seen,
        })
        .collect();
    Ok(Json(Recent {
        window,
        since,
        until,
        total: unidentified + species.iter().map(|s| s.count).sum::<u64>(),
        unidentified,
        species,
    }))
}

#[derive(Serialize)]
struct DailySpecies {
    common_name: String,
    scientific_name: String,
    total: u64,
    by_hour: [u64; 24],
}

#[derive(Serialize)]
pub struct Daily {
    date: NaiveDate,
    total: u64,
    unidentified: u64,
    species: Vec<DailySpecies>,
}

/// `GET /animals/daily?date=YYYY-MM-DD` (default: today in the station time zone): the animals
/// seen that day, with a count per local hour.
pub async fn daily(
    State(app): State<AppState>,
    Query(p): Query<AnimalParams>,
) -> ApiResult<Json<Daily>> {
    let date = match p.date.as_deref().filter(|d| !d.is_empty()) {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
            .map_err(|_| ApiError::bad_request("date must be YYYY-MM-DD"))?,
        None => local_date_hour(Utc::now(), app.config.station.timezone).0,
    };
    let events = app.store.call(move |s| s.events_on(date)).await?;
    let (species, unidentified) = tally(&events);
    let species: Vec<DailySpecies> = species
        .into_iter()
        .map(|s| DailySpecies {
            total: s.by_hour.iter().sum(),
            common_name: s.common_name,
            scientific_name: s.scientific_name,
            by_hour: s.by_hour,
        })
        .collect();
    Ok(Json(Daily {
        date,
        total: unidentified + species.iter().map(|s| s.total).sum::<u64>(),
        unidentified,
        species,
    }))
}
