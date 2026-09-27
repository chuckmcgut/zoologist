//! Turning tracks and motion into events (plan Step 5.2).
//!
//! An event is what the UI lists and the store saves: a confirmed track (person, vehicle,
//! animal), or a period of motion in which no object was found (`motion`).

use chrono::{DateTime, Utc};
use zoologist_core::config::MotionConfig;
use zoologist_core::{BBox, CameraId, Frame, Label};

use crate::tracker::{BestCrop, TrackEvent};

/// Motion stops counting as continuous after a gap this long.
const MOTION_GAP_MS: i64 = 1000;
/// A motion event ends after this long without motion.
const MOTION_END_MS: i64 = 5000;

/// Identifies an event before the store has given it an id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EventKey {
    pub camera_id: CameraId,
    pub source: EventSource,
}

/// What an event came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventSource {
    Track(u64),
    Motion(u64),
}

/// A change to an event.
#[derive(Clone, Debug)]
pub enum EventUpdate {
    Started {
        key: EventKey,
        label: Label,
        raw_class: Option<String>,
        started_at: DateTime<Utc>,
        score: f32,
        snapshot: Frame,
        bbox: Option<BBox>,
    },
    Updated {
        key: EventKey,
        top_score: f32,
        best: Option<BestCrop>,
    },
    Ended {
        key: EventKey,
        ended_at: DateTime<Utc>,
        top_score: f32,
        median_score: f32,
        /// Best crops for species classification (empty for motion events).
        crops: Vec<BestCrop>,
    },
}

impl EventUpdate {
    /// The event this update belongs to.
    pub fn key(&self) -> &EventKey {
        match self {
            EventUpdate::Started { key, .. }
            | EventUpdate::Updated { key, .. }
            | EventUpdate::Ended { key, .. } => key,
        }
    }
}

struct MotionEvent {
    id: u64,
    started_at: DateTime<Utc>,
    last_motion: DateTime<Utc>,
}

/// Per-camera event logic.
pub struct EventManager {
    camera_id: CameraId,
    labels: Vec<Label>,
    min_motion: chrono::Duration,
    cooldown: chrono::Duration,
    /// Track ids that were reported as Started (labels not in `labels` are never reported),
    /// with when their current event started.
    started_tracks: std::collections::HashMap<u64, DateTime<Utc>>,
    /// Longer events are ended and continued as new ones (`None`: no limit).
    max_event: Option<chrono::Duration>,
    /// Start of the current run of continuous motion without objects.
    motion_since: Option<DateTime<Utc>>,
    last_motion_seen: Option<DateTime<Utc>>,
    /// Frame with the most motion in the current run, for the snapshot.
    best_motion_frame: Option<(f32, Frame, BBox)>,
    active_motion: Option<MotionEvent>,
    cooldown_until: Option<DateTime<Utc>>,
    next_motion_id: u64,
}

impl EventManager {
    /// `labels` are the camera's configured labels; others never produce events.
    pub fn new(camera_id: CameraId, labels: &[Label], motion: &MotionConfig) -> Self {
        let ms = |s: f32| chrono::Duration::milliseconds((s * 1000.0) as i64);
        Self {
            camera_id,
            labels: labels.to_vec(),
            min_motion: ms(motion.motion_event_min_seconds),
            cooldown: ms(motion.motion_event_cooldown_seconds),
            started_tracks: Default::default(),
            max_event: None,
            motion_since: None,
            last_motion_seen: None,
            best_motion_frame: None,
            active_motion: None,
            cooldown_until: None,
            next_motion_id: 1,
        }
    }

    /// Ends events after `max` and continues them as new events, so that no clip gets longer
    /// than that (clips are cut in memory-sized pieces and long clips are hard to watch).
    pub fn with_max_event_length(mut self, max: Option<chrono::Duration>) -> Self {
        self.max_event = max.filter(|m| *m > chrono::Duration::zero());
        self
    }

    fn too_long(&self, started_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        self.max_event.is_some_and(|max| now - started_at >= max)
    }

    fn key(&self, source: EventSource) -> EventKey {
        EventKey {
            camera_id: self.camera_id.clone(),
            source,
        }
    }

    /// Converts tracker events into event updates.
    pub fn on_track_events(&mut self, events: Vec<TrackEvent>) -> Vec<EventUpdate> {
        let mut out = Vec::new();
        for event in events {
            match event {
                TrackEvent::Confirmed(track) => {
                    if !self.labels.contains(&track.label) {
                        continue;
                    }
                    let Some(best) = track.best() else { continue };
                    self.started_tracks.insert(track.id, track.first_seen);
                    out.push(EventUpdate::Started {
                        key: self.key(EventSource::Track(track.id)),
                        label: track.label,
                        raw_class: Some(track.raw_class.clone()),
                        started_at: track.first_seen,
                        score: track.top_score(),
                        snapshot: best.frame.clone(),
                        bbox: Some(best.bbox),
                    });
                }
                TrackEvent::Updated(track) => {
                    let Some(&started_at) = self.started_tracks.get(&track.id) else {
                        continue;
                    };
                    let now = track.last_detected;
                    if self.too_long(started_at, now)
                        && let Some(best) = track.best()
                    {
                        // End this part and go on with a new event from here.
                        let key = self.key(EventSource::Track(track.id));
                        out.push(EventUpdate::Ended {
                            key: key.clone(),
                            ended_at: now,
                            top_score: track.top_score(),
                            median_score: track.median_score(),
                            crops: track.crops.clone(),
                        });
                        out.push(EventUpdate::Started {
                            key,
                            label: track.label,
                            raw_class: Some(track.raw_class.clone()),
                            started_at: now,
                            score: track.top_score(),
                            snapshot: best.frame.clone(),
                            bbox: Some(track.bbox),
                        });
                        self.started_tracks.insert(track.id, now);
                    } else {
                        out.push(EventUpdate::Updated {
                            key: self.key(EventSource::Track(track.id)),
                            top_score: track.top_score(),
                            best: track.best().cloned(),
                        });
                    }
                }
                TrackEvent::Ended(track) => {
                    if self.started_tracks.remove(&track.id).is_some() {
                        out.push(EventUpdate::Ended {
                            key: self.key(EventSource::Track(track.id)),
                            ended_at: track.last_detected,
                            top_score: track.top_score(),
                            median_score: track.median_score(),
                            crops: track.crops.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// Handles the motion result of one frame. `objects_active` is true while the tracker has
    /// a confirmed track, in which case motion belongs to that object and makes no motion event.
    pub fn on_motion(
        &mut self,
        now: DateTime<Utc>,
        frame: &Frame,
        motion: &[BBox],
        objects_active: bool,
    ) -> Vec<EventUpdate> {
        let mut out = Vec::new();
        if !self.labels.contains(&Label::Motion) {
            return out;
        }
        let moving = !motion.is_empty();

        // An object took over: the motion is explained, so end any motion event.
        if objects_active {
            self.motion_since = None;
            self.best_motion_frame = None;
            out.extend(self.end_motion(now));
            return out;
        }

        if moving {
            if self
                .last_motion_seen
                .is_some_and(|last| now - last > chrono::Duration::milliseconds(MOTION_GAP_MS))
            {
                // A gap: this is a new run of motion.
                self.motion_since = None;
                self.best_motion_frame = None;
            }
            self.last_motion_seen = Some(now);
            let area: f32 = motion.iter().map(BBox::area).sum();
            let union = motion.iter().skip(1).fold(motion[0], |acc, b| acc.union(b));
            if self
                .best_motion_frame
                .as_ref()
                .is_none_or(|(a, _, _)| area > *a)
            {
                self.best_motion_frame = Some((area, frame.clone(), union));
            }
            let since = *self.motion_since.get_or_insert(now);

            if let Some(active) = &mut self.active_motion {
                active.last_motion = now;
                let (id, started_at) = (active.id, active.started_at);
                if self.too_long(started_at, now) {
                    // End this part and go on with a new motion event, without a cooldown.
                    out.push(EventUpdate::Ended {
                        key: self.key(EventSource::Motion(id)),
                        ended_at: now,
                        top_score: 0.0,
                        median_score: 0.0,
                        crops: Vec::new(),
                    });
                    let id = self.next_motion_id;
                    self.next_motion_id += 1;
                    self.active_motion = Some(MotionEvent {
                        id,
                        started_at: now,
                        last_motion: now,
                    });
                    out.push(EventUpdate::Started {
                        key: self.key(EventSource::Motion(id)),
                        label: Label::Motion,
                        raw_class: None,
                        started_at: now,
                        score: 0.0,
                        snapshot: frame.clone(),
                        bbox: Some(union),
                    });
                }
            } else if now - since >= self.min_motion
                && self.cooldown_until.is_none_or(|until| now >= until)
                && let Some((_, snapshot, bbox)) = self.best_motion_frame.clone()
            {
                let id = self.next_motion_id;
                self.next_motion_id += 1;
                self.active_motion = Some(MotionEvent {
                    id,
                    started_at: since,
                    last_motion: now,
                });
                out.push(EventUpdate::Started {
                    key: self.key(EventSource::Motion(id)),
                    label: Label::Motion,
                    raw_class: None,
                    started_at: since,
                    score: 0.0,
                    snapshot,
                    bbox: Some(bbox),
                });
            }
        } else if self
            .active_motion
            .as_ref()
            .is_some_and(|m| now - m.last_motion > chrono::Duration::milliseconds(MOTION_END_MS))
        {
            out.extend(self.end_motion(now));
        }
        out
    }

    /// Ends a running motion event (also used at shutdown).
    pub fn end_motion(&mut self, now: DateTime<Utc>) -> Option<EventUpdate> {
        let active = self.active_motion.take()?;
        self.motion_since = None;
        self.best_motion_frame = None;
        self.cooldown_until = Some(now + self.cooldown);
        Some(EventUpdate::Ended {
            key: self.key(EventSource::Motion(active.id)),
            ended_at: active.last_motion,
            top_score: 0.0,
            median_score: 0.0,
            crops: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::TimeZone;
    use zoologist_core::Detection;
    use zoologist_core::config::TrackingConfig;

    use super::*;
    use crate::tracker::Tracker;

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

    /// One simulated camera: motion and detections per frame, 200 ms apart.
    struct Sim {
        tracker: Tracker,
        events: EventManager,
        log: Vec<(i64, String)>,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                tracker: Tracker::new(&TrackingConfig::default(), 3),
                events: EventManager::new("cam".into(), &Label::ALL, &MotionConfig::default()),
                log: Vec::new(),
            }
        }

        fn step(&mut self, ms: i64, motion: &[BBox], dets: &[Detection]) {
            let f = frame(ms);
            let track_events = self.tracker.update(t(ms), &f, dets);
            let mut updates = self.events.on_track_events(track_events);
            updates.extend(
                self.events
                    .on_motion(t(ms), &f, motion, self.tracker.has_confirmed()),
            );
            for u in updates {
                let name = match &u {
                    EventUpdate::Started { label, .. } => format!("start {label}"),
                    EventUpdate::Updated { .. } => continue,
                    EventUpdate::Ended { key, .. } => match key.source {
                        EventSource::Track(_) => "end track".into(),
                        EventSource::Motion(_) => "end motion".into(),
                    },
                };
                self.log.push((ms, name));
            }
        }

        fn names(&self) -> Vec<&str> {
            self.log.iter().map(|(_, n)| n.as_str()).collect()
        }
    }

    fn b(x: f32) -> BBox {
        BBox::new(x, 0.3, x + 0.15, 0.6)
    }

    fn person(x: f32) -> Detection {
        Detection {
            label: Label::Person,
            raw_class: "person".into(),
            score: 0.85,
            bbox: b(x),
        }
    }

    #[test]
    fn person_walking_through_is_one_person_event_and_no_motion_event() {
        let mut sim = Sim::new();
        for i in 0..30 {
            let x = i as f32 * 0.03;
            sim.step(i * 200, &[b(x)], &[person(x)]);
        }
        for i in 30..70 {
            sim.step(i * 200, &[], &[]);
        }
        assert_eq!(sim.names(), vec!["start person", "end track"]);
    }

    #[test]
    fn wind_makes_one_motion_event_then_the_cooldown_suppresses_the_next() {
        let mut sim = Sim::new();
        // 10 s of swaying branches, 10 s calm, 10 s of branches again (within the 120 s cooldown).
        for i in 0..50 {
            sim.step(i * 200, &[b(0.7)], &[]);
        }
        for i in 50..100 {
            sim.step(i * 200, &[], &[]);
        }
        for i in 100..150 {
            sim.step(i * 200, &[b(0.7)], &[]);
        }
        assert_eq!(sim.names(), vec!["start motion", "end motion"]);
        // Started once motion lasted 3 s; ended 5 s after it stopped.
        assert_eq!(sim.log[0].0, 3000);
        assert!(sim.log[1].0 >= 49 * 200 + 5000);
    }

    /// A person working in view for 25 s with a 10 s limit: three events back to back.
    #[test]
    fn a_long_visit_is_split_into_events_of_at_most_the_limit() {
        let mut sim = Sim::new();
        sim.events = EventManager::new("cam".into(), &Label::ALL, &MotionConfig::default())
            .with_max_event_length(Some(chrono::Duration::seconds(10)));
        for i in 0..125 {
            let x = 0.05 + i as f32 * 0.005; // walking slowly across
            sim.step(i * 200, &[b(x)], &[person(x)]);
        }
        for i in 125..170 {
            sim.step(i * 200, &[], &[]);
        }
        assert_eq!(
            sim.names(),
            vec![
                "start person",
                "end track",
                "start person",
                "end track",
                "start person",
                "end track"
            ]
        );
        // Each part ends and the next starts on the same frame.
        assert_eq!(sim.log[1].0, sim.log[2].0);
        assert_eq!(sim.log[3].0, sim.log[4].0);
        assert!(sim.log[2].0 - sim.log[0].0 <= 10_200, "{:?}", sim.log);
    }

    #[test]
    fn long_motion_is_split_without_a_cooldown_gap() {
        let mut sim = Sim::new();
        sim.events = EventManager::new("cam".into(), &Label::ALL, &MotionConfig::default())
            .with_max_event_length(Some(chrono::Duration::seconds(10)));
        for i in 0..110 {
            sim.step(i * 200, &[b(0.7)], &[]); // 22 s of swaying branches
        }
        for i in 110..150 {
            sim.step(i * 200, &[], &[]);
        }
        assert_eq!(
            sim.names(),
            vec![
                "start motion",
                "end motion",
                "start motion",
                "end motion",
                "start motion",
                "end motion"
            ]
        );
        // The first event started after 3 s of motion but counts from when motion began (0 s).
        assert_eq!((sim.log[0].0, sim.log[1].0), (3000, 10_000));
        assert_eq!(sim.log[2].0, 10_000);
    }

    #[test]
    fn brief_motion_is_ignored() {
        let mut sim = Sim::new();
        for i in 0..10 {
            sim.step(i * 200, &[b(0.2)], &[]); // 2 s < 3 s
        }
        for i in 10..60 {
            sim.step(i * 200, &[], &[]);
        }
        assert!(sim.names().is_empty());
    }

    #[test]
    fn animal_pausing_for_three_seconds_stays_one_event() {
        let mut sim = Sim::new();
        let deer = |x: f32| Detection {
            label: Label::Animal,
            raw_class: "animal".into(),
            score: 0.8,
            bbox: b(x),
        };
        for i in 0..10 {
            sim.step(i * 200, &[b(0.2)], &[deer(0.2)]);
        }
        // Stands still, the detector misses it for 3 s.
        for i in 10..25 {
            sim.step(i * 200, &[], &[]);
        }
        for i in 25..35 {
            sim.step(i * 200, &[b(0.2)], &[deer(0.2)]);
        }
        for i in 35..70 {
            sim.step(i * 200, &[], &[]);
        }
        assert_eq!(sim.names(), vec!["start animal", "end track"]);
    }

    #[test]
    fn labels_not_configured_for_the_camera_make_no_events() {
        let mut sim = Sim::new();
        sim.events = EventManager::new("cam".into(), &[Label::Animal], &MotionConfig::default());
        for i in 0..30 {
            sim.step(i * 200, &[b(0.1)], &[person(0.1)]);
        }
        for i in 30..70 {
            sim.step(i * 200, &[b(0.5)], &[]);
        }
        assert!(sim.names().is_empty());
    }
}
