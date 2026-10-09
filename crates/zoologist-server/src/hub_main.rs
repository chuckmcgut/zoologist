//! The full-resolution recording of a battery camera's event, fetched from the Hub on demand.
//!
//! Zoologist keeps the small H.264 sub recording as an event's clip. The Hub also holds the
//! main recording of the same moment (H.265, 5120×1440 on the owner's panorama camera), until
//! its storage wraps around. When someone asks for it, it is downloaded to `tmp/main/<id>.mp4`
//! and served from there (browsers ask for byte ranges, which a pass-through of the Hub's
//! answer could not give). The copies are removed again after [`KEEP`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use zoologist_core::config::CameraKind;
use zoologist_store::EventRecord;
use zoologist_video::reolink_hub::{HubClient, HubFile};

use crate::app::AppState;

/// How long a fetched recording is kept.
const KEEP: Duration = Duration::from_secs(3600);
/// Longer recordings are not fetched: the Hub sends them at about real-time speed.
const MAX_SECONDS: i64 = 600;
/// How far before the event its recording may have started.
const LOOK_BACK_SECONDS: i64 = 300;
/// Clock slack between the event's start and the recording's times.
const SLACK_SECONDS: i64 = 2;

/// Why there is no main recording to show.
#[derive(Debug)]
pub enum MainError {
    /// This event can never have one (not a battery camera behind a Hub).
    NotOnHub(&'static str),
    /// The Hub has no recording of that moment (any more).
    Gone,
    Hub(String),
}

impl std::fmt::Display for MainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MainError::NotOnHub(why) => f.write_str(why),
            MainError::Gone => {
                f.write_str("the Hub has no full-resolution recording of this event (any more)")
            }
            MainError::Hub(e) => write!(f, "cannot fetch the recording from the Hub: {e}"),
        }
    }
}

/// One fetch at a time: a browser asks for the same video several times at once, and the Hub
/// is slow.
static FETCHING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The main recording that contains `at`: the latest one that started before it.
pub fn recording_of(files: Vec<HubFile>, at: DateTime<Utc>) -> Option<HubFile> {
    let slack = chrono::Duration::seconds(SLACK_SECONDS);
    files
        .into_iter()
        .filter(|f| f.start <= at + slack && f.end + slack >= at)
        .max_by_key(|f| f.start)
}

/// Removes fetched recordings older than [`KEEP`], and downloads that were cut short.
fn forget_old(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > KEEP));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Where the fetched main recording of event `id` is kept.
pub fn cached(data_dir: &Path, id: u64) -> PathBuf {
    data_dir.join("tmp").join("main").join(format!("{id}.mp4"))
}

/// The local copy of the event's main recording, downloading it from the Hub first if needed.
pub async fn fetch(app: &AppState, record: &EventRecord) -> Result<PathBuf, MainError> {
    let camera = app
        .config
        .cameras
        .iter()
        .find(|c| c.id == record.camera_id)
        .filter(|c| c.kind == CameraKind::HubClips)
        .ok_or(MainError::NotOnHub(
            "only events of battery cameras behind a Hub have a full-resolution recording",
        ))?;
    let hub = camera
        .hub
        .as_ref()
        .and_then(|id| app.config.reolink_hubs.iter().find(|h| &h.id == id))
        .ok_or(MainError::NotOnHub("the camera's Hub is not in the config"))?;
    let channel = camera
        .channel
        .ok_or(MainError::NotOnHub("the camera has no Hub channel"))?;

    let dest = cached(&app.data_dir, record.id);
    let dir = dest.parent().unwrap_or(&app.data_dir).to_path_buf();
    let _one_at_a_time = FETCHING.lock().await;
    if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        return Ok(dest);
    }
    let (url, user, password) = (hub.url.clone(), hub.user.clone(), hub.password.clone());
    let (tz, at, id) = (app.config.station.timezone, record.started_at, record.id);
    let target = dest.clone();
    tokio::task::spawn_blocking(move || {
        forget_old(&dir, SystemTime::now());
        let mut client = HubClient::new(&url, &user, &password);
        let from = at - chrono::Duration::seconds(LOOK_BACK_SECONDS);
        let to = at + chrono::Duration::seconds(SLACK_SECONDS);
        let files = client
            .search(channel, "main", from, to, tz)
            .map_err(|e| MainError::Hub(e.to_string()))?;
        let file = recording_of(files, at).ok_or(MainError::Gone)?;
        if (file.end - file.start).num_seconds() > MAX_SECONDS {
            return Err(MainError::Hub(format!(
                "the recording is {} s long",
                (file.end - file.start).num_seconds()
            )));
        }
        let started = std::time::Instant::now();
        let bytes = client
            .download(&file, &target)
            .map_err(|e| MainError::Hub(e.to_string()))?;
        tracing::info!(
            event = id,
            file = %file.name,
            mb = format!("{:.1}", bytes as f64 / 1_048_576.0),
            secs = format!("{:.1}", started.elapsed().as_secs_f32()),
            "fetched the main recording from the Hub"
        );
        Ok(target)
    })
    .await
    .map_err(|e| MainError::Hub(e.to_string()))?
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn file(name: &str, from: i64, to: i64) -> HubFile {
        let t = |s: i64| {
            Utc.with_ymd_and_hms(2026, 10, 9, 11, 50, 0).unwrap() + chrono::Duration::seconds(s)
        };
        HubFile {
            name: name.into(),
            start: t(from),
            end: t(to),
            size: 0,
            stream: "main".into(),
            width: 0,
            height: 0,
        }
    }

    #[test]
    fn the_recording_that_contains_the_event_is_chosen() {
        let at = |s: i64| {
            Utc.with_ymd_and_hms(2026, 10, 9, 11, 50, 0).unwrap() + chrono::Duration::seconds(s)
        };
        let files = || vec![file("a", -41, -31), file("b", 9, 23), file("c", 91, 107)];
        let name = |s: i64| recording_of(files(), at(s)).map(|f| f.name);
        assert_eq!(
            name(9).as_deref(),
            Some("b"),
            "an event that starts with it"
        );
        assert_eq!(name(20).as_deref(), Some("b"));
        // The Hub's times are whole seconds; an event may seem to start a moment early.
        assert_eq!(name(8).as_deref(), Some("b"));
        assert_eq!(name(60), None, "between two recordings");
        assert_eq!(name(-100), None);
    }

    #[test]
    fn old_copies_are_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.mp4"), b"x").unwrap();
        forget_old(dir.path(), SystemTime::now());
        assert!(dir.path().join("1.mp4").exists(), "fresh");
        forget_old(
            dir.path(),
            SystemTime::now() + KEEP + Duration::from_secs(1),
        );
        assert!(!dir.path().join("1.mp4").exists());
    }
}
