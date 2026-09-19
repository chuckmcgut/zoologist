//! Retention janitor (plan Step 11.1): keeps the data directory within its limits.
//!
//! Each pass, in order:
//! 1. deletes recording segments older than `recording.keep_segments_hours`, except those a
//!    clip that is still being cut may need;
//! 2. purges the clip, snapshot and thumbnail of events older than `retention.clips_days`
//!    (the event itself is kept, with `clip_state = purged`);
//! 3. deletes the oldest clips until they fit in `retention.clips_max_total_mb` (motion events
//!    go before others from the same day; their pictures are kept);
//! 4. removes empty date directories;
//! 5. warns when the file system has less than 5 % free.
//!
//! With `dry_run`, nothing is changed; the report lists what would be done.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use zoologist_core::{Config, Label};
use zoologist_store::{ClipState, EventPatch, EventRecord, Store};

use crate::app::AppState;

/// Time between passes.
const INTERVAL: Duration = Duration::from_secs(600);
/// The first pass runs this long after startup.
const FIRST_PASS: Duration = Duration::from_secs(30);
/// Warn below this fraction of free space.
const LOW_SPACE: f64 = 0.05;
/// Media directories that are organised by date and can be pruned when empty.
const MEDIA_DIRS: [&str; 4] = ["recordings", "clips", "snapshots", "thumbs"];

/// What one pass did (or, in a dry run, would do).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JanitorReport {
    pub segments_deleted: usize,
    pub events_purged: usize,
    pub clips_trimmed: usize,
    pub dirs_removed: usize,
    pub bytes_freed: u64,
    /// One line per file or record, e.g. `delete segment recordings/yard/20260919/….mp4`.
    pub actions: Vec<String>,
}

impl JanitorReport {
    fn is_empty(&self) -> bool {
        self.segments_deleted + self.events_purged + self.clips_trimmed + self.dirs_removed == 0
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Deletes `rel` under `data_dir` (a missing file is fine) and counts its size.
fn remove(data_dir: &Path, rel: &str, dry_run: bool, report: &mut JanitorReport, why: &str) {
    let path = data_dir.join(rel);
    let len = file_len(&path);
    report.actions.push(format!("{why}: delete {rel}"));
    report.bytes_freed += len;
    if !dry_run
        && let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("janitor: cannot delete {}: {e}", path.display());
    }
}

/// Runs one pass. Blocking: call from a blocking thread.
pub fn run_once(
    store: &Store,
    config: &Config,
    data_dir: &Path,
    now: DateTime<Utc>,
    dry_run: bool,
) -> zoologist_store::Result<JanitorReport> {
    let mut report = JanitorReport::default();
    let rec = &config.recording;
    let ms = |s: f32| chrono::Duration::milliseconds((s * 1000.0) as i64);

    // 1. Old segments. A clip that is still pending needs footage from its pre-capture onwards.
    let mut cutoff = now - chrono::Duration::hours(i64::from(rec.keep_segments_hours));
    let recent_window = now - chrono::Duration::hours(i64::from(rec.keep_segments_hours) + 24);
    let pending_start = store
        .events_between(recent_window, None)?
        .iter()
        .filter(|e| e.clip_state == ClipState::Pending)
        .map(|e| e.started_at - ms(rec.pre_capture_seconds))
        .min();
    if let Some(start) = pending_start
        && start < cutoff
    {
        cutoff = start;
    }
    for (camera, segment) in store.segments_before(cutoff)? {
        // Only segments that also *ended* before the cutoff.
        if segment.ended_at >= cutoff {
            continue;
        }
        remove(data_dir, &segment.path, dry_run, &mut report, "old segment");
        remove(
            data_dir,
            &segment.index_path,
            dry_run,
            &mut report,
            "old segment",
        );
        if !dry_run {
            store.delete_segment(&camera, segment.started_at)?;
        }
        report.segments_deleted += 1;
    }

    // 2. Events past clips_days lose their media.
    let clips_cutoff = now - chrono::Duration::days(i64::from(config.retention.clips_days));
    let epoch = DateTime::<Utc>::UNIX_EPOCH;
    for e in store.events_between(epoch, Some(clips_cutoff))? {
        let paths: Vec<&String> = [&e.clip_path, &e.snapshot_path, &e.thumb_path]
            .into_iter()
            .flatten()
            .collect();
        if paths.is_empty() && e.clip_state == ClipState::Purged {
            continue;
        }
        for rel in &paths {
            remove(
                data_dir,
                rel,
                dry_run,
                &mut report,
                &format!("event {} expired", e.id),
            );
        }
        if !dry_run {
            let patch = EventPatch {
                clip_state: Some(ClipState::Purged),
                clip_path: Some(None),
                clip_bytes: Some(None),
                snapshot_path: Some(None),
                thumb_path: Some(None),
                ..Default::default()
            };
            store.update_event(e.id, &patch)?;
        }
        report.events_purged += 1;
    }

    // 3. Total clip size.
    let cap = config
        .retention
        .clips_max_total_mb
        .saturating_mul(1024 * 1024);
    let mut with_clips: Vec<EventRecord> = store
        .events_between(clips_cutoff, None)?
        .into_iter()
        .filter(|e| e.clip_path.is_some())
        .collect();
    let mut total: u64 = with_clips
        .iter()
        .map(|e| {
            e.clip_bytes.unwrap_or_else(|| {
                file_len(&data_dir.join(e.clip_path.as_deref().unwrap_or_default()))
            })
        })
        .sum();
    if total > cap {
        // Oldest day first; within a day, motion events first; then by time.
        with_clips.sort_by_key(|e| (e.local_date, e.label != Label::Motion, e.started_at));
        for e in with_clips {
            if total <= cap {
                break;
            }
            let rel = e.clip_path.clone().unwrap_or_default();
            let bytes = e
                .clip_bytes
                .unwrap_or_else(|| file_len(&data_dir.join(&rel)));
            remove(
                data_dir,
                &rel,
                dry_run,
                &mut report,
                &format!("clips over {} MB", config.retention.clips_max_total_mb),
            );
            if !dry_run {
                let patch = EventPatch {
                    clip_state: Some(ClipState::Purged),
                    clip_path: Some(None),
                    clip_bytes: Some(None),
                    ..Default::default()
                };
                store.update_event(e.id, &patch)?;
            }
            total = total.saturating_sub(bytes);
            report.clips_trimmed += 1;
        }
    }

    // 4. Empty directories (never the top-level media directories themselves).
    for top in MEDIA_DIRS {
        remove_empty_dirs(&data_dir.join(top), true, dry_run, data_dir, &mut report);
    }

    // 5. Free space.
    if let Some(fraction) = crate::disk::free_fraction(data_dir)
        && fraction < LOW_SPACE
    {
        tracing::warn!(
            "only {:.1} % of the disk holding {} is free; lower retention.clips_max_total_mb or recording.keep_segments_hours",
            fraction * 100.0,
            data_dir.display()
        );
    }
    Ok(report)
}

/// Removes empty directories below `dir` (and `dir` itself unless `keep`). Returns whether `dir`
/// is (or, in a dry run, would be) empty and gone.
fn remove_empty_dirs(
    dir: &Path,
    keep: bool,
    dry_run: bool,
    data_dir: &Path,
    report: &mut JanitorReport,
) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut empty = true;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !(is_dir && remove_empty_dirs(&path, false, dry_run, data_dir, report)) {
            // A file, or a directory that stays. In a dry run, files about to be deleted
            // still count, so the report may list fewer directories than a real pass removes.
            empty = false;
        }
    }
    if !empty || keep {
        return false;
    }
    let rel = dir
        .strip_prefix(data_dir)
        .unwrap_or(dir)
        .display()
        .to_string();
    report
        .actions
        .push(format!("empty directory: remove {rel}"));
    report.dirs_removed += 1;
    if !dry_run && let Err(e) = std::fs::remove_dir(dir) {
        tracing::warn!("janitor: cannot remove {}: {e}", dir.display());
        return false;
    }
    true
}

/// Runs a pass shortly after startup and then every [`INTERVAL`] until cancelled.
pub async fn janitor_forever(app: AppState, cancel: CancellationToken) {
    let mut wait = FIRST_PASS;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(wait) => {}
        }
        wait = INTERVAL;
        let (config, dir) = (app.config.clone(), app.data_dir.clone());
        let result = app
            .store
            .call(move |s| run_once(s, &config, &dir, Utc::now(), false))
            .await;
        match result {
            Ok(r) if r.is_empty() => tracing::debug!("janitor: nothing to do"),
            Ok(r) => tracing::info!(
                segments = r.segments_deleted,
                expired_events = r.events_purged,
                trimmed_clips = r.clips_trimmed,
                dirs = r.dirs_removed,
                freed_mb = r.bytes_freed / (1024 * 1024),
                "janitor pass"
            ),
            Err(e) => tracing::warn!("janitor failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests;
