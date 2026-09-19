//! `zoologist seed-demo`: fills the database with ~200 made-up events so the UI can be checked
//! without cameras (plan Step 10.2). Pictures come from `tools/fixtures/images`, and every event
//! with a clip shares one clip cut from the fox fixture.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{Duration, Timelike, Utc};
use zoologist_core::yuv::rgb_to_i420;
use zoologist_core::{BBox, Config, Frame, Label, SpeciesGuess};
use zoologist_store::{ClipState, EventPatch, NewEvent, Store};
use zoologist_video::clips::{write_snapshot, write_thumb};
use zoologist_video::file_source::load_file_items;
use zoologist_video::recorder::{SegmentBuilder, write_segment};

/// How many events to create.
const EVENTS: usize = 200;

/// One kind of demo event (boxes are what MegaDetector finds in each photo) and the picture used for it.
struct Kind {
    label: Label,
    image: &'static str,
    bbox: Option<[f32; 4]>,
    /// (common name, scientific name) for animals; `None` = unidentified.
    species: Option<(&'static str, &'static str)>,
    /// Relative frequency.
    weight: u32,
    /// Mostly seen at night.
    nocturnal: bool,
}

const KINDS: &[Kind] = &[
    Kind {
        label: Label::Person,
        image: "person.jpg",
        bbox: Some([0.07, 0.58, 0.24, 0.71]),
        species: None,
        weight: 30,
        nocturnal: false,
    },
    Kind {
        label: Label::Vehicle,
        image: "car.jpg",
        bbox: Some([0.68, 0.57, 0.88, 0.73]),
        species: None,
        weight: 35,
        nocturnal: false,
    },
    Kind {
        label: Label::Animal,
        image: "deer.jpg",
        bbox: Some([0.0, 0.04, 0.42, 1.0]),
        species: Some(("White-tailed deer", "Odocoileus virginianus")),
        weight: 22,
        nocturnal: true,
    },
    Kind {
        label: Label::Animal,
        image: "fox.jpg",
        bbox: Some([0.24, 0.5, 0.43, 0.65]),
        species: Some(("Red fox", "Vulpes vulpes")),
        weight: 14,
        nocturnal: true,
    },
    Kind {
        label: Label::Animal,
        image: "coyote.jpg",
        bbox: Some([0.76, 0.49, 0.93, 0.63]),
        species: Some(("Coyote", "Canis latrans")),
        weight: 8,
        nocturnal: true,
    },
    Kind {
        label: Label::Animal,
        image: "turkey.jpg",
        bbox: Some([0.03, 0.06, 0.91, 0.94]),
        species: Some(("Wild turkey", "Meleagris gallopavo")),
        weight: 10,
        nocturnal: false,
    },
    Kind {
        label: Label::Animal,
        image: "cat.jpg",
        bbox: Some([0.06, 0.18, 0.68, 0.85]),
        species: Some(("Domestic cat", "Felis catus")),
        weight: 12,
        nocturnal: true,
    },
    Kind {
        label: Label::Animal,
        image: "ringtail_night.jpg",
        bbox: Some([0.8, 0.32, 0.9, 0.46]),
        species: Some(("Procyonidae family", "Procyonidae")),
        weight: 6,
        nocturnal: true,
    },
    Kind {
        label: Label::Animal,
        image: "ringtail_night_ir.jpg",
        bbox: Some([0.32, 0.39, 0.42, 0.6]),
        species: None,
        weight: 6,
        nocturnal: true,
    },
    Kind {
        label: Label::Motion,
        image: "ringtail_night_ir.jpg",
        bbox: None,
        species: None,
        weight: 12,
        nocturnal: false,
    },
];

/// A small deterministic random number generator (the demo is the same on every run).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Turns a photo into a 960-pixel-wide frame, as if a camera had sent it.
fn photo_frame(image: &image::RgbImage) -> Result<Frame> {
    let width = 960;
    let height = (width * image.height() / image.width()) & !1;
    let small = image::imageops::thumbnail(image, width, height);
    Ok(Frame {
        camera_id: "demo".into(),
        seq: 0,
        captured_at: Utc::now(),
        width,
        height,
        i420: Arc::new(
            rgb_to_i420(small.as_raw(), width, height).map_err(|e| anyhow::anyhow!("{e:?}"))?,
        ),
    })
}

/// Writes the snapshot and thumbnail of event `id`, exactly as the pipeline does.
fn write_pictures(
    data_dir: &Path,
    frame: &Frame,
    bbox: Option<&BBox>,
    id: u64,
    date: &str,
) -> Result<(String, Option<String>)> {
    let snap = format!("snapshots/{date}/{id}.jpg");
    write_snapshot(frame, bbox, &data_dir.join(&snap))?;
    let Some(b) = bbox else {
        return Ok((snap, None));
    };
    let thumb = format!("thumbs/{date}/{id}.jpg");
    write_thumb(frame, b, &data_dir.join(&thumb))?;
    Ok((snap, Some(thumb)))
}

/// Cuts the fox fixture into `clips/demo.mp4`; returns its path and size, or `None` when the
/// fixture is missing.
fn demo_clip(data_dir: &Path, fixtures: &Path) -> Result<Option<(String, u64)>> {
    let video = fixtures.join("fox_walk_640x360_10fps.h264");
    if !video.exists() {
        return Ok(None);
    }
    let items = load_file_items(&format!("file://{}?fps=10", video.display()), Utc::now())
        .map_err(anyhow::Error::msg)?;
    let mut builder = SegmentBuilder::new(60);
    let mut segment = None;
    for item in items {
        segment = builder.push(item).or(segment);
    }
    let segment = builder
        .flush()
        .or(segment)
        .context("the fixture has no frames")?;
    // Written as a recording segment, then moved into clips/ (a segment is a valid clip).
    let written = write_segment(data_dir, "demo", &segment)?;
    let rel = "clips/demo.mp4".to_string();
    std::fs::create_dir_all(data_dir.join("clips"))?;
    std::fs::rename(data_dir.join(&written.path), data_dir.join(&rel))?;
    let _ = std::fs::remove_dir_all(data_dir.join("recordings/demo"));
    Ok(Some((rel, written.bytes)))
}

/// Picks an hour of the day: animals marked nocturnal mostly at night, others by day.
fn pick_hour(rng: &mut Rng, nocturnal: bool) -> u32 {
    let night = [20, 21, 22, 23, 0, 1, 2, 3, 4, 5];
    let day = [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18];
    let own_time = rng.below(10) < 8;
    if nocturnal == own_time {
        night[rng.below(night.len() as u64) as usize]
    } else {
        day[rng.below(day.len() as u64) as usize]
    }
}

pub fn seed_demo(config: &Config, fixtures: &Path) -> Result<()> {
    let data_dir: PathBuf = config.server.data_dir.clone();
    std::fs::create_dir_all(&data_dir)?;
    let store = Store::open(&data_dir.join("zoologist.redb"), config.station.timezone)?;
    if !store
        .events_since(Utc::now() - Duration::days(3650))?
        .is_empty()
    {
        bail!(
            "{} already has events; seed-demo only fills an empty database (use a separate server.data_dir)",
            data_dir.display()
        );
    }
    let mut cameras: Vec<String> = config.enabled_cameras().map(|c| c.id.clone()).collect();
    if cameras.is_empty() {
        cameras = vec!["driveway".into(), "backyard".into(), "garden".into()];
    }
    let images_dir = fixtures.join("images");
    let mut images = std::collections::HashMap::new();
    for kind in KINDS {
        if !images.contains_key(kind.image) {
            let path = images_dir.join(kind.image);
            let img = image::open(&path)
                .with_context(|| format!("cannot open {}", path.display()))?
                .to_rgb8();
            images.insert(kind.image, photo_frame(&img)?);
        }
    }
    let clip = demo_clip(&data_dir, fixtures)?;
    if clip.is_none() {
        tracing::warn!("fox fixture missing: demo events will have no clips");
    }

    let tz = config.station.timezone;
    let now = Utc::now();
    let total_weight: u32 = KINDS.iter().map(|k| k.weight).sum();
    let mut rng = Rng(0x5EED_2026_0919);
    // Times first, oldest first, so ids increase with time like real events.
    let mut plans = Vec::new();
    for n in 0..EVENTS {
        let mut pick = rng.below(u64::from(total_weight)) as u32;
        let kind = KINDS
            .iter()
            .find(|k| {
                if pick < k.weight {
                    true
                } else {
                    pick -= k.weight;
                    false
                }
            })
            .expect("weights add up");
        // Half of the events in the last day, the rest over the last 30 days.
        let days_ago = if n % 2 == 0 {
            0
        } else {
            1 + rng.below(29) as i64
        };
        let hour = pick_hour(&mut rng, kind.nocturnal);
        let local_now = now.with_timezone(&tz);
        let mut local = local_now - Duration::days(days_ago);
        local = local
            .with_hour(hour)
            .and_then(|t| t.with_minute(rng.below(60) as u32))
            .and_then(|t| t.with_second(rng.below(60) as u32))
            .unwrap_or(local);
        let mut started = local.with_timezone(&Utc);
        if started > now {
            started -= Duration::days(1); // today's hour has not come yet
        }
        plans.push((started, kind));
    }
    plans.sort_by_key(|(t, _)| *t);
    // The last two events are still in progress, to show the LIVE badge.
    let len = plans.len();
    plans[len - 1].0 = now - Duration::seconds(20);
    plans[len - 2].0 = now - Duration::seconds(50);

    for (i, (started, kind)) in plans.iter().enumerate() {
        let camera = cameras[rng.below(cameras.len() as u64) as usize].clone();
        let jitter = |rng: &mut Rng, v: f32| (v + (rng.unit() - 0.5) * 0.01).clamp(0.0, 1.0);
        let bbox = kind.bbox.map(|[x1, y1, x2, y2]| BBox {
            x1: jitter(&mut rng, x1),
            y1: jitter(&mut rng, y1),
            x2: jitter(&mut rng, x2),
            y2: jitter(&mut rng, y2),
        });
        let score = 0.45 + 0.5 * rng.unit();
        let event = store.insert_event(&NewEvent {
            camera_id: camera,
            label: kind.label,
            raw_class: Some(kind.label.as_str().to_string()),
            started_at: *started,
            top_score: score,
            median_score: score * 0.9,
            best_bbox: bbox,
            snapshot_path: None,
            thumb_path: None,
        })?;
        let date = zoologist_core::local_date_hour(*started, tz).0.to_string();
        let (snap, thumb) = write_pictures(
            &data_dir,
            &images[kind.image],
            bbox.as_ref(),
            event.id,
            &date,
        )?;
        let live = i >= len - 2;
        let clip_state = match (&clip, live, rng.below(20)) {
            (None, _, _) => ClipState::Failed,
            (Some(_), true, _) => ClipState::Pending,
            (Some(_), false, 0) => ClipState::Failed,
            (Some(_), false, 1) => ClipState::Purged,
            _ => ClipState::Ready,
        };
        let ready = clip_state == ClipState::Ready;
        let species = kind.species.map(|(common, sci)| {
            let p = 0.7 + 0.29 * rng.unit();
            SpeciesGuess {
                scientific_name: sci.to_string(),
                common_name: common.to_string(),
                score: p,
                model_id: "demo".into(),
                candidates: vec![
                    (common.to_string(), p),
                    ("Domestic dog".into(), (1.0 - p) * 0.6),
                    ("Striped skunk".into(), (1.0 - p) * 0.3),
                ],
            }
        });
        let patch = EventPatch {
            ended_at: (!live).then(|| *started + Duration::seconds(5 + rng.below(90) as i64)),
            snapshot_path: Some(Some(snap)),
            thumb_path: Some(thumb),
            clip_state: Some(clip_state),
            clip_path: Some(clip.as_ref().filter(|_| ready).map(|c| c.0.clone())),
            clip_bytes: Some(clip.as_ref().filter(|_| ready).map(|c| c.1)),
            species,
            ..Default::default()
        };
        store.update_event(event.id, &patch)?;
    }
    println!("seeded {EVENTS} demo events into {}", data_dir.display());
    Ok(())
}
