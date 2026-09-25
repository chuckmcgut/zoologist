//! Following detected objects across frames (plan Step 5.1).
//!
//! Each detection is matched to an existing track of the same label by IoU with the track's
//! predicted position (constant velocity). A track becomes *confirmed* (an event) after enough
//! hits with a good median score, and *ends* when it has not been detected for a while.

use chrono::{DateTime, Utc};
use zoologist_core::config::TrackingConfig;
use zoologist_core::{BBox, Detection, Frame, Label};

/// Scores kept per track for the median (older ones are dropped).
const MAX_SCORES: usize = 100;
/// Crops kept for species classification come from different slots of this length.
const CROP_SLOT_MS: i64 = 1000;
/// `Updated` events are sent at most this often per track (unless the best crop improves).
const UPDATE_INTERVAL_MS: i64 = 1000;
/// A box overlapping its previous position by at least this much has not moved.
const STILL_IOU: f32 = 0.8;
/// Movement counts once the box has stayed away from where it started for this long.
const MOVING_FOR_MS: i64 = 600;
/// Edges this close to the picture's border are cut off by it.
const BORDER: f32 = 0.01;
/// Growing or shrinking evenly: the smaller side's move is at least this share of the larger.
const BALANCED: f32 = 0.3;
/// A new track overlapping a remembered parked object by this much is that object again.
const SAME_OBJECT_IOU: f32 = 0.6;

/// A good picture of a tracked object, for snapshots and species classification.
#[derive(Clone, Debug)]
pub struct BestCrop {
    pub frame: Frame,
    pub bbox: BBox,
    pub score: f32,
    /// `score × sqrt(area)`: prefers confident and large views.
    pub quality: f32,
}

/// One object followed across frames.
#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub label: Label,
    pub raw_class: String,
    pub first_seen: DateTime<Utc>,
    pub last_detected: DateTime<Utc>,
    pub hits: u32,
    pub scores: Vec<f32>,
    pub bbox: BBox,
    /// Normalised units per second.
    pub velocity: (f32, f32),
    /// Up to `max_crops` crops, best first, each from a different second of the track.
    pub crops: Vec<BestCrop>,
    pub confirmed: bool,
    /// Where the object was first seen, and whether it has moved away from there since.
    origin: BBox,
    pub moved: bool,
    /// Since when the box has been away from `origin` without a break (see [`has_moved`]).
    moving_since: Option<DateTime<Utc>>,
    /// Where the object was when it last moved, and when it stopped there.
    anchor: BBox,
    still_since: DateTime<Utc>,
    /// A parked object: its event has ended and it starts no new ones until it moves.
    pub dormant: bool,
    last_update_sent: Option<DateTime<Utc>>,
}

impl Track {
    /// Median of the recorded scores.
    pub fn median_score(&self) -> f32 {
        median(&self.scores)
    }

    /// Highest recorded score.
    pub fn top_score(&self) -> f32 {
        self.scores.iter().copied().fold(0.0, f32::max)
    }

    /// The best crop, if any.
    pub fn best(&self) -> Option<&BestCrop> {
        self.crops.first()
    }

    /// Where the object should be at `now`, assuming it keeps moving the same way.
    pub fn predicted(&self, now: DateTime<Utc>) -> BBox {
        let dt = seconds(now - self.last_detected).min(2.0);
        let (dx, dy) = (self.velocity.0 * dt, self.velocity.1 * dt);
        BBox {
            x1: self.bbox.x1 + dx,
            y1: self.bbox.y1 + dy,
            x2: self.bbox.x2 + dx,
            y2: self.bbox.y2 + dy,
        }
    }
}

/// What happened to tracks during an update.
#[derive(Clone, Debug)]
pub enum TrackEvent {
    /// The track just became an event.
    Confirmed(Track),
    /// A confirmed track moved on (at most once per second) or got a better crop.
    Updated(Track),
    /// A confirmed track has not been seen for `max_missed_seconds`.
    Ended(Track),
}

/// Tracks for one camera.
pub struct Tracker {
    cfg: TrackingConfig,
    max_crops: usize,
    next_id: u64,
    tracks: Vec<Track>,
    /// Objects that stopped moving, so that seeing them again starts no event.
    parked: Vec<Parked>,
}

/// An object that parked: where it stands and when it was last seen there.
#[derive(Clone, Debug)]
struct Parked {
    label: Label,
    bbox: BBox,
    seen: DateTime<Utc>,
}

impl Tracker {
    pub fn new(cfg: &TrackingConfig, max_crops: usize) -> Self {
        Self {
            cfg: cfg.clone(),
            max_crops: max_crops.max(1),
            next_id: 1,
            tracks: Vec::new(),
            parked: Vec::new(),
        }
    }

    /// Active tracks (confirmed or not).
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// `true` if an object is being watched right now. Parked objects do not count: motion
    /// elsewhere should still make motion events.
    pub fn has_confirmed(&self) -> bool {
        self.tracks.iter().any(|t| t.confirmed && !t.dormant)
    }

    /// Predicted boxes of tracks not detected for at least `every`, so the caller can run the
    /// detector on them even without motion (a stopped animal is still there).
    pub fn keepalive_due(&self, now: DateTime<Utc>, every: chrono::Duration) -> Vec<BBox> {
        self.tracks
            .iter()
            .filter(|t| now - t.last_detected >= every)
            .map(|t| t.predicted(now).clamp())
            .collect()
    }

    /// Adds the detections of one frame taken at `now`.
    pub fn update(
        &mut self,
        now: DateTime<Utc>,
        frame: &Frame,
        dets: &[Detection],
    ) -> Vec<TrackEvent> {
        let mut events = self.expire(now);

        // Greedy matching by highest IoU with the predicted position, same label only.
        let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
        for (ti, track) in self.tracks.iter().enumerate() {
            let predicted = track.predicted(now);
            for (di, det) in dets.iter().enumerate() {
                if det.label != track.label {
                    continue;
                }
                let iou = predicted.iou(&det.bbox).max(track.bbox.iou(&det.bbox));
                if iou >= self.cfg.iou_match {
                    pairs.push((iou, ti, di));
                }
            }
        }
        pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut track_used = vec![false; self.tracks.len()];
        let mut det_used = vec![false; dets.len()];
        for (_, ti, di) in pairs {
            if track_used[ti] || det_used[di] {
                continue;
            }
            track_used[ti] = true;
            det_used[di] = true;
            let (cfg, max_crops) = (&self.cfg, self.max_crops);
            let event = observe(&mut self.tracks[ti], cfg, max_crops, now, frame, &dets[di]);
            // Keep remembering a parked object while it stays in view.
            if self.tracks[ti].dormant {
                let track = self.tracks[ti].clone();
                self.remember_parked(&track, now);
            }
            if let Some(event) = event {
                events.push(event);
            }
        }

        for (di, det) in dets.iter().enumerate() {
            if det_used[di] || det.label == Label::Motion {
                continue;
            }
            let mut track = Track {
                id: self.next_id,
                label: det.label,
                raw_class: det.raw_class.clone(),
                first_seen: now,
                last_detected: now,
                hits: 0,
                scores: Vec::new(),
                bbox: det.bbox,
                velocity: (0.0, 0.0),
                crops: Vec::new(),
                confirmed: false,
                origin: det.bbox,
                moved: false,
                moving_since: None,
                anchor: det.bbox,
                still_since: now,
                // An object found where a parked one was is that object: no new event.
                dormant: self.is_parked(det.label, &det.bbox, now),
                last_update_sent: None,
            };
            self.next_id += 1;
            if let Some(event) = observe(&mut track, &self.cfg, self.max_crops, now, frame, det) {
                events.push(event);
            }
            self.tracks.push(track);
        }
        events
    }

    /// Ends tracks that have not been seen for too long. Call regularly even without frames.
    pub fn tick(&mut self, now: DateTime<Utc>) -> Vec<TrackEvent> {
        self.expire(now)
    }

    /// Ends every track (shutdown, or the end of an imported file).
    pub fn finish(&mut self) -> Vec<TrackEvent> {
        self.tracks
            .drain(..)
            .filter(|t| t.confirmed && !t.dormant)
            .map(TrackEvent::Ended)
            .collect()
    }

    /// True if `bbox` is where a parked object of the same label was last seen. Remembering is
    /// refreshed, so an object that stays parked is not forgotten while it is visible.
    fn is_parked(&mut self, label: Label, bbox: &BBox, now: DateTime<Utc>) -> bool {
        let forget =
            chrono::Duration::minutes(i64::from(self.cfg.stationary_forget_minutes).max(0));
        self.parked.retain(|p| now - p.seen <= forget);
        match self
            .parked
            .iter_mut()
            .find(|p| p.label == label && p.bbox.iou(bbox) >= SAME_OBJECT_IOU)
        {
            Some(parked) => {
                parked.seen = now;
                true
            }
            None => false,
        }
    }

    /// Remembers a parked object (or refreshes it).
    fn remember_parked(&mut self, track: &Track, now: DateTime<Utc>) {
        if let Some(parked) = self
            .parked
            .iter_mut()
            .find(|p| p.label == track.label && p.bbox.iou(&track.bbox) >= SAME_OBJECT_IOU)
        {
            parked.bbox = track.bbox;
            parked.seen = now;
            return;
        }
        self.parked.push(Parked {
            label: track.label,
            bbox: track.bbox,
            seen: now,
        });
    }

    fn expire(&mut self, now: DateTime<Utc>) -> Vec<TrackEvent> {
        let max_missed =
            chrono::Duration::milliseconds((self.cfg.max_missed_seconds * 1000.0) as i64);
        let mut ended = Vec::new();
        self.tracks.retain(|t| {
            if now - t.last_detected > max_missed {
                if t.confirmed && !t.dormant {
                    ended.push(TrackEvent::Ended(t.clone()));
                }
                false
            } else {
                true
            }
        });
        ended
    }
}

/// True when an object seen at `origin` and now at `now` has really moved, not just been boxed
/// differently. The detector often boxes part of a parked car, or the car plus a shed, so the
/// box jumps around while one side stays where it was. Only these count:
/// - the whole box shifted by `min_movement` × its diagonal: along an axis, both opposite edges
///   moved the same way (the shift is the smaller of the two);
/// - the box grew or shrank a lot with the same shape and its left and right edges moved apart
///   (or together) about equally: driving towards or away from the camera.
///
/// An edge on the picture's border says nothing: the object may go on beyond it.
fn has_moved(origin: &BBox, now: &BBox, min_movement: f32) -> bool {
    let on_border = |v: f32| !(BORDER..=1.0 - BORDER).contains(&v);
    let visible = |a: f32, b: f32| !on_border(a) && !on_border(b);
    // Shift along one axis: both edges visible and moved the same way.
    let shift = |a1: f32, a2: f32, b1: f32, b2: f32| {
        let (d1, d2) = (b1 - a1, b2 - a2);
        if visible(a1, b1) && visible(a2, b2) && d1 * d2 > 0.0 {
            d1.abs().min(d2.abs())
        } else {
            0.0
        }
    };
    let dx = shift(origin.x1, origin.x2, now.x1, now.x2);
    let dy = shift(origin.y1, origin.y2, now.y1, now.y2);
    let diagonal = (origin.width().powi(2) + origin.height().powi(2)).sqrt();
    if (dx * dx + dy * dy).sqrt() > min_movement * diagonal {
        return true;
    }

    let area = |b: &BBox| (b.width() * b.height()).max(f32::EPSILON);
    let aspect = |b: &BBox| b.width().max(f32::EPSILON) / b.height().max(f32::EPSILON);
    let size_ratio = area(now) / area(origin);
    let same_shape = (aspect(now) / aspect(origin) - 1.0).abs() <= 0.25;
    if !same_shape || (0.55..=1.8).contains(&size_ratio) {
        return false;
    }
    // Growing: the left edge moves left and the right edge right (shrinking: the reverse).
    let (left, right) = (origin.x1 - now.x1, now.x2 - origin.x2);
    visible(origin.x1, now.x1)
        && visible(origin.x2, now.x2)
        && left * right > 0.0
        && left.abs().min(right.abs()) >= BALANCED * left.abs().max(right.abs())
}

/// Applies a matched detection to a track and returns the event it causes, if any.
fn observe(
    track: &mut Track,
    cfg: &TrackingConfig,
    max_crops: usize,
    now: DateTime<Utc>,
    frame: &Frame,
    det: &Detection,
) -> Option<TrackEvent> {
    if track.hits > 0 {
        let dt = seconds(now - track.last_detected);
        if dt > 0.0 {
            let (ox, oy) = track.bbox.center();
            let (nx, ny) = det.bbox.center();
            let v = ((nx - ox) / dt, (ny - oy) / dt);
            track.velocity = (
                0.5 * track.velocity.0 + 0.5 * v.0,
                0.5 * track.velocity.1 + 0.5 * v.1,
            );
        }
    }
    // Movement: a box that still overlaps where it stopped has not moved.
    let moved = track.anchor.iou(&det.bbox) < STILL_IOU;
    if moved {
        if track.dormant {
            // Its box changed: it may be driving off, or the detector may just have boxed it
            // differently. Either way it is a new visit that has to move away from where it
            // parked before it becomes an event again.
            track.dormant = false;
            track.confirmed = false;
            track.moved = false;
            track.moving_since = None;
            track.origin = track.anchor;
            track.first_seen = now;
            track.hits = 0;
            track.scores.clear();
            track.crops.clear();
            track.last_update_sent = None;
        }
        track.anchor = det.bbox;
        track.still_since = now;
    }
    track.bbox = det.bbox;
    track.last_detected = now;
    track.hits += 1;
    track.scores.push(det.score);
    if track.scores.len() > MAX_SCORES {
        track.scores.remove(0);
    }
    let crop_improved = add_crop(track, max_crops, frame, det);

    if track.dormant {
        return None;
    }
    // Parked: end the event, keep following the object silently.
    let stationary = chrono::Duration::milliseconds((cfg.stationary_seconds * 1000.0) as i64);
    if track.confirmed && cfg.stationary_seconds > 0.0 && now - track.still_since >= stationary {
        track.dormant = true;
        return Some(TrackEvent::Ended(track.clone()));
    }
    if !track.moved {
        track.moved = if cfg.min_movement <= 0.0 || !cfg.require_movement.contains(&track.label) {
            true
        } else if has_moved(&track.origin, &det.bbox, cfg.min_movement) {
            // A redrawn box flickers back; a vehicle that drives stays away.
            let since = *track.moving_since.get_or_insert(now);
            now - since >= chrono::Duration::milliseconds(MOVING_FOR_MS)
        } else {
            track.moving_since = None;
            false
        };
    }
    if !track.confirmed {
        if track.hits >= cfg.min_hits && track.median_score() >= cfg.min_event_score && track.moved
        {
            track.confirmed = true;
            track.last_update_sent = Some(now);
            tracing::debug!(
                id = track.id,
                label = %track.label,
                origin = ?track.origin,
                bbox = ?det.bbox,
                "track confirmed"
            );
            return Some(TrackEvent::Confirmed(track.clone()));
        }
        return None;
    }
    let due = track
        .last_update_sent
        .is_none_or(|t| (now - t).num_milliseconds() >= UPDATE_INTERVAL_MS);
    if due || crop_improved {
        track.last_update_sent = Some(now);
        return Some(TrackEvent::Updated(track.clone()));
    }
    None
}

/// Offers the detection as a crop. Returns `true` if it became the best crop.
///
/// The track's lifetime is split into 1-second slots; each slot keeps its best crop and the
/// track keeps the best `max_crops` slots. This spreads crops over the whole visit instead of
/// clustering them around the single best moment.
fn add_crop(track: &mut Track, max_crops: usize, frame: &Frame, det: &Detection) -> bool {
    let slot = |f: &Frame| (f.captured_at - track.first_seen).num_milliseconds() / CROP_SLOT_MS;
    let crop = BestCrop {
        frame: frame.clone(),
        bbox: det.bbox,
        score: det.score,
        quality: det.score * det.bbox.area().sqrt(),
    };
    let previous_best = track.crops.first().map(|c| c.quality);
    let new_slot = slot(&crop.frame);
    if let Some(i) = track.crops.iter().position(|c| slot(&c.frame) == new_slot) {
        if crop.quality <= track.crops[i].quality {
            return false;
        }
        track.crops[i] = crop;
    } else {
        track.crops.push(crop);
    }
    track.crops.sort_by(|a, b| b.quality.total_cmp(&a.quality));
    track.crops.truncate(max_crops);
    previous_best.is_none_or(|q| track.crops[0].quality > q)
}

fn median(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

fn seconds(d: chrono::Duration) -> f32 {
    d.num_milliseconds() as f32 / 1000.0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;

    use super::*;

    fn t(ms: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, 12, 0, 0).unwrap() + chrono::Duration::milliseconds(ms)
    }

    fn frame(ms: i64) -> Frame {
        Frame {
            camera_id: "cam".into(),
            seq: ms as u64,
            captured_at: t(ms),
            width: 2,
            height: 2,
            i420: Arc::new(vec![0; 6]),
        }
    }

    fn det(label: Label, x: f32, y: f32, size: f32, score: f32) -> Detection {
        Detection {
            label,
            raw_class: label.as_str().into(),
            score,
            bbox: BBox::new(x, y, x + size, y + size),
        }
    }

    fn tracker() -> Tracker {
        Tracker::new(&TrackingConfig::default(), 3)
    }

    /// Runs frames 200 ms apart; returns (frame index, event) pairs.
    fn run(tracker: &mut Tracker, frames: &[Vec<Detection>]) -> Vec<(usize, TrackEvent)> {
        let mut out = Vec::new();
        for (i, dets) in frames.iter().enumerate() {
            let ms = i as i64 * 200;
            for e in tracker.update(t(ms), &frame(ms), dets) {
                out.push((i, e));
            }
        }
        out
    }

    fn confirmed(events: &[(usize, TrackEvent)]) -> Vec<(usize, u64)> {
        events
            .iter()
            .filter_map(|(i, e)| match e {
                TrackEvent::Confirmed(t) => Some((*i, t.id)),
                _ => None,
            })
            .collect()
    }

    fn ended(events: &[(usize, TrackEvent)]) -> Vec<(usize, u64)> {
        events
            .iter()
            .filter_map(|(i, e)| match e {
                TrackEvent::Ended(t) => Some((*i, t.id)),
                _ => None,
            })
            .collect()
    }

    /// A parked car: one event that ends when it stops, and nothing afterwards, even when the
    /// track is lost and found again. When it drives off, a new event starts.
    #[test]
    fn a_parked_object_ends_its_event_and_starts_no_new_ones() {
        let cfg = TrackingConfig {
            stationary_seconds: 2.0,
            ..TrackingConfig::default()
        };
        let mut tracker = Tracker::new(&cfg, 3);
        let car = |x: f32| vec![det(Label::Vehicle, x, 0.5, 0.2, 0.9)];
        // One timeline, 200 ms per frame: drives in, parks, is lost, is found, drives off.
        let mut frames: Vec<Vec<Detection>> = (0..5).map(|i| car(0.1 + i as f32 * 0.08)).collect();
        let parked_from = frames.len();
        frames.extend((0..20).map(|_| car(0.42)));
        let lost_from = frames.len();
        frames.extend((0..60).map(|_| Vec::new()));
        let found_from = frames.len();
        frames.extend((0..10).map(|_| car(0.42)));
        let leaving_from = frames.len();
        frames.extend((1..=6).map(|i| car(0.42 + i as f32 * 0.08)));

        let events = run(&mut tracker, &frames);
        let starts = confirmed(&events);
        let ends = ended(&events);
        assert_eq!(starts.len(), 2, "driving in and driving off: {events:?}");
        assert!(starts[0].0 < parked_from, "the first event is the arrival");
        assert!(
            starts[1].0 >= leaving_from,
            "the second is the departure: {starts:?}"
        );
        assert_eq!(
            ends.len(),
            1,
            "the arrival event ends when it parks: {ends:?}"
        );
        // Standing still is counted from the last frame that moved (index 4), so 2 s later.
        assert!(
            (parked_from + 8..parked_from + 12).contains(&ends[0].0),
            "about 2 s after it stopped: {ends:?}"
        );
        // Nothing at all while it stands there, lost or found again.
        let quiet: Vec<_> = events
            .iter()
            .filter(|(i, _)| (ends[0].0 + 1..leaving_from).contains(i))
            .collect();
        assert!(quiet.is_empty(), "a parked car makes no events: {quiet:?}");
        assert!(found_from > lost_from);
    }

    /// The Container camera case: a parked truck found again and again, its box jumping
    /// between the whole truck, half of it and the truck plus a shed. It never moves, so it is
    /// never an event.
    #[test]
    fn an_object_that_never_moves_is_never_an_event_whatever_its_box() {
        let mut tr = tracker();
        let shapes = [
            BBox::new(0.28, 0.54, 0.46, 1.0),  // the whole truck
            BBox::new(0.28, 0.54, 0.40, 1.0),  // its front half
            BBox::new(0.28, 0.54, 0.44, 0.76), // its upper part
            BBox::new(0.27, 0.50, 0.50, 1.0),  // truck and a bit of shed
        ];
        let frames: Vec<Vec<Detection>> = (0..200)
            .map(|i| {
                vec![Detection {
                    label: Label::Vehicle,
                    raw_class: "vehicle".into(),
                    score: 0.9,
                    bbox: shapes[i % shapes.len()],
                }]
            })
            .collect();
        let events = run(&mut tr, &frames);
        assert!(confirmed(&events).is_empty(), "{events:?}");
    }

    /// The NC200 case: a truck parks, and afterwards the detector's box around it jumps now and
    /// then. That wakes the track, but it must not start a new event unless the truck leaves.
    #[test]
    fn a_parked_object_whose_box_jumps_starts_no_new_event() {
        let cfg = TrackingConfig {
            stationary_seconds: 2.0,
            ..TrackingConfig::default()
        };
        let mut tr = Tracker::new(&cfg, 3);
        let car = |x: f32| vec![det(Label::Vehicle, x, 0.5, 0.2, 0.9)];
        let parked = BBox::new(0.42, 0.5, 0.62, 0.7);
        let jumps = [
            parked,
            BBox::new(0.42, 0.5, 0.55, 0.7),  // its front
            BBox::new(0.40, 0.46, 0.66, 0.7), // with a bit of shed
        ];
        let mut frames: Vec<Vec<Detection>> = (0..5).map(|i| car(0.1 + i as f32 * 0.08)).collect();
        frames.extend((0..20).map(|_| car(0.42)));
        // Three minutes parked; the box jumps for a few frames every 10 s.
        frames.extend((0..900).map(|i| {
            let bbox = if i % 50 < 45 {
                parked
            } else {
                jumps[(i / 50) % 3]
            };
            vec![Detection {
                label: Label::Vehicle,
                raw_class: "vehicle".into(),
                score: 0.9,
                bbox,
            }]
        }));
        let leaving_from = frames.len();
        frames.extend((1..=6).map(|i| car(0.42 + i as f32 * 0.08)));

        let events = run(&mut tr, &frames);
        let starts = confirmed(&events);
        assert_eq!(starts.len(), 2, "arrival and departure only: {starts:?}");
        assert!(starts[1].0 >= leaving_from, "{starts:?}");
        assert_eq!(ended(&events).len(), 1, "{events:?}");
    }

    /// Box pairs (first box, box when it would have become an event) logged on a camera facing a
    /// parked truck: parts of the truck, the whole truck, the truck plus a vehicle beside it.
    /// None of them is movement. (One more, the windshield then the truck's upper two thirds,
    /// grows evenly enough to pass; only [`MOVING_FOR_MS`] stops that one.)
    #[test]
    fn reboxing_a_parked_truck_is_not_movement() {
        let pairs = [
            ((0.411, 0.364, 0.611, 0.577), (0.388, 0.362, 0.864, 0.998)),
            ((0.390, 0.363, 0.996, 1.0), (0.386, 0.359, 0.789, 0.835)),
            ((0.387, 0.469, 0.577, 0.999), (0.391, 0.355, 0.623, 0.841)),
            ((0.406, 0.372, 0.673, 0.619), (0.389, 0.335, 0.999, 0.998)),
            ((0.387, 0.453, 0.542, 0.873), (0.387, 0.367, 0.584, 0.998)),
            ((0.409, 0.371, 0.595, 0.633), (0.388, 0.360, 0.773, 0.994)),
            ((0.395, 0.360, 0.591, 0.717), (0.387, 0.361, 0.853, 0.995)),
            ((0.388, 0.358, 0.605, 0.733), (0.388, 0.357, 0.622, 0.999)),
            ((0.750, 0.245, 1.0, 0.658), (0.686, 0.327, 0.999, 0.774)),
        ];
        let b = |(x1, y1, x2, y2): (f32, f32, f32, f32)| BBox::new(x1, y1, x2, y2);
        for (origin, now) in pairs {
            assert!(!has_moved(&b(origin), &b(now), 0.2), "{origin:?} → {now:?}");
        }
    }

    #[test]
    fn a_car_driving_past_is_an_event_and_a_still_animal_still_is() {
        let mut tr = tracker();
        let frames: Vec<Vec<Detection>> = (0..10)
            .map(|i| {
                vec![
                    det(Label::Vehicle, 0.05 + i as f32 * 0.05, 0.5, 0.2, 0.9),
                    det(Label::Animal, 0.7, 0.1, 0.1, 0.9),
                ]
            })
            .collect();
        let events = run(&mut tr, &frames);
        let labels: Vec<Label> = events
            .iter()
            .filter_map(|(_, e)| match e {
                TrackEvent::Confirmed(t) => Some(t.label),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 2, "{events:?}");
        assert!(labels.contains(&Label::Vehicle) && labels.contains(&Label::Animal));
    }

    #[test]
    fn driving_towards_the_camera_counts_as_moving() {
        let mut tr = tracker();
        // Same place, same shape, growing: a car driving straight at the camera.
        let frames: Vec<Vec<Detection>> = (0..10)
            .map(|i| {
                let size = 0.1 * (1.0 + 0.15 * i as f32);
                let c = 0.5;
                vec![Detection {
                    label: Label::Vehicle,
                    raw_class: "vehicle".into(),
                    score: 0.9,
                    bbox: BBox::new(c - size / 2.0, c - size, c + size / 2.0, c + size),
                }]
            })
            .collect();
        let events = run(&mut tr, &frames);
        assert_eq!(confirmed(&events).len(), 1, "{events:?}");
    }

    #[test]
    fn stationary_seconds_zero_keeps_the_old_behaviour() {
        let cfg = TrackingConfig {
            stationary_seconds: 0.0,
            min_movement: 0.0,
            ..TrackingConfig::default()
        };
        let mut tracker = Tracker::new(&cfg, 3);
        let frames: Vec<Vec<Detection>> = (0..30)
            .map(|_| vec![det(Label::Vehicle, 0.5, 0.5, 0.2, 0.9)])
            .collect();
        let events = run(&mut tracker, &frames);
        assert_eq!(confirmed(&events).len(), 1);
        assert!(
            ended(&events).is_empty(),
            "no parking, so no end: {events:?}"
        );
    }

    #[test]
    fn moving_box_is_one_track_confirmed_at_the_third_hit_then_ends() {
        let mut tr = tracker();
        let frames: Vec<Vec<Detection>> = (0..20)
            .map(|i| vec![det(Label::Person, 0.1 + i as f32 * 0.02, 0.3, 0.15, 0.8)])
            .collect();
        let events = run(&mut tr, &frames);
        assert_eq!(confirmed(&events), vec![(2, 1)]);
        assert_eq!(tr.tracks().len(), 1);
        assert!(tr.tracks()[0].velocity.0 > 0.05); // moving right, ~0.1/s

        // Nothing for 6 s: the track ends once.
        let ended = tr.tick(t(19 * 200 + 6000));
        assert_eq!(ended.len(), 1);
        assert!(matches!(&ended[0], TrackEvent::Ended(t) if t.id == 1 && t.hits == 20));
        assert!(tr.tick(t(40_000)).is_empty());
    }

    #[test]
    fn crossing_objects_of_different_labels_never_swap() {
        let mut tr = tracker();
        let frames: Vec<Vec<Detection>> = (0..15)
            .map(|i| {
                let x = i as f32 * 0.05;
                vec![
                    det(Label::Person, 0.05 + x, 0.4, 0.2, 0.9),
                    det(Label::Animal, 0.75 - x, 0.4, 0.2, 0.9),
                ]
            })
            .collect();
        run(&mut tr, &frames);
        assert_eq!(tr.tracks().len(), 2);
        for track in tr.tracks() {
            assert_eq!(track.hits, 15, "{:?} was split", track.label);
        }
    }

    #[test]
    fn single_frame_false_positive_is_not_an_event() {
        let mut tr = tracker();
        let events = run(
            &mut tr,
            &[
                vec![det(Label::Vehicle, 0.5, 0.5, 0.1, 0.95)],
                vec![],
                vec![],
            ],
        );
        assert!(confirmed(&events).is_empty());
        assert!(tr.tick(t(10_000)).is_empty());
        assert!(tr.tracks().is_empty());
    }

    #[test]
    fn low_median_score_is_not_confirmed() {
        let mut tr = tracker();
        let frames: Vec<Vec<Detection>> = [0.9, 0.3, 0.3, 0.3]
            .iter()
            .map(|&s| vec![det(Label::Animal, 0.4, 0.4, 0.2, s)])
            .collect();
        assert!(confirmed(&run(&mut tr, &frames)).is_empty());
    }

    #[test]
    fn updates_are_rate_limited() {
        let mut tr = tracker();
        let frames: Vec<Vec<Detection>> = (0..16)
            .map(|_| vec![det(Label::Person, 0.4, 0.4, 0.2, 0.8)])
            .collect();
        let events = run(&mut tr, &frames);
        let updates = events
            .iter()
            .filter(|(_, e)| matches!(e, TrackEvent::Updated(_)))
            .count();
        // 16 frames over 3 s: at most one update per second after confirmation.
        assert!(updates <= 3, "{updates} updates");
    }

    #[test]
    fn keeps_best_crops_from_different_seconds() {
        let mut tr = tracker();
        // The animal walks towards the camera: every frame is a bigger, better view.
        let frames: Vec<Vec<Detection>> = (0..20)
            .map(|i| vec![det(Label::Animal, 0.3, 0.3, 0.1 + i as f32 * 0.01, 0.6)])
            .collect();
        run(&mut tr, &frames);
        let crops = &tr.tracks()[0].crops;
        assert_eq!(crops.len(), 3);
        assert!(crops.windows(2).all(|w| w[0].quality >= w[1].quality));
        let mut slots: Vec<i64> = crops
            .iter()
            .map(|c| (c.frame.captured_at - t(0)).num_milliseconds() / CROP_SLOT_MS)
            .collect();
        slots.dedup();
        assert_eq!(slots.len(), 3, "crops must come from different seconds");
        // The biggest (latest) view is the best one.
        assert_eq!(crops[0].frame.captured_at, t(19 * 200));
    }

    #[test]
    fn keepalive_lists_tracks_not_seen_recently() {
        let mut tr = tracker();
        run(&mut tr, &[vec![det(Label::Animal, 0.2, 0.2, 0.1, 0.9)]]);
        assert!(
            tr.keepalive_due(t(1000), chrono::Duration::seconds(2))
                .is_empty()
        );
        assert_eq!(
            tr.keepalive_due(t(2500), chrono::Duration::seconds(2))
                .len(),
            1
        );
    }

    #[test]
    fn finish_ends_only_confirmed_tracks() {
        let cfg = TrackingConfig {
            min_movement: 0.0,
            ..TrackingConfig::default()
        };
        let mut tr = Tracker::new(&cfg, 3);
        let mut frames: Vec<Vec<Detection>> = (0..4)
            .map(|_| vec![det(Label::Person, 0.1, 0.1, 0.2, 0.9)])
            .collect();
        frames.push(vec![
            det(Label::Person, 0.1, 0.1, 0.2, 0.9),
            det(Label::Animal, 0.7, 0.7, 0.1, 0.9),
        ]);
        run(&mut tr, &frames);
        let ended = tr.finish();
        assert_eq!(ended.len(), 1);
        assert!(tr.tracks().is_empty());
    }

    #[test]
    fn median_of_even_and_odd_lists() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }
}
