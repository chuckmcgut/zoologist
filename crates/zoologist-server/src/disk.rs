//! Disk usage of the data directory, for `/api/v1/health` and the retention janitor.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use crate::app::{AppState, DiskUsage};

/// How often disk usage is measured.
const INTERVAL: Duration = Duration::from_secs(300);

/// Total size of the files under `dir` (0 if it does not exist).
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => dir_size(&entry.path()),
            Ok(t) if t.is_file() => entry.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

/// Free bytes on the file system holding `path` (0 if unknown).
pub fn free_space(path: &Path) -> u64 {
    rustix::fs::statvfs(path).map_or(0, |s| s.f_bavail.saturating_mul(s.f_frsize))
}

/// Fraction (0..1) of the file system holding `path` that is free, if known.
pub fn free_fraction(path: &Path) -> Option<f64> {
    let s = rustix::fs::statvfs(path).ok()?;
    (s.f_blocks > 0).then(|| s.f_bavail as f64 / s.f_blocks as f64)
}

/// Measures the data directory now.
pub fn measure(data_dir: &Path) -> DiskUsage {
    DiskUsage {
        recordings_bytes: dir_size(&data_dir.join("recordings")),
        clips_bytes: dir_size(&data_dir.join("clips")),
        free_bytes: free_space(data_dir),
        measured_at: Some(Utc::now()),
    }
}

/// Measures disk usage now and then every [`INTERVAL`] until cancelled.
pub async fn measure_disk_forever(app: AppState, cancel: CancellationToken) {
    loop {
        let dir = app.data_dir.clone();
        if let Ok(usage) = tokio::task::spawn_blocking(move || measure(&dir)).await {
            *app.disk.write().unwrap_or_else(|e| e.into_inner()) = usage;
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_add_up_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("recordings/cam/20260101");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.mp4"), [0u8; 1000]).unwrap();
        std::fs::write(dir.path().join("recordings/b"), [0u8; 24]).unwrap();
        let usage = measure(dir.path());
        assert_eq!(usage.recordings_bytes, 1024);
        assert_eq!(usage.clips_bytes, 0);
        assert!(usage.free_bytes > 0);
    }
}
