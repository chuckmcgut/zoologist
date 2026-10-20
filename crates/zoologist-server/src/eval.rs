//! `zoologist corpus` and `zoologist eval`: the events someone marked as wrong, kept as test
//! cases, and a scorecard of how the current settings do on them.
//!
//! `corpus` copies events from a running Zoologist over its HTTP API (it never opens the
//! database): every marked event, plus a sample of unmarked ones as a check that what was right
//! stays right. Each event is `<id>.json` (as the API gives it) and `<id>.mp4` (its clip).
//! Running it again adds new marks and refreshes changed ones; events already there stay.
//!
//! `eval` replays each clip with the config given (see [`crate::replay::ClipAnalyser`]) and
//! compares what it finds with what should be there:
//!
//! - marked "nothing there": no person, vehicle or animal;
//! - marked as something else: that label (and species, when given), and not the wrong one;
//! - unmarked: what Zoologist said at the time.
//!
//! The marked "nothing" spots are used as the live pipeline uses them
//! (`species.learn_from_marks`), except that an event never matches its own mark.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zoologist_core::{Config, Label};
use zoologist_store::{ClipState, EventRecord, MAX_PAGE};

use crate::replay::{ClipAnalyser, ClipResult, Found};

/// Boxes overlapping this much are the same spot (as in the writer).
const SAME_SPOT_IOU: f32 = 0.5;
/// Unmarked events must be at least this old: the owner had time to mark them.
const SETTLED: chrono::Duration = chrono::Duration::days(1);

// ---------------------------------------------------------------------------------------------
// corpus
// ---------------------------------------------------------------------------------------------

/// What `zoologist corpus` added.
#[derive(Debug, Default)]
pub struct ExportReport {
    pub marked_new: usize,
    pub refreshed: usize,
    pub unmarked_new: usize,
    pub no_clip: usize,
}

/// Copies the marked events (and up to `unmarked` unmarked ones in all) from the Zoologist at
/// `server` into `out`.
pub fn export(server: &str, out: &Path, unmarked: usize, scan: usize) -> Result<ExportReport> {
    let server = server.trim_end_matches('/');
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .into();
    let existing = load_corpus(out)?;
    let mut report = ExportReport::default();

    // Every marked event. A mark may have changed since the last export.
    let marked = list_events(&agent, server, "wrong=true", usize::MAX)?;
    for (record, raw) in &marked {
        match existing.get(&record.id) {
            Some(old) if old.feedback == record.feedback => {}
            Some(_) => {
                write_json(out, record.id, raw)?;
                report.refreshed += 1;
            }
            None => {
                // Without a clip it cannot be replayed, but its spot still counts.
                if save_event(&agent, server, out, record, raw)? {
                    report.marked_new += 1;
                } else {
                    write_json(out, record.id, raw)?;
                    report.no_clip += 1;
                }
            }
        }
    }
    // Marks taken back since: the copy says so too.
    for old in existing.values() {
        if old.feedback.is_some() && !marked.iter().any(|(r, _)| r.id == old.id) {
            let url = format!("{server}/api/v1/events/{}", old.id);
            if let Ok(raw) = get_json(&agent, &url) {
                write_json(out, old.id, &raw)?;
                report.refreshed += 1;
            }
        }
    }

    // A sample of settled, unmarked events, spread over cameras and labels.
    let existing = load_corpus(out)?;
    let have = existing.values().filter(|e| e.feedback.is_none()).count();
    let want = unmarked.saturating_sub(have);
    if want > 0 {
        let settled = chrono::Utc::now() - SETTLED;
        let mut buckets: BTreeMap<(String, Label), Vec<(EventRecord, serde_json::Value)>> =
            BTreeMap::new();
        for (record, raw) in list_events(&agent, server, "", scan)? {
            if record.feedback.is_none()
                && record.clip_state == ClipState::Ready
                && record.ended_at.is_some_and(|t| t < settled)
                && !existing.contains_key(&record.id)
            {
                buckets
                    .entry((record.camera_id.clone(), record.label))
                    .or_default()
                    .push((record, raw));
            }
        }
        // Round robin, newest first in each bucket.
        let mut added = 0;
        'fill: while added < want && buckets.values().any(|b| !b.is_empty()) {
            for bucket in buckets.values_mut() {
                if added == want {
                    break 'fill;
                }
                if bucket.is_empty() {
                    continue;
                }
                let (record, raw) = bucket.remove(0);
                if save_event(&agent, server, out, &record, &raw)? {
                    added += 1;
                } else {
                    report.no_clip += 1;
                }
            }
        }
        report.unmarked_new = added;
    }
    Ok(report)
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<serde_json::Value> {
    agent
        .get(url)
        .call()
        .and_then(|mut r| r.body_mut().read_json())
        .with_context(|| format!("cannot read {url}"))
}

/// Up to `max` events matching `query` (e.g. `wrong=true`), newest first, each parsed and as
/// the API gave it.
fn list_events(
    agent: &ureq::Agent,
    server: &str,
    query: &str,
    max: usize,
) -> Result<Vec<(EventRecord, serde_json::Value)>> {
    #[derive(Deserialize)]
    struct Page {
        items: Vec<serde_json::Value>,
        next_before_id: Option<u64>,
    }
    let mut events = Vec::new();
    let mut before: Option<u64> = None;
    while events.len() < max {
        let mut url = format!("{server}/api/v1/events?limit={MAX_PAGE}&{query}");
        if let Some(b) = before {
            url.push_str(&format!("&before_id={b}"));
        }
        let page: Page = serde_json::from_value(get_json(agent, &url)?)
            .with_context(|| format!("unexpected answer from {url}"))?;
        for raw in page.items {
            let record: EventRecord =
                serde_json::from_value(raw.clone()).context("unexpected event JSON")?;
            events.push((record, raw));
        }
        match page.next_before_id {
            Some(b) => before = Some(b),
            None => break,
        }
    }
    events.truncate(max);
    Ok(events)
}

/// Downloads the event's clip and writes its JSON. `false` when it has no clip.
fn save_event(
    agent: &ureq::Agent,
    server: &str,
    out: &Path,
    record: &EventRecord,
    raw: &serde_json::Value,
) -> Result<bool> {
    if record.clip_state != ClipState::Ready {
        return Ok(false);
    }
    let url = format!("{server}/api/v1/events/{}/clip.mp4", record.id);
    let mut response = match agent.get(&url).call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("cannot download {url}")),
    };
    let tmp = out.join(format!("{}.mp4.part", record.id));
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::io::copy(&mut response.body_mut().as_reader(), &mut file)
        .with_context(|| format!("cannot download {url}"))?;
    file.flush()?;
    std::fs::rename(&tmp, out.join(format!("{}.mp4", record.id)))?;
    write_json(out, record.id, raw)?;
    Ok(true)
}

fn write_json(out: &Path, id: u64, raw: &serde_json::Value) -> Result<()> {
    let path = out.join(format!("{id}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(raw)?)
        .with_context(|| format!("cannot write {}", path.display()))
}

/// The corpus's events by id, with or without a clip (`<id>.mp4`).
pub fn load_corpus(dir: &Path) -> Result<BTreeMap<u64, EventRecord>> {
    let mut events = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(events),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let record: EventRecord = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("cannot read {}", path.display()))?;
        events.insert(record.id, record);
    }
    Ok(events)
}

// ---------------------------------------------------------------------------------------------
// eval
// ---------------------------------------------------------------------------------------------

/// What should be found in an event's clip.
#[derive(Clone, Debug, PartialEq)]
pub enum Expect {
    /// No person, vehicle or animal.
    Nothing,
    Object {
        label: Label,
        /// The animal's name, when known (matched loosely: "deer" matches "white-tailed deer").
        species: Option<String>,
        /// The wrong label someone corrected: it must be gone.
        not_label: Option<Label>,
        /// The wrong name someone marked (without giving the right one): it must be gone.
        not_species: Option<String>,
    },
}

impl Expect {
    pub fn for_event(e: &EventRecord) -> Self {
        let name = || e.species.as_ref().map(|s| s.common_name.clone());
        let Some(f) = &e.feedback else {
            return match e.label {
                Label::Motion => Expect::Nothing,
                label => Expect::Object {
                    label,
                    species: if label == Label::Animal { name() } else { None },
                    not_label: None,
                    not_species: None,
                },
            };
        };
        match f.actual {
            None | Some(Label::Motion) => Expect::Nothing,
            Some(label) => {
                let same = label == e.label;
                Expect::Object {
                    label,
                    species: f.species.clone().filter(|_| label == Label::Animal),
                    not_label: (!same && e.label != Label::Motion).then_some(e.label),
                    not_species: (same && label == Label::Animal && f.species.is_none())
                        .then(name)
                        .flatten(),
                }
            }
        }
    }

    /// Whether `found` meets the expectation. Names are only checked when `named`.
    pub fn met_by(&self, found: &[Found], named: bool) -> bool {
        let objects: Vec<&Found> = found.iter().filter(|f| f.is_object()).collect();
        match self {
            Expect::Nothing => objects.is_empty(),
            Expect::Object {
                label,
                species,
                not_label,
                not_species,
            } => {
                let hits: Vec<&&Found> = objects.iter().filter(|f| f.label == *label).collect();
                let any_named = |name: &str| {
                    hits.iter()
                        .any(|f| f.species.as_deref().is_some_and(|s| same_name(s, name)))
                };
                !hits.is_empty()
                    && not_label.is_none_or(|bad| objects.iter().all(|f| f.label != bad))
                    && (!named
                        || (species.as_deref().is_none_or(any_named)
                            && !not_species.as_deref().is_some_and(any_named)))
            }
        }
    }
}

impl std::fmt::Display for Expect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expect::Nothing => f.write_str("nothing"),
            Expect::Object {
                label,
                species,
                not_label,
                not_species,
            } => {
                write!(f, "{label}")?;
                if let Some(s) = species {
                    write!(f, " ({s})")?;
                }
                if let Some(l) = not_label {
                    write!(f, ", not {l}")?;
                }
                if let Some(s) = not_species {
                    write!(f, ", not {s}")?;
                }
                Ok(())
            }
        }
    }
}

/// "person ×3, animal (red fox)": each kind once, in the order first found.
fn summarise(found: &[&Found]) -> String {
    let mut kinds: Vec<(String, usize)> = Vec::new();
    for f in found {
        let text = f.to_string();
        match kinds.iter_mut().find(|(k, _)| *k == text) {
            Some((_, n)) => *n += 1,
            None => kinds.push((text, 1)),
        }
    }
    kinds
        .into_iter()
        .map(|(k, n)| if n > 1 { format!("{k} ×{n}") } else { k })
        .collect::<Vec<_>>()
        .join(", ")
}

/// "deer" and "White-tailed Deer" are the same answer; "fox" and "red fox" too.
fn same_name(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim().to_lowercase(), b.trim().to_lowercase());
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// One test case's outcome, as saved for the next run's comparison.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseResult {
    pub camera: String,
    pub marked: bool,
    pub pass: bool,
    pub expected: String,
    pub found: String,
}

/// A saved run: `--save` writes it, `--baseline` reads it.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EvalRun {
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    pub version: String,
    pub species: bool,
    pub cases: BTreeMap<u64, CaseResult>,
}

pub struct EvalOptions {
    pub corpus: PathBuf,
    pub cameras: Vec<String>,
    pub species: bool,
    pub save: Option<PathBuf>,
    pub baseline: Option<PathBuf>,
}

/// Whether event `id`'s box `bbox` on `camera` overlaps another event marked "nothing there".
fn in_marked_spot(
    corpus: &BTreeMap<u64, EventRecord>,
    id: u64,
    camera: &str,
    bbox: &zoologist_core::BBox,
) -> bool {
    corpus.values().any(|e| {
        e.id != id
            && e.camera_id == camera
            && e.feedback.as_ref().is_some_and(|f| f.actual.is_none())
            && e.best_bbox.is_some_and(|b| b.iou(bbox) >= SAME_SPOT_IOU)
    })
}

pub fn eval(config: Config, options: &EvalOptions) -> Result<()> {
    let corpus = load_corpus(&options.corpus)?;
    if corpus.is_empty() {
        bail!(
            "no events in {} (fill it with `zoologist corpus`)",
            options.corpus.display()
        );
    }
    let baseline: Option<EvalRun> = match &options.baseline {
        Some(path) => Some(
            serde_json::from_slice(
                &std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?,
            )
            .with_context(|| format!("cannot read {}", path.display()))?,
        ),
        None => None,
    };
    // Clips run side by side. The worker counts change how fast, not what is found; each clip
    // has at most one detector job and one species job waiting, so the queues never drop one.
    let mut config = config;
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let parallel = cores.min(config.inference.queue_capacity).max(1);
    config.inference.workers = cores.min(8);
    config.species.workers = cores.min(8);
    let analyser = ClipAnalyser::new(config, options.species)?;
    let named = analyser.has_species();
    let mut run = EvalRun {
        at: Some(chrono::Utc::now()),
        version: env!("CARGO_PKG_VERSION").into(),
        species: named,
        cases: BTreeMap::new(),
    };
    let mut skipped: Vec<String> = Vec::new();
    let mut todo: Vec<(&EventRecord, zoologist_core::config::CameraConfig)> = Vec::new();
    for e in corpus.values() {
        if !options.cameras.is_empty() && !options.cameras.contains(&e.camera_id) {
            continue;
        }
        if !options.corpus.join(format!("{}.mp4", e.id)).exists() {
            continue;
        }
        match analyser
            .config()
            .cameras
            .iter()
            .find(|c| c.id == e.camera_id)
        {
            Some(camera) => todo.push((e, camera.clone())),
            None => skipped.push(format!(
                "#{}: camera {} is not in the config",
                e.id, e.camera_id
            )),
        }
    }
    let started = std::time::Instant::now();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<(u64, Result<ClipResult>)>> = std::sync::Mutex::default();
    std::thread::scope(|scope| {
        for _ in 0..parallel {
            scope.spawn(|| {
                use std::sync::atomic::Ordering::Relaxed;
                while let Some((e, camera)) = todo.get(next.fetch_add(1, Relaxed)) {
                    let clip = options.corpus.join(format!("{}.mp4", e.id));
                    let spot =
                        |b: &zoologist_core::BBox| in_marked_spot(&corpus, e.id, &e.camera_id, b);
                    let result = analyser.analyse(camera, &clip, None, &spot);
                    results
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((e.id, result));
                    eprint!("\r{}/{} clips", done.fetch_add(1, Relaxed) + 1, todo.len());
                }
            });
        }
    });
    eprintln!(
        "\r{} clips in {:.0} s ({parallel} at a time)",
        todo.len(),
        started.elapsed().as_secs_f64()
    );
    for (id, result) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
        let e = &corpus[&id];
        let found = match result? {
            ClipResult::Events(found) => found,
            ClipResult::Skipped(why) => {
                skipped.push(format!("#{id}: {why}"));
                continue;
            }
        };
        let expect = Expect::for_event(e);
        let objects: Vec<&Found> = found.iter().filter(|f| f.is_object()).collect();
        let found_text = if !objects.is_empty() {
            summarise(&objects)
        } else if found.is_empty() {
            "nothing".into()
        } else {
            format!("nothing ({})", summarise(&found.iter().collect::<Vec<_>>()))
        };
        run.cases.insert(
            id,
            CaseResult {
                camera: e.camera_id.clone(),
                marked: e.feedback.is_some(),
                pass: expect.met_by(&found, named),
                expected: expect.to_string(),
                found: found_text,
            },
        );
    }

    print_scorecard(&run, &corpus, baseline.as_ref());
    for s in &skipped {
        println!("skipped {s}");
    }
    if let Some(path) = &options.save {
        std::fs::write(path, serde_json::to_vec_pretty(&run)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("\nsaved to {}", path.display());
    }
    if let Some(base) = &baseline {
        let worse = run
            .cases
            .iter()
            .filter(|(id, c)| !c.pass && base.cases.get(id).is_some_and(|b| b.pass))
            .count();
        if worse > 0 {
            bail!("{worse} cases are worse than in the baseline");
        }
    }
    Ok(())
}

fn print_scorecard(run: &EvalRun, corpus: &BTreeMap<u64, EventRecord>, baseline: Option<&EvalRun>) {
    #[derive(Default)]
    struct Row {
        fixed: usize,
        wrong: usize,
        same: usize,
        changed: usize,
    }
    let mut rows: BTreeMap<&str, Row> = BTreeMap::new();
    for c in run.cases.values() {
        let row = rows.entry(&c.camera).or_default();
        match (c.marked, c.pass) {
            (true, true) => row.fixed += 1,
            (true, false) => row.wrong += 1,
            (false, true) => row.same += 1,
            (false, false) => row.changed += 1,
        }
    }
    println!(
        "\n{} cases{}",
        run.cases.len(),
        if run.species {
            ""
        } else {
            " (labels only: no species classifier)"
        }
    );
    println!(
        "{:<16} {:>21} {:>21}",
        "camera", "marked: fixed/wrong", "unmarked: same/changed"
    );
    let mut total = Row::default();
    for (camera, r) in &rows {
        println!(
            "{camera:<16} {:>15}/{:<5} {:>16}/{:<5}",
            r.fixed, r.wrong, r.same, r.changed
        );
        total.fixed += r.fixed;
        total.wrong += r.wrong;
        total.same += r.same;
        total.changed += r.changed;
    }
    println!(
        "{:<16} {:>15}/{:<5} {:>16}/{:<5}",
        "all", total.fixed, total.wrong, total.same, total.changed
    );

    let line = |id: &u64, c: &CaseResult| {
        let when = corpus
            .get(id)
            .map(|e| e.started_at.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let note = corpus
            .get(id)
            .and_then(|e| e.feedback.as_ref())
            .and_then(|f| f.note.as_deref())
            .map(|n| format!(" [{n}]"))
            .unwrap_or_default();
        format!(
            "  #{id:<6} {:<14} {when}  expected {}, found {}{note}",
            c.camera, c.expected, c.found
        )
    };
    let section = |title: &str, pick: &dyn Fn(&u64, &CaseResult) -> bool| {
        let lines: Vec<String> = run
            .cases
            .iter()
            .filter(|(id, c)| pick(id, c))
            .map(|(id, c)| line(id, c))
            .collect();
        if !lines.is_empty() {
            println!("\n{title} ({}):", lines.len());
            for l in lines {
                println!("{l}");
            }
        }
    };
    if let Some(base) = baseline {
        let before = |id: &u64| base.cases.get(id).map(|b| b.pass);
        section("WORSE than the baseline", &|id, c| {
            !c.pass && before(id) == Some(true)
        });
        section("better than the baseline", &|id, c| {
            c.pass && before(id) == Some(false)
        });
        section("new since the baseline", &|id, _| before(id).is_none());
    }
    section("unmarked, now found differently", &|_, c| {
        !c.marked && !c.pass
    });
    section("marked, still wrong", &|_, c| c.marked && !c.pass);
    section("marked, now right", &|_, c| c.marked && c.pass);
}

/// Prints what `zoologist corpus` did.
pub fn print_export(report: &ExportReport, out: &Path) -> Result<()> {
    let corpus = load_corpus(out)?;
    let with_clip = |e: &&EventRecord| out.join(format!("{}.mp4", e.id)).exists();
    let corpus: Vec<&EventRecord> = corpus.values().filter(with_clip).collect();
    let marked = corpus.iter().filter(|e| e.feedback.is_some()).count();
    let by_camera = corpus
        .iter()
        .fold(HashMap::<&str, usize>::new(), |mut m, e| {
            *m.entry(e.camera_id.as_str()).or_default() += 1;
            m
        });
    let mut by_camera: Vec<_> = by_camera.into_iter().collect();
    by_camera.sort();
    println!(
        "added {} marked and {} unmarked events, refreshed {}, {} without a clip",
        report.marked_new, report.unmarked_new, report.refreshed, report.no_clip
    );
    println!(
        "{}: {} clips ({marked} marked), {}",
        out.display(),
        corpus.len(),
        by_camera
            .iter()
            .map(|(c, n)| format!("{c} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests;
