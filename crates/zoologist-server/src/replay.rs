//! `zoologist replay`: runs saved clips through a camera's analysis (motion, detector, tracker,
//! events) and prints the events each clip produces. Used to measure settings on real footage
//! before changing them: replay the same clips with a setting off and on, compare.
//!
//! [`ClipAnalyser`] is shared with `zoologist eval`, which replays the clips of marked events and
//! scores the result.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use zoologist_core::config::{CameraConfig, CameraKind};
use zoologist_core::{BBox, Config, Label};
use zoologist_video::mp4r::{read_mp4_index, read_samples};
use zoologist_video::stream::Codec;
use zoologist_vision::events::{EventKey, EventUpdate};
use zoologist_vision::pool::DetectorHandle;
use zoologist_vision::species::{Check, SpeciesAnswer, SpeciesHandle};

use crate::analysis::{AnalysisOptions, analyse_recording};
use crate::pipeline::load_detector_named;

/// One event found in a clip, with the label it ends up with.
#[derive(Clone, Debug)]
pub struct Found {
    /// The final label, after the species classifier and the marked spots.
    pub label: Label,
    /// The detector's label, before anything changed it.
    pub detected: Label,
    /// The animal's common name.
    pub species: Option<String>,
    /// Why the label changed (or why an animal has no name).
    pub note: Option<&'static str>,
    pub start_s: f64,
    pub duration_s: Option<f64>,
    pub score: f32,
}

impl Found {
    /// A person, vehicle or animal (not motion).
    pub fn is_object(&self) -> bool {
        self.label != Label::Motion
    }
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.species, self.note) {
            (Some(name), _) => write!(f, "{} ({name})", self.label),
            (None, Some(note)) => write!(f, "{} ({note})", self.label),
            (None, None) => write!(f, "{}", self.label),
        }
    }
}

/// What a clip gave.
pub enum ClipResult {
    Events(Vec<Found>),
    /// The clip could not be analysed (unreadable, or not H.264).
    Skipped(String),
}

/// Runs clips through a camera's analysis as the live pipeline and the Hub importer do,
/// including the species classifier's verdicts when it is loaded.
pub struct ClipAnalyser {
    config: Arc<Config>,
    /// By model key, loaded when a camera first needs one.
    detectors: Mutex<HashMap<String, DetectorHandle>>,
    species: Option<SpeciesHandle>,
}

impl ClipAnalyser {
    /// `with_species`: name animals, take the second opinion on people, and apply
    /// `species.still_unnamed_as_motion`, as the live pipeline does.
    pub fn new(config: Config, with_species: bool) -> Result<Self> {
        let species = if with_species {
            match crate::pipeline::load_species(&config) {
                Ok(Some(handle)) => Some(handle),
                Ok(None) => bail!("the species classifier is turned off"),
                Err(problem) => bail!("{problem}"),
            }
        } else {
            None
        };
        Ok(Self {
            config: Arc::new(config),
            detectors: Mutex::default(),
            species,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn has_species(&self) -> bool {
        self.species.is_some()
    }

    /// The detector `camera` uses (a battery camera may have its Hub's own), loaded once.
    fn detector(&self, camera: &CameraConfig) -> Result<DetectorHandle> {
        let key = self.config.detector_for(camera).to_string();
        let mut detectors = self.detectors.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = detectors.get(&key) {
            return Ok(d.clone());
        }
        let (detector, _threads) =
            load_detector_named(&self.config, &key, self.config.inference.workers)?;
        detectors.insert(key, detector.clone());
        Ok(detector)
    }

    /// Analyses one clip of `camera`. `main`: the same recording's main stream, for sharp views
    /// of animals (prints what they changed). `marked_spot`: whether a still person or animal
    /// with this box stands where someone marked "nothing there" (`species.learn_from_marks`).
    /// Several clips can be analysed at once from different threads.
    pub fn analyse(
        &self,
        camera: &CameraConfig,
        clip: &Path,
        main: Option<&Path>,
        marked_spot: &(dyn Fn(&BBox) -> bool + Sync),
    ) -> Result<ClipResult> {
        let config = self.config.clone();
        let detector = self.detector(camera)?;
        let (info, entries) = match read_mp4_index(clip) {
            Ok(x) => x,
            Err(e) => return Ok(ClipResult::Skipped(format!("cannot read: {e}"))),
        };
        if info.codec != Codec::H264 {
            return Ok(ClipResult::Skipped(format!("{:?}, skipped", info.codec)));
        }
        let samples = read_samples(clip, &entries)?;
        let start = Utc::now();
        // Battery-camera recordings are analysed as the Hub importer does: the whole picture
        // for the first seconds, since they start with the subject already in view.
        let hub_clip = camera.kind == CameraKind::HubClips;
        let detector_key = config.detector_for(camera);
        let options = AnalysisOptions {
            background: hub_clip,
            tiles_until: hub_clip
                .then(|| start + chrono::Duration::seconds(crate::hub_import::TILE_SECONDS)),
            model_size: config.models.get(detector_key).map(|m| m.input_size),
        };
        let (updates, _) =
            analyse_recording(&config, camera, &detector, info, samples, start, options)?;
        let still_as_motion = config.species.still_unnamed_as_motion;
        let learn = config.species.learn_from_marks;
        let mut open: HashMap<EventKey, (usize, Option<BBox>)> = HashMap::new();
        let mut found: Vec<Found> = Vec::new();
        for update in updates {
            match update {
                EventUpdate::Started {
                    key,
                    label,
                    started_at,
                    score,
                    bbox,
                    ..
                } => {
                    open.insert(key, (found.len(), bbox));
                    found.push(Found {
                        label,
                        detected: label,
                        species: None,
                        note: None,
                        start_s: (started_at - start).num_milliseconds() as f64 / 1000.0,
                        duration_s: None,
                        score,
                    });
                }
                // The best view so far is the event's box, as in the store.
                EventUpdate::Updated {
                    key,
                    best: Some(best),
                    ..
                } => {
                    if let Some((_, bbox)) = open.get_mut(&key) {
                        *bbox = Some(best.bbox);
                    }
                }
                EventUpdate::Updated { .. } => {}
                EventUpdate::Ended {
                    key,
                    ended_at,
                    top_score,
                    crops,
                    moved,
                    ..
                } => {
                    let Some(&(i, bbox)) = open.get(&key) else {
                        continue;
                    };
                    let f = &mut found[i];
                    f.duration_s =
                        Some((ended_at - start).num_milliseconds() as f64 / 1000.0 - f.start_s);
                    f.score = f.score.max(top_score);
                    // As the writer: a still object in a spot marked "nothing there" is motion,
                    // before the classifier is asked.
                    if matches!(f.label, Label::Person | Label::Animal)
                        && !moved
                        && learn
                        && bbox.is_some_and(|b| marked_spot(&b))
                    {
                        f.label = Label::Motion;
                        f.note = Some("still, in a spot marked as nothing");
                        continue;
                    }
                    let Some(species) = &self.species else {
                        continue;
                    };
                    if f.label == Label::Person && config.species.check_people && !crops.is_empty()
                    {
                        let answer = crate::reclassify::classify(
                            species,
                            crops.clone(),
                            f.score,
                            Check::Person,
                        );
                        if zoologist_vision::species::settle_person(answer).is_some() {
                            f.label = Label::Motion;
                            f.note = Some("person: nothing there");
                        }
                    }
                    let mut crops = crops;
                    if let (Some(main), true) = (main, f.label == Label::Animal) {
                        let extra = sharp_views(&config, &detector, main, &crops, start);
                        let sub_only = crate::reclassify::classify(
                            species,
                            crops.clone(),
                            f.score,
                            Check::Animal,
                        );
                        println!(
                            "  sub stream only: {sub_only:?}\n  sharp views found: {}",
                            extra.len()
                        );
                        crops.extend(extra);
                    }
                    if f.label == Label::Animal && !crops.is_empty() {
                        let answer =
                            crate::reclassify::classify(species, crops, f.score, Check::Animal);
                        match zoologist_vision::species::settle_still_animal(
                            answer,
                            moved,
                            still_as_motion,
                        ) {
                            SpeciesAnswer::Species(g) => f.species = Some(g.common_name),
                            SpeciesAnswer::NotAnimal(Label::Motion) if !moved => {
                                f.label = Label::Motion;
                                f.note = Some("still, unnamed animal");
                            }
                            SpeciesAnswer::NotAnimal(l) => {
                                f.label = l;
                                f.note = Some("not an animal");
                            }
                            SpeciesAnswer::Unknown => f.note = Some("unnamed, moved"),
                        }
                    }
                }
            }
        }
        Ok(ClipResult::Events(found))
    }
}

/// As the Hub importer: sharp views of an animal from the main recording.
fn sharp_views(
    config: &Config,
    detector: &DetectorHandle,
    main: &Path,
    crops: &[zoologist_vision::tracker::BestCrop],
    t0: chrono::DateTime<Utc>,
) -> Vec<zoologist_vision::tracker::BestCrop> {
    let mut extra = Vec::new();
    for crop in crops {
        let offset = (crop.frame.captured_at - t0).num_milliseconds() as f64 / 1000.0;
        let located = crate::snapshots::ffmpeg_frame(&config.video.ffmpeg_path, main, offset)
            .and_then(|jpeg| {
                crate::snapshots::snap_from_jpeg(&jpeg, crop.bbox, "replay", crop.frame.captured_at)
            })
            .and_then(|snap| crate::snapshots::locate_blocking(detector, snap));
        if let Some(c) = located {
            extra.push(zoologist_vision::tracker::BestCrop {
                frame: c.frame,
                bbox: c.bbox,
                score: crop.score,
                quality: c.quality,
                sharp: true,
            });
        }
    }
    extra
}

pub fn replay(
    mut config: Config,
    camera_id: &str,
    clips: &[PathBuf],
    min_movement: Option<f32>,
    with_species: bool,
    main: Option<PathBuf>,
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
        config.tracking_for(&camera).require_movement,
        config.tracking.stationary_seconds
    );
    let analyser = ClipAnalyser::new(config, with_species)?;
    let mut totals: HashMap<String, usize> = HashMap::new();
    for clip in clips {
        let name = clip.file_name().map_or_else(
            || clip.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        // Replay knows no marks: every spot is new.
        let found = match analyser.analyse(&camera, clip, main.as_deref(), &|_| false)? {
            ClipResult::Events(found) => found,
            ClipResult::Skipped(why) => {
                println!("{name}: {why}");
                continue;
            }
        };
        let described: Vec<String> = found.iter().map(ToString::to_string).collect();
        for d in &described {
            *totals.entry(d.clone()).or_default() += 1;
        }
        let list: Vec<String> = found
            .iter()
            .zip(&described)
            .map(|(f, d)| {
                format!(
                    "{d} at {:.1}s for {} (score {:.2})",
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
