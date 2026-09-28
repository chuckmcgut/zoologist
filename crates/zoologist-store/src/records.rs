//! Stored record types (plan §3.4).

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use zoologist_core::{BBox, Label, SpeciesGuess};

/// State of an event's video clip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipState {
    /// Waiting for the recording segments to be written.
    #[default]
    Pending,
    Ready,
    Failed,
    /// Deleted by the retention janitor; the event itself is kept.
    Purged,
}

/// One stored event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: u64,
    pub camera_id: String,
    pub label: Label,
    pub raw_class: Option<String>,
    pub started_at: DateTime<Utc>,
    /// `None` while the event is still going on.
    pub ended_at: Option<DateTime<Utc>>,
    /// Date and hour in the station time zone, for daily charts.
    pub local_date: NaiveDate,
    pub local_hour: u8,
    pub top_score: f32,
    pub median_score: f32,
    pub best_bbox: Option<BBox>,
    pub species: Option<SpeciesGuess>,
    /// Paths are relative to the data directory.
    pub snapshot_path: Option<String>,
    pub thumb_path: Option<String>,
    pub clip_path: Option<String>,
    pub clip_bytes: Option<u64>,
    pub clip_state: ClipState,
}

/// Fields of a new event; the store fills in the id, local date and clip state.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    pub camera_id: String,
    pub label: Label,
    pub raw_class: Option<String>,
    pub started_at: DateTime<Utc>,
    pub top_score: f32,
    pub median_score: f32,
    pub best_bbox: Option<BBox>,
    pub snapshot_path: Option<String>,
    pub thumb_path: Option<String>,
}

/// A partial update. `None` leaves a field unchanged; for the `Option<Option<_>>` fields,
/// `Some(None)` clears the value (used when clips are purged).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventPatch {
    /// A corrected label, e.g. an "animal" the species classifier recognised as a person.
    pub label: Option<Label>,
    pub ended_at: Option<DateTime<Utc>>,
    pub top_score: Option<f32>,
    pub median_score: Option<f32>,
    pub best_bbox: Option<BBox>,
    pub species: Option<SpeciesGuess>,
    pub snapshot_path: Option<Option<String>>,
    pub thumb_path: Option<Option<String>>,
    pub clip_path: Option<Option<String>>,
    pub clip_bytes: Option<Option<u64>>,
    pub clip_state: Option<ClipState>,
}

impl EventPatch {
    pub(crate) fn apply(&self, r: &mut EventRecord) {
        if let Some(v) = self.label {
            r.label = v;
        }
        if let Some(v) = self.ended_at {
            r.ended_at = Some(v);
        }
        if let Some(v) = self.top_score {
            r.top_score = v;
        }
        if let Some(v) = self.median_score {
            r.median_score = v;
        }
        if let Some(v) = self.best_bbox {
            r.best_bbox = Some(v);
        }
        if let Some(v) = &self.species {
            r.species = Some(v.clone());
        }
        if let Some(v) = &self.snapshot_path {
            r.snapshot_path = v.clone();
        }
        if let Some(v) = &self.thumb_path {
            r.thumb_path = v.clone();
        }
        if let Some(v) = &self.clip_path {
            r.clip_path = v.clone();
        }
        if let Some(v) = self.clip_bytes {
            r.clip_bytes = v;
        }
        if let Some(v) = self.clip_state {
            r.clip_state = v;
        }
    }
}

/// Sort order for [`EventQuery`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    Asc,
    /// Newest first.
    #[default]
    Desc,
}

/// Filters and paging for listing events.
#[derive(Clone, Debug, PartialEq)]
pub struct EventQuery {
    /// Only ids greater than this.
    pub after_id: Option<u64>,
    /// Only ids smaller than this.
    pub before_id: Option<u64>,
    /// 1..=[`crate::MAX_PAGE`].
    pub limit: usize,
    pub order: Order,
    pub camera: Option<String>,
    pub label: Option<Label>,
    /// Matches the species' common or scientific name, ignoring case.
    pub species: Option<String>,
    /// Only events that started at or after this time.
    pub since: Option<DateTime<Utc>>,
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            before_id: None,
            limit: 50,
            order: Order::Desc,
            camera: None,
            label: None,
            species: None,
            since: None,
        }
    }
}

impl EventQuery {
    pub(crate) fn matches(&self, e: &EventRecord) -> bool {
        self.camera.as_deref().is_none_or(|c| e.camera_id == c)
            && self.label.is_none_or(|l| e.label == l)
            && self.since.is_none_or(|since| e.started_at >= since)
            && self.species.as_deref().is_none_or(|wanted| {
                e.species.as_ref().is_some_and(|s| {
                    s.common_name.eq_ignore_ascii_case(wanted)
                        || s.scientific_name.eq_ignore_ascii_case(wanted)
                })
            })
    }
}

/// A page of events plus cursors for the next page.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    pub items: Vec<EventRecord>,
    /// Highest id in this page (use as `after_id` to continue ascending).
    pub next_after_id: Option<u64>,
    /// Lowest id in this page (use as `before_id` to continue descending).
    pub next_before_id: Option<u64>,
}

/// One bar of the species chart.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeciesStat {
    /// `None` for animals whose species could not be identified.
    pub common_name: Option<String>,
    pub scientific_name: Option<String>,
    pub count: u64,
    pub last_seen: DateTime<Utc>,
    /// The event to play for "best recording" (highest score with a clip, if any).
    pub best_event_id: u64,
    pub best_score: f32,
}

/// Event counts for one hour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HourCounts {
    pub person: u64,
    pub vehicle: u64,
    pub animal: u64,
    pub motion: u64,
}

impl HourCounts {
    pub(crate) fn add(&mut self, label: Label) {
        match label {
            Label::Person => self.person += 1,
            Label::Vehicle => self.vehicle += 1,
            Label::Animal => self.animal += 1,
            Label::Motion => self.motion += 1,
        }
    }
}

/// One recording segment file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentRecord {
    /// Relative to the data directory.
    pub path: String,
    /// The sample index (`.idx`) next to the file.
    pub index_path: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub bytes: u64,
}
