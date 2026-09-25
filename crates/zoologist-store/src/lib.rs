#![forbid(unsafe_code)]
//! Persistent storage for events and recording segments (plan Step 6.1), in `redb`.
//!
//! Tables:
//! - `meta`: schema version and the next event id;
//! - `events`: event id → JSON [`EventRecord`];
//! - `events_by_time`: (started_at µs, id) → (), for time-window queries and charts;
//! - `segments`: (camera, start µs) → JSON [`SegmentRecord`];
//! - `hub_imports`: (hub id, recording file name) → event id (−1 = nothing kept).
//!
//! Every method is synchronous and fast; call them from `spawn_blocking` in async code, or use
//! [`Store::call`].

mod records;

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use zoologist_core::{Label, local_date_hour};

pub use records::{
    ClipState, EventPatch, EventQuery, EventRecord, HourCounts, NewEvent, Order, Page,
    SegmentRecord, SpeciesStat,
};

const SCHEMA_VERSION: u64 = 1;

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("events");
const EVENTS_BY_TIME: TableDefinition<(i64, u64), ()> = TableDefinition::new("events_by_time");
const SEGMENTS: TableDefinition<(&str, i64), &[u8]> = TableDefinition::new("segments");
const HUB_IMPORTS: TableDefinition<(&str, &str), i64> = TableDefinition::new("hub_imports");

/// Segments longer than this are not expected; used to bound range scans.
const MAX_SEGMENT_US: i64 = 10 * 60 * 1_000_000;
/// Largest page of events returned at once.
pub const MAX_PAGE: usize = 500;

/// Storage errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] redb::Error),
    #[error("corrupt record: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database was written by a newer version (schema {0})")]
    NewerSchema(u64),
    #[error("background task failed: {0}")]
    Task(String),
}

macro_rules! impl_from_redb {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::Db(e.into())
            }
        }
    )*};
}
impl_from_redb!(
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError
);

pub type Result<T> = std::result::Result<T, StoreError>;

/// The event and segment store. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
    tz: Tz,
}

fn micros(ts: DateTime<Utc>) -> i64 {
    ts.timestamp_micros()
}

impl Store {
    /// Opens (or creates) the database at `path`. `tz` is the station time zone, used for the
    /// local date and hour stored with each event.
    pub fn open(path: &Path, tz: Tz) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Db(redb::Error::Io(e)))?;
        }
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            let version = meta.get("schema_version")?.map(|v| v.value());
            match version {
                None => {
                    meta.insert("schema_version", SCHEMA_VERSION)?;
                    meta.insert("next_event_id", 1)?;
                }
                Some(v) if v > SCHEMA_VERSION => return Err(StoreError::NewerSchema(v)),
                Some(_) => {}
            }
            txn.open_table(EVENTS)?;
            txn.open_table(EVENTS_BY_TIME)?;
            txn.open_table(SEGMENTS)?;
            txn.open_table(HUB_IMPORTS)?;
        }
        txn.commit()?;
        Ok(Store {
            db: Arc::new(db),
            tz,
        })
    }

    /// Runs `f` on a blocking thread. Use from async code.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Store) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| StoreError::Task(e.to_string()))?
    }

    // -----------------------------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------------------------

    /// Stores a new event and returns its id. Ids increase strictly, in insertion order.
    pub fn insert_event(&self, new: &NewEvent) -> Result<EventRecord> {
        let txn = self.db.begin_write()?;
        let record = {
            let mut meta = txn.open_table(META)?;
            let id = meta.get("next_event_id")?.map_or(1, |v| v.value());
            meta.insert("next_event_id", id + 1)?;
            let (local_date, local_hour) = local_date_hour(new.started_at, self.tz);
            let record = EventRecord {
                id,
                camera_id: new.camera_id.clone(),
                label: new.label,
                raw_class: new.raw_class.clone(),
                started_at: new.started_at,
                ended_at: None,
                local_date,
                local_hour: local_hour as u8,
                top_score: new.top_score,
                median_score: new.median_score,
                best_bbox: new.best_bbox,
                species: None,
                snapshot_path: new.snapshot_path.clone(),
                thumb_path: new.thumb_path.clone(),
                clip_path: None,
                clip_bytes: None,
                clip_state: ClipState::Pending,
            };
            txn.open_table(EVENTS)?
                .insert(id, serde_json::to_vec(&record)?.as_slice())?;
            txn.open_table(EVENTS_BY_TIME)?
                .insert((micros(new.started_at), id), ())?;
            record
        };
        txn.commit()?;
        Ok(record)
    }

    /// Applies `patch` to an event. Returns the updated record, or `None` for an unknown id.
    pub fn update_event(&self, id: u64, patch: &EventPatch) -> Result<Option<EventRecord>> {
        let txn = self.db.begin_write()?;
        let updated = {
            let mut events = txn.open_table(EVENTS)?;
            let current = events.get(id)?.map(|v| v.value().to_vec());
            match current {
                None => None,
                Some(bytes) => {
                    let mut record: EventRecord = serde_json::from_slice(&bytes)?;
                    patch.apply(&mut record);
                    events.insert(id, serde_json::to_vec(&record)?.as_slice())?;
                    Some(record)
                }
            }
        };
        txn.commit()?;
        Ok(updated)
    }

    /// Deletes an event record (its files are the caller's). Returns `false` for an unknown id.
    pub fn delete_event(&self, id: u64) -> Result<bool> {
        let txn = self.db.begin_write()?;
        let found = {
            let mut events = txn.open_table(EVENTS)?;
            let removed = events.remove(id)?.map(|v| v.value().to_vec());
            match removed {
                None => false,
                Some(bytes) => {
                    let record: EventRecord = serde_json::from_slice(&bytes)?;
                    txn.open_table(EVENTS_BY_TIME)?
                        .remove((micros(record.started_at), id))?;
                    true
                }
            }
        };
        txn.commit()?;
        Ok(found)
    }

    /// One event by id.
    pub fn get_event(&self, id: u64) -> Result<Option<EventRecord>> {
        let txn = self.db.begin_read()?;
        let events = txn.open_table(EVENTS)?;
        match events.get(id)? {
            Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
            None => Ok(None),
        }
    }

    /// A page of events in id order (ascending or descending), filtered.
    pub fn list_events(&self, q: &EventQuery) -> Result<Page> {
        let limit = q.limit.clamp(1, MAX_PAGE);
        let txn = self.db.begin_read()?;
        let events = txn.open_table(EVENTS)?;
        let lower = q.after_id.map_or(0, |a| a + 1);
        let upper = q.before_id.unwrap_or(u64::MAX);
        let mut items = Vec::new();
        if lower < upper {
            let range = events.range(lower..upper)?;
            let mut push = |entry: std::result::Result<
                (redb::AccessGuard<u64>, redb::AccessGuard<&[u8]>),
                redb::StorageError,
            >|
             -> Result<bool> {
                let (_, value) = entry?;
                let record: EventRecord = serde_json::from_slice(value.value())?;
                if q.matches(&record) {
                    items.push(record);
                }
                Ok(items.len() >= limit)
            };
            match q.order {
                Order::Asc => {
                    for entry in range {
                        if push(entry)? {
                            break;
                        }
                    }
                }
                Order::Desc => {
                    for entry in range.rev() {
                        if push(entry)? {
                            break;
                        }
                    }
                }
            }
        }
        Ok(Page {
            next_after_id: items.iter().map(|e| e.id).max(),
            next_before_id: items.iter().map(|e| e.id).min(),
            items,
        })
    }

    /// Events that started at or after `since`, oldest first.
    pub fn events_since(&self, since: DateTime<Utc>) -> Result<Vec<EventRecord>> {
        self.events_between(since, None)
    }

    /// Events that started in `[from, to)` (to the end when `to` is `None`), oldest first.
    pub fn events_between(
        &self,
        from: DateTime<Utc>,
        to: Option<DateTime<Utc>>,
    ) -> Result<Vec<EventRecord>> {
        let txn = self.db.begin_read()?;
        let index = txn.open_table(EVENTS_BY_TIME)?;
        let events = txn.open_table(EVENTS)?;
        let end = to.map_or((i64::MAX, u64::MAX), |t| (micros(t), 0));
        let mut out = Vec::new();
        for entry in index.range((micros(from), 0)..end)? {
            let (key, _) = entry?;
            let (_, id) = key.value();
            if let Some(v) = events.get(id)? {
                out.push(serde_json::from_slice(v.value())?);
            }
        }
        Ok(out)
    }

    /// Event counts per label since `since`, largest first. Labels with no events are omitted.
    pub fn stats_by_label(
        &self,
        since: DateTime<Utc>,
        camera: Option<&str>,
    ) -> Result<Vec<(Label, u64)>> {
        let mut counts = std::collections::BTreeMap::<Label, u64>::new();
        for e in self.events_since(since)? {
            if camera.is_none_or(|c| e.camera_id == c) {
                *counts.entry(e.label).or_default() += 1;
            }
        }
        let mut out: Vec<(Label, u64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Animal event counts per species since `since`, largest first. Animals without a species
    /// are grouped under `common_name: None`.
    pub fn stats_by_species(
        &self,
        since: DateTime<Utc>,
        camera: Option<&str>,
    ) -> Result<Vec<SpeciesStat>> {
        let mut by_name = std::collections::HashMap::<Option<String>, SpeciesStat>::new();
        for e in self.events_since(since)? {
            if e.label != Label::Animal || camera.is_some_and(|c| e.camera_id != c) {
                continue;
            }
            // Names are grouped ignoring case, like the `species` filter of `list_events`.
            let key = e.species.as_ref().map(|s| s.common_name.to_lowercase());
            let score = e.species.as_ref().map_or(e.top_score, |s| s.score);
            let stat = by_name.entry(key).or_insert_with(|| SpeciesStat {
                common_name: e.species.as_ref().map(|s| s.common_name.clone()),
                scientific_name: e.species.as_ref().map(|s| s.scientific_name.clone()),
                count: 0,
                last_seen: e.started_at,
                best_event_id: e.id,
                best_score: score,
            });
            stat.count += 1;
            if e.started_at > stat.last_seen {
                stat.last_seen = e.started_at;
            }
            let has_clip = e.clip_state == ClipState::Ready;
            if score > stat.best_score && has_clip {
                stat.best_score = score;
                stat.best_event_id = e.id;
            }
        }
        let mut out: Vec<SpeciesStat> = by_name.into_values().collect();
        out.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.common_name.cmp(&b.common_name))
        });
        Ok(out)
    }

    /// Event counts per local hour and label for one local date in the station time zone.
    pub fn stats_hourly(&self, date: NaiveDate, camera: Option<&str>) -> Result<[HourCounts; 24]> {
        let mut hours = [HourCounts::default(); 24];
        // The local day can be 23–25 h long around DST; widen the UTC range and filter by date.
        let start = self.local_midnight(date) - chrono::Duration::hours(3);
        let end =
            self.local_midnight(date + chrono::Duration::days(1)) + chrono::Duration::hours(3);
        for e in self.events_between(start, Some(end))? {
            if e.local_date == date && camera.is_none_or(|c| e.camera_id == c) {
                hours[e.local_hour as usize % 24].add(e.label);
            }
        }
        Ok(hours)
    }

    fn local_midnight(&self, date: NaiveDate) -> DateTime<Utc> {
        let naive = date.and_hms_opt(0, 0, 0).expect("midnight exists");
        self.tz
            .from_local_datetime(&naive)
            .earliest()
            .map_or_else(|| Utc.from_utc_datetime(&naive), |d| d.with_timezone(&Utc))
    }

    /// Closes events left open by a crash or power cut: `ended_at` becomes `started_at` (the real
    /// end is unknown) and pending clips are marked failed. Returns how many were closed.
    pub fn close_dangling_events(&self) -> Result<u64> {
        let txn = self.db.begin_write()?;
        let mut closed = 0;
        {
            let mut events = txn.open_table(EVENTS)?;
            let mut open = Vec::new();
            for entry in events.iter()? {
                let (_, value) = entry?;
                let record: EventRecord = serde_json::from_slice(value.value())?;
                if record.ended_at.is_none() || record.clip_state == ClipState::Pending {
                    open.push(record);
                }
            }
            for mut record in open {
                if record.ended_at.is_none() {
                    record.ended_at = Some(record.started_at);
                }
                if record.clip_state == ClipState::Pending {
                    record.clip_state = ClipState::Failed;
                }
                events.insert(record.id, serde_json::to_vec(&record)?.as_slice())?;
                closed += 1;
            }
        }
        txn.commit()?;
        Ok(closed)
    }

    // -----------------------------------------------------------------------------------------
    // Recording segments
    // -----------------------------------------------------------------------------------------

    /// Records a finished recording segment.
    pub fn insert_segment(&self, camera: &str, segment: &SegmentRecord) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(SEGMENTS)?.insert(
            (camera, micros(segment.started_at)),
            serde_json::to_vec(segment)?.as_slice(),
        )?;
        txn.commit()?;
        Ok(())
    }

    /// Segments of `camera` that overlap `[from, to)`, oldest first.
    pub fn segments_between(
        &self,
        camera: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<SegmentRecord>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SEGMENTS)?;
        let mut out = Vec::new();
        let range = (camera, micros(from) - MAX_SEGMENT_US)..(camera, micros(to));
        for entry in table.range(range)? {
            let (_, value) = entry?;
            let segment: SegmentRecord = serde_json::from_slice(value.value())?;
            if segment.ended_at > from && segment.started_at < to {
                out.push(segment);
            }
        }
        Ok(out)
    }

    /// Every segment (of every camera) that started before `before`, oldest first.
    pub fn segments_before(&self, before: DateTime<Utc>) -> Result<Vec<(String, SegmentRecord)>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SEGMENTS)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let (camera, start) = key.value();
            if start < micros(before) {
                let segment: SegmentRecord = serde_json::from_slice(value.value())?;
                out.push((camera.to_string(), segment));
            }
        }
        out.sort_by_key(|(_, s)| s.started_at);
        Ok(out)
    }

    /// Forgets a segment (after its files were deleted).
    pub fn delete_segment(&self, camera: &str, started_at: DateTime<Utc>) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(SEGMENTS)?
            .remove((camera, micros(started_at)))?;
        txn.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Reolink Hub imports
    // -----------------------------------------------------------------------------------------

    /// `true` if this Hub recording was already imported.
    pub fn hub_import_seen(&self, hub: &str, file: &str) -> Result<bool> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(HUB_IMPORTS)?.get((hub, file))?.is_some())
    }

    /// Marks a Hub recording as imported; `event_id` is `None` when nothing was kept.
    pub fn record_hub_import(&self, hub: &str, file: &str, event_id: Option<u64>) -> Result<()> {
        let txn = self.db.begin_write()?;
        txn.open_table(HUB_IMPORTS)?
            .insert((hub, file), event_id.map_or(-1, |id| id as i64))?;
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
