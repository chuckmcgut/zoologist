//! Species classification with SpeciesNet (plan Phase 8).
//!
//! The classifier looks at crops of an animal and gives a probability for each of ~2,500 labels
//! (species, but also genera, families, "blank", "human", "vehicle"). The probabilities of a
//! visit's best crops are averaged (weighted by crop quality), then SpeciesNet's own rules decide
//! what to report:
//!
//! 1. top label above 0.8, or above `min_score` while the detector is fairly sure it saw an
//!    animal → that label, unless the geofence says it does not live here, in which case the
//!    answer is rolled up (family, order, …) until the combined probability beats the top score;
//! 2. otherwise roll up to genus, family, order, class, and report the first group whose
//!    combined probability exceeds `min_score`;
//! 3. otherwise: an unidentified animal.
//!
//! This mirrors `speciesnet/ensemble_prediction_combiner.py` and `geofence_utils.py`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tract_onnx::prelude::*;
use zoologist_core::config::{SpeciesConfig, StationConfig};
use zoologist_core::yuv::{PixelRect, RgbCropper};
use zoologist_core::{BBox, Frame, Label, SpeciesGuess};

use crate::detector::DetectorError;

/// Input side of the SpeciesNet classifier.
pub const INPUT_SIZE: u32 = 480;
/// A top label above this is always accepted (SpeciesNet threshold 4a).
const CERTAIN: f32 = 0.8;
/// Minimum detector score for the lower `min_score` threshold to apply (threshold 4b).
const DETECTOR_AGREES: f32 = 0.2;
/// "blank" (an empty scene) above this means the detector found something that is not there.
const NOTHING_THERE: f32 = 0.9;
/// A *person* is overruled only when the classifier is surer still, and sees no human at all:
/// on the owner's cameras real people scored up to 0.88 "blank" (a partial or blurred view), and
/// insects in the infrared light 0.69 to 0.975.
const PERSON_NOTHING_THERE: f32 = 0.95;
const PERSON_NO_HUMAN: f32 = 0.02;
/// Model id stored with each result.
pub const MODEL_ID: &str = "speciesnet-4.0.3a";

/// One SpeciesNet label: `uuid;class;order;family;genus;species;common name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeciesLabel {
    pub uuid: String,
    pub class: String,
    pub order: String,
    pub family: String,
    pub genus: String,
    pub species: String,
    pub common: String,
}

/// Taxonomy levels used for roll-ups, most specific first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Genus,
    Family,
    Order,
    Class,
}

impl SpeciesLabel {
    /// Parses a label line; `None` if it does not have 7 `;`-separated parts.
    pub fn parse(line: &str) -> Option<Self> {
        let p: Vec<&str> = line.trim().split(';').collect();
        let [uuid, class, order, family, genus, species, common] = p.as_slice() else {
            return None;
        };
        Some(Self {
            uuid: uuid.to_string(),
            class: class.to_string(),
            order: order.to_string(),
            family: family.to_string(),
            genus: genus.to_string(),
            species: species.to_string(),
            common: common.to_string(),
        })
    }

    /// `class;order;family;genus;species`, the key of the taxonomy and geofence maps.
    pub fn full_class(&self) -> String {
        [
            &self.class,
            &self.order,
            &self.family,
            &self.genus,
            &self.species,
        ]
        .map(String::as_str)
        .join(";")
    }

    /// `true` for labels that are not an animal: blank, human, vehicle, "no cv result", and the
    /// generic "animal" label.
    pub fn is_non_animal(&self) -> bool {
        self.class.is_empty() || self.common == "human" || self.class == "no cv result"
    }

    /// The key of this label's ancestor at `level`, or `None` if the label is not that specific.
    fn ancestor_key(&self, level: Level) -> Option<String> {
        let parts = [&self.class, &self.order, &self.family, &self.genus];
        let keep = match level {
            Level::Genus => 4,
            Level::Family => 3,
            Level::Order => 2,
            Level::Class => 1,
        };
        if parts[..keep].iter().any(|p| p.is_empty()) {
            return None;
        }
        let mut out: Vec<&str> = parts[..keep].iter().map(|s| s.as_str()).collect();
        out.resize(5, "");
        Some(out.join(";"))
    }

    /// Scientific name: "Vulpes vulpes" for a species, "Vulpes" for a genus, "Canidae" for a family.
    pub fn scientific_name(&self) -> String {
        let cap = |s: &str| {
            let mut c = s.chars();
            c.next()
                .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
        };
        if !self.species.is_empty() {
            format!("{} {}", cap(&self.genus), self.species)
        } else {
            [&self.genus, &self.family, &self.order, &self.class]
                .into_iter()
                .find(|s| !s.is_empty())
                .map_or(String::new(), |s| cap(s))
        }
    }

    /// Common name with a capital first letter, e.g. "Red fox".
    pub fn display_name(&self) -> String {
        let mut c = self.common.chars();
        c.next()
            .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
    }
}

/// Geofence rules for one taxon.
#[derive(Clone, Debug, Default, PartialEq)]
struct Rule {
    allow: Option<HashMap<String, Vec<String>>>,
    block: Option<HashMap<String, Vec<String>>>,
}

/// Where each taxon may and may not be reported (SpeciesNet's geofence file).
#[derive(Clone, Debug, Default)]
pub struct Geofence {
    rules: HashMap<String, Rule>,
}

impl Geofence {
    /// Parses SpeciesNet's geofence JSON.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let obj = value.as_object().ok_or("geofence file is not an object")?;
        let region_map = |v: Option<&serde_json::Value>| {
            v.and_then(|v| v.as_object()).map(|m| {
                m.iter()
                    .map(|(country, regions)| {
                        let list = regions
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|r| r.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        (country.clone(), list)
                    })
                    .collect()
            })
        };
        let rules = obj
            .iter()
            .map(|(taxon, rule)| {
                (
                    taxon.clone(),
                    Rule {
                        allow: region_map(rule.get("allow")),
                        block: region_map(rule.get("block")),
                    },
                )
            })
            .collect();
        Ok(Self { rules })
    }

    /// `true` if `label` must not be reported in `country` / `admin1`.
    /// Mirrors `should_geofence_animal_classification`.
    pub fn blocks(
        &self,
        label: &SpeciesLabel,
        country: Option<&str>,
        admin1: Option<&str>,
    ) -> bool {
        let Some(country) = country else { return false };
        let Some(rule) = self.rules.get(&label.full_class()) else {
            return false;
        };
        if let Some(allow) = rule.allow.as_ref().filter(|a| !a.is_empty()) {
            match allow.get(country) {
                None => return true,
                Some(regions) => {
                    if let Some(a1) = admin1
                        && !regions.is_empty()
                        && !regions.iter().any(|r| r == a1)
                    {
                        return true;
                    }
                }
            }
        }
        if let Some(regions) = rule.block.as_ref().and_then(|b| b.get(country)) {
            if regions.is_empty() {
                return true;
            }
            if admin1.is_some_and(|a1| regions.iter().any(|r| r == a1)) {
                return true;
            }
        }
        false
    }
}

/// The SpeciesNet taxonomy: every taxon (species, genus, family, …) by its full class string.
#[derive(Clone, Debug, Default)]
pub struct Taxonomy {
    by_class: HashMap<String, SpeciesLabel>,
}

impl Taxonomy {
    pub fn from_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Self {
        Self {
            by_class: lines
                .filter_map(SpeciesLabel::parse)
                .map(|l| (l.full_class(), l))
                .collect(),
        }
    }

    fn ancestor(&self, label: &SpeciesLabel, level: Level) -> Option<&SpeciesLabel> {
        self.by_class.get(&label.ancestor_key(level)?)
    }
}

/// Decides what to report from averaged class probabilities.
pub struct SpeciesRules {
    pub labels: Vec<SpeciesLabel>,
    pub taxonomy: Taxonomy,
    pub geofence: Geofence,
    pub country: Option<String>,
    pub admin1: Option<String>,
    pub min_score: f32,
}

/// What the species classifier concluded about an "animal" event.
#[derive(Clone, Debug, PartialEq)]
pub enum SpeciesAnswer {
    Species(SpeciesGuess),
    /// It is certainly not an animal: the detector mislabelled a person or a vehicle.
    NotAnimal(Label),
    /// No confident answer (or the job was dropped).
    Unknown,
}

/// Applies `species.still_unnamed_as_motion`: an "animal" that never moved and got no confident
/// answer becomes motion. On the owner's Reolink camera, 60 of 60 such night-time "animals" were
/// stumps, dark bushes and reflections; real animals move, or get named.
pub fn settle_still_animal(answer: SpeciesAnswer, moved: bool, enabled: bool) -> SpeciesAnswer {
    match answer {
        SpeciesAnswer::Unknown if enabled && !moved => SpeciesAnswer::NotAnimal(Label::Motion),
        other => other,
    }
}

/// The classifier's second opinion on a *person* event (`species.check_people`): only "nothing
/// there" changes it, to motion. A "human" or "vehicle" answer, or an animal name, leaves the
/// detector's person alone: people are what the detector is best at.
pub fn settle_person(answer: SpeciesAnswer) -> Option<Label> {
    match answer {
        SpeciesAnswer::NotAnimal(Label::Motion) => Some(Label::Motion),
        _ => None,
    }
}

impl SpeciesRules {
    /// What the classifier says the event is, when it is certainly **not** an animal:
    ///
    /// - `Person`/`Vehicle` when "human" and "vehicle" together pass [`CERTAIN`] (a person on a
    ///   quad bike splits its probability between the two, so neither alone is enough);
    /// - `Motion` when "blank" alone passes [`NOTHING_THERE`]: the detector found an animal in
    ///   moving leaves, glare or rain, and the event is kept as plain motion.
    ///
    /// The threshold for "blank" is higher because a real animal in an unusual picture (night
    /// infrared, thermal, heavy blur) can also score as blank.
    pub fn not_an_animal(&self, probs: &[f32]) -> Option<Label> {
        let (mut blank, mut human, mut vehicle) = (0.0, 0.0, 0.0);
        for (i, p) in probs.iter().enumerate() {
            match self.labels.get(i).map(|l| l.common.as_str()) {
                Some("human") => human += p,
                Some("vehicle") => vehicle += p,
                Some("blank") => blank += p,
                _ => {}
            }
        }
        tracing::debug!(
            blank,
            human,
            vehicle,
            "species classifier: not-an-animal classes"
        );
        let _ = vehicle;
        if human + vehicle > CERTAIN {
            return Some(if vehicle > human {
                Label::Vehicle
            } else {
                Label::Person
            });
        }
        (blank > NOTHING_THERE).then_some(Label::Motion)
    }

    /// For a person event: true when the classifier is sure nothing is there (see
    /// [`PERSON_NOTHING_THERE`]).
    pub fn person_not_there(&self, probs: &[f32]) -> bool {
        let share = |name: &str| -> f32 {
            probs
                .iter()
                .enumerate()
                .filter(|(i, _)| self.labels.get(*i).is_some_and(|l| l.common == name))
                .map(|(_, p)| p)
                .sum()
        };
        share("blank") >= PERSON_NOTHING_THERE && share("human") < PERSON_NO_HUMAN
    }

    /// The answer for averaged probabilities `probs` (one per label) of an event whose
    /// detector score was `detector_score`. `None` means "unidentified animal".
    pub fn decide(&self, probs: &[f32], detector_score: f32) -> Option<SpeciesGuess> {
        let mut top: Vec<(usize, f32)> = probs.iter().copied().enumerate().collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        top.truncate(5);
        let (first, score) = *top.first()?;
        let candidates: Vec<(String, f32)> = top
            .iter()
            .map(|&(i, p)| (self.labels[i].display_name(), p))
            .collect();
        let guess = |label: &SpeciesLabel, score: f32| SpeciesGuess {
            scientific_name: label.scientific_name(),
            common_name: label.display_name(),
            score,
            model_id: MODEL_ID.into(),
            candidates: candidates.clone(),
        };
        let top_label = &self.labels[first];

        if !top_label.is_non_animal()
            && (score > CERTAIN || (score > self.min_score && detector_score > DETECTOR_AGREES))
        {
            if !self.blocked(top_label) {
                return Some(guess(top_label, score));
            }
            // Not found here: roll up until the group beats the top score.
            let levels = [Level::Family, Level::Order, Level::Class];
            return self
                .roll_up(&top, &levels, score - 1e-6)
                .map(|(label, s)| guess(label, s));
        }
        let levels = [Level::Genus, Level::Family, Level::Order, Level::Class];
        self.roll_up(&top, &levels, self.min_score)
            .map(|(label, s)| guess(label, s))
    }

    fn blocked(&self, label: &SpeciesLabel) -> bool {
        self.geofence
            .blocks(label, self.country.as_deref(), self.admin1.as_deref())
    }

    /// The first level whose best (not geofenced) group has a combined probability above
    /// `threshold`. Mirrors `roll_up_labels_to_first_matching_level` (without the "kingdom"
    /// level: "some animal" is reported as unidentified instead).
    fn roll_up(
        &self,
        top: &[(usize, f32)],
        levels: &[Level],
        threshold: f32,
    ) -> Option<(&SpeciesLabel, f32)> {
        for &level in levels {
            let mut sums: Vec<(&SpeciesLabel, f32)> = Vec::new();
            // Blank, human and vehicle never count towards an animal group.
            for &(i, p) in top.iter().filter(|(i, _)| !self.labels[*i].is_non_animal()) {
                let Some(ancestor) = self.taxonomy.ancestor(&self.labels[i], level) else {
                    continue;
                };
                match sums
                    .iter_mut()
                    .find(|(a, _)| a.full_class() == ancestor.full_class())
                {
                    Some((_, s)) => *s += p,
                    None => sums.push((ancestor, p)),
                }
            }
            let best = sums
                .into_iter()
                .filter(|(a, _)| !self.blocked(a))
                .max_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((label, s)) = best
                && s > threshold
            {
                return Some((label, s));
            }
        }
        None
    }
}

/// The SpeciesNet classifier plus the rules above.
pub struct SpeciesModel {
    plan: Arc<TypedRunnableModel>,
    pub rules: SpeciesRules,
}

impl SpeciesModel {
    /// Loads the model, labels, taxonomy and geofence named in the config.
    pub fn load(cfg: &SpeciesConfig, station: &StationConfig) -> Result<Self, DetectorError> {
        let s = INPUT_SIZE as usize;
        let plan = tract_onnx::onnx()
            .model_for_path(&cfg.path)
            .and_then(|m| m.with_input_fact(0, f32::fact([1, s, s, 3]).into()))
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(|source| DetectorError::Load {
                path: cfg.path.display().to_string(),
                source,
            })?;
        let read = |p: &Path| {
            std::fs::read_to_string(p)
                .map_err(|e| DetectorError::Invalid(format!("{}: {e}", p.display())))
        };
        let labels: Vec<SpeciesLabel> = read(&cfg.labels)?
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                SpeciesLabel::parse(l)
                    .ok_or_else(|| DetectorError::Invalid(format!("bad label line {l:?}")))
            })
            .collect::<Result<_, _>>()?;
        let taxonomy = match &cfg.taxonomy {
            Some(p) => Taxonomy::from_lines(read(p)?.lines()),
            None => Taxonomy::default(),
        };
        let geofence = match &cfg.geofence {
            Some(p) => Geofence::from_json(&read(p)?)
                .map_err(|e| DetectorError::Invalid(format!("{}: {e}", p.display())))?,
            None => Geofence::default(),
        };
        tracing::info!(labels = labels.len(), "species classifier loaded");
        Ok(Self {
            plan,
            rules: SpeciesRules {
                labels,
                taxonomy,
                geofence,
                country: station.country.clone(),
                admin1: station.admin1_region.clone(),
                min_score: cfg.min_score,
            },
        })
    }

    /// Class probabilities (softmax) for a 480×480 RGB crop.
    pub fn classify(&self, rgb: &[u8]) -> Result<Vec<f32>, DetectorError> {
        let s = INPUT_SIZE as usize;
        if rgb.len() != s * s * 3 {
            return Err(DetectorError::Invalid(format!(
                "expected {s}×{s} RGB, got {} bytes",
                rgb.len()
            )));
        }
        let data: Vec<f32> = rgb.iter().map(|&v| f32::from(v) / 255.0).collect();
        let input = Tensor::from_shape(&[1, s, s, 3], &data)?;
        let out = self.plan.run(tvec!(input.into()))?;
        let logits: Vec<f32> = out[0]
            .to_plain_array_view::<f32>()?
            .iter()
            .copied()
            .collect();
        Ok(softmax(&logits))
    }
}

/// Softmax of `logits`.
pub fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::MIN, f32::max);
    let exp: Vec<f32> = logits.iter().map(|&l| (l - max).exp()).collect();
    let sum: f32 = exp.iter().sum();
    exp.into_iter().map(|e| e / sum).collect()
}

/// Averages probability vectors, weighted (e.g. by crop quality). Empty input gives an empty vec.
pub fn vote(probs: &[(Vec<f32>, f32)]) -> Vec<f32> {
    let Some((first, _)) = probs.first() else {
        return Vec::new();
    };
    let total: f32 = probs.iter().map(|(_, w)| w.max(1e-6)).sum();
    let mut out = vec![0f32; first.len()];
    for (p, w) in probs {
        let w = w.max(1e-6) / total;
        for (o, v) in out.iter_mut().zip(p) {
            *o += v * w;
        }
    }
    out
}

/// The 480×480 classifier input for an animal box: the box plus 10 % margin on each side,
/// stretched (SpeciesNet's "always_crop" preprocessing).
pub fn crop_for_species(
    frame: &Frame,
    bbox: &BBox,
    cropper: &mut RgbCropper,
    dst: &mut Vec<u8>,
) -> Result<(), DetectorError> {
    let (x, y, w, h) = bbox.expand(0.2).to_pixels(frame.width, frame.height);
    cropper
        .crop_to(frame, PixelRect { x, y, w, h }, INPUT_SIZE, INPUT_SIZE, dst)
        .map_err(|e| DetectorError::Invalid(e.to_string()))
}

/// A species classifier as the pool sees it. Implemented by [`SpeciesModel`]; tests use fakes.
pub trait SpeciesClassifier: Send + Sync + 'static {
    /// Class probabilities for a 480×480 RGB crop.
    fn classify(&self, rgb: &[u8]) -> Result<Vec<f32>, DetectorError>;
    /// The answer for vote-averaged probabilities.
    fn decide(&self, probs: &[f32], detector_score: f32) -> Option<SpeciesGuess>;
    /// See [`SpeciesRules::not_an_animal`].
    fn not_an_animal(&self, _probs: &[f32]) -> Option<Label> {
        None
    }
    /// See [`SpeciesRules::person_not_there`].
    fn person_not_there(&self, _probs: &[f32]) -> bool {
        false
    }
}

impl SpeciesClassifier for SpeciesModel {
    fn classify(&self, rgb: &[u8]) -> Result<Vec<f32>, DetectorError> {
        SpeciesModel::classify(self, rgb)
    }
    fn decide(&self, probs: &[f32], detector_score: f32) -> Option<SpeciesGuess> {
        self.rules.decide(probs, detector_score)
    }
    fn not_an_animal(&self, probs: &[f32]) -> Option<Label> {
        self.rules.not_an_animal(probs)
    }
    fn person_not_there(&self, probs: &[f32]) -> bool {
        self.rules.person_not_there(probs)
    }
}

/// One crop of an animal: the frame, the box and how good the view is (vote weight).
#[derive(Clone, Debug)]
pub struct SpeciesCrop {
    pub frame: Frame,
    pub bbox: BBox,
    pub quality: f32,
}

/// What a species job is for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Check {
    /// Name an animal (or find that it is a person, a vehicle, or nothing).
    #[default]
    Animal,
    /// A second opinion on a person: only "surely nothing there" is answered, as
    /// `NotAnimal(Motion)`; anything else is `Unknown`.
    Person,
}

/// Work for the species pool: the best crops of one visit.
pub struct SpeciesJob {
    pub check: Check,
    pub crops: Vec<SpeciesCrop>,
    /// The detector's best score for the animal.
    pub detector_score: f32,
    pub reply: tokio::sync::oneshot::Sender<SpeciesAnswer>,
}

/// Handle for queueing species jobs. Cheap to clone.
#[derive(Clone)]
pub struct SpeciesHandle {
    tx: std::sync::mpsc::SyncSender<SpeciesJob>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
}

impl SpeciesHandle {
    /// Queues a job. If the queue is full the job is dropped (answered with `None`) and counted:
    /// species results are nice to have, detection must never wait for them.
    pub fn submit(&self, job: SpeciesJob) {
        use std::sync::atomic::Ordering;
        self.pending.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self.tx.try_send(job) {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            let job = match e {
                std::sync::mpsc::TrySendError::Full(j)
                | std::sync::mpsc::TrySendError::Disconnected(j) => j,
            };
            let _ = job.reply.send(SpeciesAnswer::Unknown);
        }
    }

    /// Jobs waiting or running.
    pub fn queue_depth(&self) -> usize {
        self.pending.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Jobs dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Starts `workers` species threads with a queue of `capacity` jobs. The threads stop when every
/// handle is dropped.
pub fn spawn_species_pool(
    model: Arc<dyn SpeciesClassifier>,
    workers: usize,
    capacity: usize,
) -> std::io::Result<(SpeciesHandle, Vec<std::thread::JoinHandle<()>>)> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<SpeciesJob>(capacity.max(1));
    let rx = Arc::new(std::sync::Mutex::new(rx));
    let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut threads = Vec::new();
    for n in 0..workers.max(1) {
        let (rx, model, pending) = (rx.clone(), model.clone(), pending.clone());
        threads.push(
            std::thread::Builder::new()
                .name(format!("species-{n}"))
                .spawn(move || {
                    let mut cropper = RgbCropper::new();
                    let mut rgb = Vec::new();
                    loop {
                        let job = match rx.lock().unwrap_or_else(|e| e.into_inner()).recv() {
                            Ok(job) => job,
                            Err(_) => return,
                        };
                        let mut votes = Vec::new();
                        for crop in &job.crops {
                            let result =
                                crop_for_species(&crop.frame, &crop.bbox, &mut cropper, &mut rgb)
                                    .and_then(|()| model.classify(&rgb));
                            match result {
                                Ok(p) => votes.push((p, crop.quality)),
                                Err(e) => tracing::warn!("species classification failed: {e}"),
                            }
                        }
                        let answer = if votes.is_empty() {
                            SpeciesAnswer::Unknown
                        } else {
                            let probs = vote(&votes);
                            if job.check == Check::Person {
                                model.not_an_animal(&probs); // logs the shares
                                if model.person_not_there(&probs) {
                                    SpeciesAnswer::NotAnimal(Label::Motion)
                                } else {
                                    SpeciesAnswer::Unknown
                                }
                            } else {
                                match model.not_an_animal(&probs) {
                                    Some(label) => SpeciesAnswer::NotAnimal(label),
                                    None => model
                                        .decide(&probs, job.detector_score)
                                        .map_or(SpeciesAnswer::Unknown, SpeciesAnswer::Species),
                                }
                            }
                        };
                        pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                        let _ = job.reply.send(answer);
                    }
                })?,
        );
    }
    Ok((
        SpeciesHandle {
            tx,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending,
        },
        threads,
    ))
}

#[cfg(test)]
mod tests;
