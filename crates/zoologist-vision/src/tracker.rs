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
}

impl Tracker {
    pub fn new(cfg: &TrackingConfig, max_crops: usize) -> Self {
        Self {
            cfg: cfg.clone(),
            max_crops: max_crops.max(1),
            next_id: 1,
            tracks: Vec::new(),
        }
    }

    /// Active tracks (confirmed or not).
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// `true` if any track is confirmed.
    pub fn has_confirmed(&self) -> bool {
        self.tracks.iter().any(|t| t.confirmed)
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
            if let Some(event) =
                observe(&mut self.tracks[ti], cfg, max_crops, now, frame, &dets[di])
            {
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
            .filter(|t| t.confirmed)
            .map(TrackEvent::Ended)
            .collect()
    }

    fn expire(&mut self, now: DateTime<Utc>) -> Vec<TrackEvent> {
        let max_missed =
            chrono::Duration::milliseconds((self.cfg.max_missed_seconds * 1000.0) as i64);
        let mut ended = Vec::new();
        self.tracks.retain(|t| {
            if now - t.last_detected > max_missed {
                if t.confirmed {
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
    track.bbox = det.bbox;
    track.last_detected = now;
    track.hits += 1;
    track.scores.push(det.score);
    if track.scores.len() > MAX_SCORES {
        track.scores.remove(0);
    }
    let crop_improved = add_crop(track, max_crops, frame, det);

    if !track.confirmed {
        if track.hits >= cfg.min_hits && track.median_score() >= cfg.min_event_score {
            track.confirmed = true;
            track.last_update_sent = Some(now);
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
        let mut tr = tracker();
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
