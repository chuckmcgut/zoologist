//! `zoologist reclassify`: names the animals of stored events again, from their clips. For
//! events stored while the species classifier was not running (a missing model file), or to try
//! a newer model on old events. Each clip goes through the camera's analysis again, and the best
//! views of the animal go to the species classifier, as for a live event. Without `yes` the
//! answers are only printed. Run it with Zoologist stopped: the database can only be opened once.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use zoologist_core::{Config, Label};
use zoologist_store::{EventPatch, EventRecord, Store};
use zoologist_video::mp4r::{read_mp4_index, read_samples};
use zoologist_video::stream::Codec;
use zoologist_vision::events::EventUpdate;
use zoologist_vision::species::{SpeciesAnswer, SpeciesCrop, SpeciesHandle, SpeciesJob};
use zoologist_vision::tracker::BestCrop;

use crate::analysis::{AnalysisOptions, analyse_recording};
use crate::hub_import::merge_recording_events;
use crate::pipeline::{load_detector, load_species};

/// Seconds of whole-picture detection at the start of each clip.
const TILE_SECONDS: i64 = 2;

/// Which events to classify again.
pub struct ReclassifyFilter {
    /// Empty: every camera.
    pub cameras: Vec<String>,
    /// Only events from this local (station) date on.
    pub since: Option<NaiveDate>,
    /// Also events that already have a species.
    pub all: bool,
}

impl ReclassifyFilter {
    fn matches(&self, e: &EventRecord) -> bool {
        e.label == Label::Animal
            && e.clip_path.is_some()
            && e.ended_at.is_some()
            && (self.all || e.species.is_none())
            && (self.cameras.is_empty() || self.cameras.contains(&e.camera_id))
            && self.since.is_none_or(|d| e.local_date >= d)
    }
}

pub fn reclassify(config: Config, filter: &ReclassifyFilter, yes: bool) -> Result<()> {
    let config = Arc::new(config);
    let dir = &config.server.data_dir;
    let store = Store::open(&dir.join("zoologist.redb"), config.station.timezone)
        .context("cannot open the database (stop Zoologist first)")?;
    let chosen: Vec<EventRecord> = store
        .events_between(DateTime::<Utc>::UNIX_EPOCH, None)?
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect();
    if chosen.is_empty() {
        println!("no animal events to classify");
        return Ok(());
    }
    let species = match load_species(&config) {
        Ok(Some(handle)) => handle,
        Ok(None) => bail!("the species classifier is turned off ([species] enabled = false)"),
        Err(problem) => bail!("{problem}"),
    };
    let (detector, _threads) = load_detector(&config)?;
    println!("{} animal events to classify", chosen.len());

    let (mut named, mut relabelled, mut unknown, mut skipped) = (0, 0, 0, 0);
    for e in &chosen {
        let what = format!(
            "event {} ({}, {})",
            e.id,
            e.camera_id,
            e.started_at.format("%Y-%m-%d %H:%M")
        );
        let crops = match animal_crops(&config, &detector, e) {
            Ok(Some(crops)) => crops,
            Ok(None) => {
                println!("{what}: no animal found in the clip");
                skipped += 1;
                continue;
            }
            Err(err) => {
                println!("{what}: skipped: {err}");
                skipped += 1;
                continue;
            }
        };
        let patch = match classify(&species, crops, e.top_score) {
            SpeciesAnswer::Species(guess) => {
                println!(
                    "{what}: {} ({}) {:.0} %",
                    guess.common_name,
                    guess.scientific_name,
                    guess.score * 100.0
                );
                named += 1;
                EventPatch {
                    species: Some(guess),
                    ..Default::default()
                }
            }
            SpeciesAnswer::NotAnimal(label) => {
                println!("{what}: not an animal, a {label}");
                relabelled += 1;
                EventPatch {
                    label: Some(label),
                    ..Default::default()
                }
            }
            SpeciesAnswer::Unknown => {
                println!("{what}: no confident answer");
                unknown += 1;
                continue;
            }
        };
        if yes {
            store.update_event(e.id, &patch)?;
        }
    }
    let verb = if yes { "stored" } else { "found (not stored)" };
    println!(
        "\n{verb}: {named} named, {relabelled} relabelled; {unknown} without a confident answer, \
         {skipped} skipped"
    );
    if !yes && named + relabelled > 0 {
        println!("run again with --yes to store them");
    }
    Ok(())
}

/// The best views of the animal in an event's clip, or `None` if the clip shows none.
fn animal_crops(
    config: &Arc<Config>,
    detector: &zoologist_vision::pool::DetectorHandle,
    e: &EventRecord,
) -> Result<Option<Vec<BestCrop>>> {
    let camera = config
        .cameras
        .iter()
        .find(|c| c.id == e.camera_id)
        .with_context(|| format!("camera {:?} is no longer in the config", e.camera_id))?;
    let clip = config
        .server
        .data_dir
        .join(e.clip_path.as_deref().unwrap_or_default());
    let (info, entries) =
        read_mp4_index(&clip).with_context(|| format!("cannot read {}", clip.display()))?;
    if info.codec != Codec::H264 {
        bail!("the clip is {:?}, not H.264", info.codec);
    }
    let samples = read_samples(&clip, &entries)?;
    // As for Hub recordings: the whole picture for the first seconds (an animal may already be
    // sitting still), then where there is motion, as when the event was recorded. The whole
    // picture all the time costs minutes per clip.
    let options = AnalysisOptions {
        background: false,
        tiles_until: Some(e.started_at + chrono::Duration::seconds(TILE_SECONDS)),
    };
    let (updates, _) = analyse_recording(
        config,
        camera,
        detector,
        info,
        samples,
        e.started_at,
        options,
    )?;
    let max = config.species.max_crops_per_event;
    let updates = merge_recording_events(updates, max);
    let mut labels = HashMap::new();
    let mut crops: Vec<BestCrop> = Vec::new();
    for u in updates {
        match u {
            EventUpdate::Started { key, label, .. } => {
                labels.insert(key, label);
            }
            EventUpdate::Ended { key, crops: c, .. }
                if labels.get(&key) == Some(&Label::Animal) =>
            {
                crops.extend(c);
            }
            _ => {}
        }
    }
    crops.sort_by(|a, b| b.quality.total_cmp(&a.quality));
    crops.truncate(max);
    Ok((!crops.is_empty()).then_some(crops))
}

fn classify(species: &SpeciesHandle, crops: Vec<BestCrop>, detector_score: f32) -> SpeciesAnswer {
    let (tx, rx) = tokio::sync::oneshot::channel();
    species.submit(SpeciesJob {
        crops: crops
            .into_iter()
            .map(|c| SpeciesCrop {
                frame: c.frame,
                bbox: c.bbox,
                quality: c.quality,
            })
            .collect(),
        detector_score,
        reply: tx,
    });
    rx.blocking_recv().unwrap_or(SpeciesAnswer::Unknown)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use zoologist_core::SpeciesGuess;
    use zoologist_store::ClipState;

    use super::*;

    fn event(camera: &str, label: Label, day: u32, named: bool) -> EventRecord {
        let started_at = Utc.with_ymd_and_hms(2026, 9, day, 18, 0, 0).unwrap();
        EventRecord {
            id: 1,
            camera_id: camera.into(),
            label,
            raw_class: None,
            started_at,
            ended_at: Some(started_at + chrono::Duration::seconds(20)),
            local_date: started_at.date_naive(),
            local_hour: 18,
            top_score: 0.8,
            median_score: 0.7,
            best_bbox: None,
            species: named.then(|| SpeciesGuess {
                scientific_name: "corvus brachyrhynchos".into(),
                common_name: "American crow".into(),
                score: 0.9,
                model_id: "speciesnet".into(),
                candidates: Vec::new(),
            }),
            snapshot_path: None,
            thumb_path: None,
            clip_path: Some("clips/1.mp4".into()),
            clip_bytes: None,
            clip_state: ClipState::Ready,
        }
    }

    #[test]
    fn picks_unnamed_animals_of_the_chosen_cameras_and_dates() {
        let filter = ReclassifyFilter {
            cameras: vec!["yard".into()],
            since: NaiveDate::from_ymd_opt(2026, 9, 25),
            all: false,
        };
        assert!(filter.matches(&event("yard", Label::Animal, 26, false)));
        assert!(
            !filter.matches(&event("yard", Label::Animal, 26, true)),
            "already named"
        );
        assert!(
            !filter.matches(&event("yard", Label::Person, 26, false)),
            "not an animal"
        );
        assert!(
            !filter.matches(&event("road", Label::Animal, 26, false)),
            "other camera"
        );
        assert!(
            !filter.matches(&event("yard", Label::Animal, 24, false)),
            "too old"
        );
        let mut no_clip = event("yard", Label::Animal, 26, false);
        no_clip.clip_path = None;
        assert!(!filter.matches(&no_clip), "nothing to look at");

        let every = ReclassifyFilter {
            cameras: Vec::new(),
            since: None,
            all: true,
        };
        assert!(every.matches(&event("road", Label::Animal, 1, true)));
    }
}
