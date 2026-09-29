//! `zoologist reclassify`: names the animals of stored events again, from their clips. For
//! events stored while the species classifier was not running (a missing model file), or to try
//! a newer model on old events. Each clip goes through the camera's analysis again, and the best
//! views of the animal go to the species classifier, as for a live event. Without `yes` the
//! answers are only printed.
//!
//! When Zoologist is running (its database is in use), the command asks the running server to do
//! the work, one event at a time: `POST /api/v1/events/{id}/reclassify`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use zoologist_core::{Config, Label, SpeciesGuess};
use zoologist_store::{EventPatch, EventRecord, MAX_PAGE, Store};
use zoologist_video::mp4r::{read_mp4_index, read_samples};
use zoologist_video::stream::Codec;
use zoologist_vision::events::EventUpdate;
use zoologist_vision::pool::DetectorHandle;
use zoologist_vision::species::{
    SpeciesAnswer, SpeciesCrop, SpeciesHandle, SpeciesJob, settle_still_animal,
};
use zoologist_vision::tracker::BestCrop;

use crate::analysis::{AnalysisOptions, analyse_recording};
use crate::hub_import::merge_recording_events;
use crate::pipeline::{load_detector, load_species};

/// Seconds of whole-picture detection at the start of each clip.
const TILE_SECONDS: i64 = 2;
/// How long one event may take on a busy server.
const SERVER_TIMEOUT: Duration = Duration::from_secs(600);

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

/// What classifying one event again found.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The animal was named.
    Named { species: SpeciesGuess },
    /// The classifier is sure it is a person or a vehicle, not an animal.
    NotAnimal { label: Label },
    /// It never moved and could not be named, or a second look finds no animal at all: most
    /// likely a stump or a shadow (`species.still_unnamed_as_motion`). Stored as motion.
    StillUnnamed,
    /// No confident answer (it moved, so it is kept as an animal).
    Unknown,
    /// The detector finds no animal in the clip (`still_unnamed_as_motion` is off).
    NoAnimal,
}

impl Outcome {
    /// The change to store, if any.
    pub fn patch(&self) -> Option<EventPatch> {
        match self {
            Outcome::Named { species } => Some(EventPatch {
                species: Some(species.clone()),
                ..Default::default()
            }),
            Outcome::NotAnimal { label } => Some(EventPatch {
                label: Some(*label),
                ..Default::default()
            }),
            Outcome::StillUnnamed => Some(EventPatch {
                label: Some(Label::Motion),
                ..Default::default()
            }),
            Outcome::Unknown | Outcome::NoAnimal => None,
        }
    }

    fn describe(&self) -> String {
        match self {
            Outcome::Named { species } => format!(
                "{} ({}) {:.0} %",
                species.common_name,
                species.scientific_name,
                species.score * 100.0
            ),
            Outcome::NotAnimal { label } => format!("not an animal, a {label}"),
            Outcome::StillUnnamed => "never moved and could not be named: motion".into(),
            Outcome::Unknown => "no confident answer".into(),
            Outcome::NoAnimal => "no animal found in the clip".into(),
        }
    }
}

/// Classifies one stored animal event again. `background` puts its detector work behind live
/// cameras (used inside the running server).
pub fn reclassify_one(
    config: &Arc<Config>,
    detector: &DetectorHandle,
    species: &SpeciesHandle,
    e: &EventRecord,
    background: bool,
) -> Result<Outcome> {
    let still_as_motion = config.species.still_unnamed_as_motion;
    let Some((crops, moved)) = animal_crops(config, detector, e, background)? else {
        return Ok(if still_as_motion {
            Outcome::StillUnnamed
        } else {
            Outcome::NoAnimal
        });
    };
    let answer = settle_still_animal(
        classify(species, crops, e.top_score, Default::default()),
        moved,
        still_as_motion,
    );
    Ok(match answer {
        SpeciesAnswer::Species(guess) => Outcome::Named { species: guess },
        SpeciesAnswer::NotAnimal(Label::Motion) if !moved => Outcome::StillUnnamed,
        SpeciesAnswer::NotAnimal(label) => Outcome::NotAnimal { label },
        SpeciesAnswer::Unknown => Outcome::Unknown,
    })
}

/// Tally of a run, printed at the end.
#[derive(Default)]
struct Tally {
    named: usize,
    relabelled: usize,
    unknown: usize,
    skipped: usize,
}

impl Tally {
    fn add(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Named { .. } => self.named += 1,
            Outcome::NotAnimal { .. } | Outcome::StillUnnamed => self.relabelled += 1,
            Outcome::Unknown => self.unknown += 1,
            Outcome::NoAnimal => self.skipped += 1,
        }
    }

    fn print(&self, yes: bool) {
        let verb = if yes { "stored" } else { "found, NOT stored" };
        println!(
            "\n{verb}: {} named, {} relabelled; {} without a confident answer, {} skipped",
            self.named, self.relabelled, self.unknown, self.skipped
        );
        if !yes && self.named + self.relabelled > 0 {
            println!("nothing was changed: run again with --yes to store these answers");
        }
    }
}

fn describe_event(e: &EventRecord) -> String {
    format!(
        "event {} ({}, {})",
        e.id,
        e.camera_id,
        e.started_at.format("%Y-%m-%d %H:%M")
    )
}

/// `server`: the running Zoologist's address, used when the database is in use (default: this
/// machine, at the config's port).
pub fn reclassify(
    config: Config,
    filter: &ReclassifyFilter,
    yes: bool,
    server: Option<String>,
) -> Result<()> {
    let config = Arc::new(config);
    let dir = &config.server.data_dir;
    match Store::open(&dir.join("zoologist.redb"), config.station.timezone) {
        Ok(store) => reclassify_here(&config, &store, filter, yes),
        Err(open_error) => {
            let server =
                server.unwrap_or_else(|| format!("http://127.0.0.1:{}", config.server.bind.port()));
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(SERVER_TIMEOUT))
                .build()
                .into();
            if agent.get(format!("{server}/api/v1/health")).call().is_err() {
                bail!(
                    "cannot open the database ({open_error}), and no Zoologist answers at \
                     {server}. Run this where Zoologist runs (docker compose exec zoologist \
                     zoologist reclassify ...), or give its address with --server."
                );
            }
            println!("Zoologist is running: asking it at {server}");
            reclassify_on_server(&agent, &server, filter, yes)
        }
    }
}

/// Zoologist is not running: open the database and load the models here.
fn reclassify_here(
    config: &Arc<Config>,
    store: &Store,
    filter: &ReclassifyFilter,
    yes: bool,
) -> Result<()> {
    let chosen: Vec<EventRecord> = store
        .events_between(DateTime::<Utc>::UNIX_EPOCH, None)?
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect();
    if chosen.is_empty() {
        println!("no animal events to classify");
        return Ok(());
    }
    let species = match load_species(config) {
        Ok(Some(handle)) => handle,
        Ok(None) => bail!("the species classifier is turned off ([species] enabled = false)"),
        Err(problem) => bail!("{problem}"),
    };
    let (detector, _threads) = load_detector(config)?;
    println!("{} animal events to classify", chosen.len());
    let mut tally = Tally::default();
    for e in &chosen {
        let outcome = match reclassify_one(config, &detector, &species, e, false) {
            Ok(outcome) => outcome,
            Err(err) => {
                println!("{}: skipped: {err:#}", describe_event(e));
                tally.skipped += 1;
                continue;
            }
        };
        print_outcome(e, &outcome, yes);
        tally.add(&outcome);
        if yes && let Some(patch) = outcome.patch() {
            store.update_event(e.id, &patch)?;
        }
    }
    tally.print(yes);
    Ok(())
}

fn print_outcome(e: &EventRecord, outcome: &Outcome, yes: bool) {
    let stored = if !yes && outcome.patch().is_some() {
        " (not stored)"
    } else {
        ""
    };
    println!("{}: {}{stored}", describe_event(e), outcome.describe());
}

/// Zoologist is running: list the events through its API and let it do the work.
fn reclassify_on_server(
    agent: &ureq::Agent,
    server: &str,
    filter: &ReclassifyFilter,
    yes: bool,
) -> Result<()> {
    #[derive(Deserialize)]
    struct Page {
        items: Vec<EventRecord>,
        next_before_id: Option<u64>,
    }
    let mut chosen = Vec::new();
    let mut before: Option<u64> = None;
    loop {
        let mut url = format!("{server}/api/v1/events?label=animal&limit={MAX_PAGE}");
        if let Some(b) = before {
            url.push_str(&format!("&before_id={b}"));
        }
        let page: Page = agent
            .get(&url)
            .call()
            .and_then(|mut r| r.body_mut().read_json())
            .with_context(|| format!("cannot list events at {url}"))?;
        let done = page.items.is_empty() || page.next_before_id.is_none();
        chosen.extend(page.items.into_iter().filter(|e| filter.matches(e)));
        if done {
            break;
        }
        before = page.next_before_id;
    }
    chosen.reverse(); // oldest first, as when working on the database directly
    if chosen.is_empty() {
        println!("no animal events to classify");
        return Ok(());
    }
    println!("{} animal events to classify", chosen.len());
    let mut tally = Tally::default();
    for e in &chosen {
        let url = format!("{server}/api/v1/events/{}/reclassify", e.id);
        let result = agent
            .post(&url)
            .send_json(serde_json::json!({ "store": yes }))
            .and_then(|mut r| r.body_mut().read_json::<Outcome>());
        match result {
            Ok(outcome) => {
                print_outcome(e, &outcome, yes);
                tally.add(&outcome);
            }
            Err(err) => {
                println!("{}: skipped: {err}", describe_event(e));
                tally.skipped += 1;
            }
        }
    }
    tally.print(yes);
    Ok(())
}

/// The best views of the animal in an event's clip, or `None` if the clip shows none.
fn animal_crops(
    config: &Arc<Config>,
    detector: &DetectorHandle,
    e: &EventRecord,
    background: bool,
) -> Result<Option<(Vec<BestCrop>, bool)>> {
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
        background,
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
    let mut moved = false;
    for u in updates {
        match u {
            EventUpdate::Started { key, label, .. } => {
                labels.insert(key, label);
            }
            EventUpdate::Ended {
                key,
                crops: c,
                moved: m,
                ..
            } if labels.get(&key) == Some(&Label::Animal) => {
                crops.extend(c);
                moved |= m;
            }
            _ => {}
        }
    }
    crops.sort_by(|a, b| b.quality.total_cmp(&a.quality));
    crops.truncate(max);
    Ok((!crops.is_empty()).then_some((crops, moved)))
}

pub(crate) fn classify(
    species: &SpeciesHandle,
    crops: Vec<BestCrop>,
    detector_score: f32,
    check: zoologist_vision::species::Check,
) -> SpeciesAnswer {
    let (tx, rx) = tokio::sync::oneshot::channel();
    species.submit(SpeciesJob {
        check,
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
            feedback: None,
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
