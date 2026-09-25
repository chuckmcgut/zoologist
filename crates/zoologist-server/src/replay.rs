//! `zoologist replay`: runs saved clips through a camera's analysis (motion, detector, tracker,
//! events) and prints the events each clip produces. Used to measure settings on real footage
//! before changing them: replay the same clips with a setting off and on, compare.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use zoologist_core::Config;
use zoologist_video::mp4r::{read_mp4_index, read_samples};
use zoologist_video::stream::Codec;
use zoologist_vision::events::{EventKey, EventUpdate};

use crate::analysis::{AnalysisOptions, analyse_recording};
use crate::pipeline::load_detector;

/// One event found in a clip.
struct Found {
    label: String,
    start_s: f64,
    duration_s: Option<f64>,
    score: f32,
}

pub fn replay(
    mut config: Config,
    camera_id: &str,
    clips: &[PathBuf],
    min_movement: Option<f32>,
) -> Result<()> {
    if let Some(m) = min_movement {
        config.tracking.min_movement = m;
    }
    let camera = config
        .cameras
        .iter()
        .find(|c| c.id == camera_id)
        .with_context(|| format!("no camera {camera_id:?} in the config"))?
        .clone();
    if clips.is_empty() {
        bail!("give one or more clip files (.mp4)");
    }
    println!(
        "camera {camera_id}: tracking.min_movement = {} for {:?}, stationary_seconds = {}",
        config.tracking.min_movement,
        config.tracking.require_movement,
        config.tracking.stationary_seconds
    );
    let config = Arc::new(config);
    let (detector, _threads) = load_detector(&config)?;
    let mut totals: HashMap<String, usize> = HashMap::new();
    for clip in clips {
        let name = clip.file_name().map_or_else(
            || clip.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let (info, entries) = match read_mp4_index(clip) {
            Ok(x) => x,
            Err(e) => {
                println!("{name}: cannot read: {e}");
                continue;
            }
        };
        if info.codec != Codec::H264 {
            println!("{name}: {:?}, skipped", info.codec);
            continue;
        }
        let samples = read_samples(clip, &entries)?;
        let start = Utc::now();
        let options = AnalysisOptions {
            background: false,
            tiles_until: None,
        };
        let (updates, _) =
            analyse_recording(&config, &camera, &detector, info, samples, start, options)?;
        let mut open: HashMap<EventKey, usize> = HashMap::new();
        let mut found: Vec<Found> = Vec::new();
        for update in updates {
            match update {
                EventUpdate::Started {
                    key,
                    label,
                    started_at,
                    score,
                    ..
                } => {
                    open.insert(key, found.len());
                    found.push(Found {
                        label: label.to_string(),
                        start_s: (started_at - start).num_milliseconds() as f64 / 1000.0,
                        duration_s: None,
                        score,
                    });
                }
                EventUpdate::Ended {
                    key,
                    ended_at,
                    top_score,
                    ..
                } => {
                    if let Some(&i) = open.get(&key) {
                        let f = &mut found[i];
                        f.duration_s =
                            Some((ended_at - start).num_milliseconds() as f64 / 1000.0 - f.start_s);
                        f.score = f.score.max(top_score);
                    }
                }
                EventUpdate::Updated { .. } => {}
            }
        }
        for f in &found {
            *totals.entry(f.label.clone()).or_default() += 1;
        }
        let list: Vec<String> = found
            .iter()
            .map(|f| {
                format!(
                    "{} at {:.1}s for {} (score {:.2})",
                    f.label,
                    f.start_s,
                    f.duration_s.map_or("?".into(), |d| format!("{d:.1}s")),
                    f.score
                )
            })
            .collect();
        println!(
            "{name}: {}",
            if list.is_empty() {
                "no events".into()
            } else {
                list.join("; ")
            }
        );
    }
    let mut totals: Vec<_> = totals.into_iter().collect();
    totals.sort();
    println!("\ntotal over {} clips: {totals:?}", clips.len());
    Ok(())
}
