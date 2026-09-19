# Zoologist — Phased Implementation Plan

A Rust application that watches several RTSP cameras, detects **people, vehicles, animals (with
species), and general motion**, records clips of each event, and serves a web dashboard with bar
charts and clip playback. The UI is modelled on `~/git/birdsong`.

**Target:** AMD Ryzen 7 5700U (Zen 2, 8C/16T, 15 W) in a **Proxmox VM** running **Docker Compose**.
**Priorities:** performance, **no `unsafe` in our code**, **pure Rust wherever the ecosystem allows**.
**Cameras:** Reolink wired (PoE/Ethernet), Reolink battery cameras behind a **Reolink Home Hub**, and Wyze
cameras (USB-powered with RTSP firmware, others via `docker-wyze-bridge`, battery models best effort).
See STACKS.md §0.5.

This plan is written to be carried out **one step at a time by an AI coding agent**, including
less capable ones. Each step is a self-contained ticket: objective, files, interfaces, and
acceptance tests. The technology choices and pure-Rust trade-offs are explained in
[`STACKS.md`](STACKS.md) (read its §0 and §2 first). Read §0–§4 of this file before any step.

---

## 0. Rules for the implementing agent

1. **One step at a time, in order.** Finish a step's acceptance tests before starting the next.
   Do not "improve" earlier steps unless the current step says to. **Exception:** Phase 0 needs the owner's
   VM and cameras. Phases 1, 3, 5, Steps 2.1–2.2, 4.1, 6.1–6.2 and 9–10 do **not** depend on Phase 0 results
   and may proceed in parallel with it. Phase 0 must be finished before Steps 2.3 (decoder choice),
   4.2 (detector choice), 7.2 (Hub API details), and 11.x.
2. **No `unsafe` in project code.** Every crate root starts with `#![forbid(unsafe_code)]`. No exceptions.
3. **Pure Rust dependencies.** Do not add any crate that binds a C/C++ library (`*-sys` crates,
   `ort`, `openssl`, `rusqlite`/`sqlx-sqlite`, `ffmpeg-next`, `opencv`, `openh264`, `gstreamer`, `tch`).
   `scripts/check-pure-rust.sh` (Step 1.1) enforces this in CI. **Known, accepted nuance:** `tract-linalg`
   uses the `cc` crate at build time to assemble tract's own hand-written SIMD kernels (`.S` files). That is
   assembly shipped inside a Rust crate, not a C library and not FFI, and it is what makes tract fast. It is
   allowlisted by name in the script. The Docker builder therefore needs a C toolchain/assembler (the
   `rust:*-bookworm` images have one). The **only** other allowed exceptions are:
   - the `ffmpeg` **executable** as a child process, used only if Phase 0 chooses `decoder = "ffmpeg"` (§2.1);
   - an optional `ort` cargo feature, **off by default**, added only with owner approval (Phase 11.3).
   If you think you need another exception, **stop and ask the owner**.
4. Dependencies with internal `unsafe` (tokio, tract, fast_image_resize, redb, image) are acceptable.
   They are pure Rust; the rule is about *our* code.
5. At the end of every step, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`, and `scripts/check-pure-rust.sh` must pass. `unwrap()`/`expect()` are allowed only in
   tests and `main.rs` startup.
6. Every public item on a crate boundary gets a `///` doc comment. Every step adds tests. Prefer
   small, boring, obvious code. **Measure before optimising**. Every performance change records
   before/after numbers in `docs/DECISIONS.md`.
7. Record each non-obvious decision in `docs/DECISIONS.md`: a dated entry with *Decision / Why /
   Alternatives rejected*.
8. If this plan is wrong (an API changed, a crate is abandoned, a number is off), fix the plan **and**
   add a `DECISIONS.md` entry. The plan is a living document.
9. Python is a **dev-time tool only**, for model conversion and golden outputs under `tools/`. It
   never ships in the image.
10. **Git:** commit messages, PR text, and code comments must not mention AI tools or carry
    `Co-Authored-By`/"Generated with" lines (the owner's birdsong convention). Use imperative subjects with a short "why" body.
11. When stuck for more than ~3 attempts on the same error, **stop** and write what you tried in
    `docs/BLOCKERS.md` instead of rewriting large parts of the code.

**Notation.** `[VERIFY]` means check this fact against real docs or code first. `[ASK OWNER]` means the owner decides.

---

## 1. What we are building

### 1.1 Requirements

| # | Requirement | Where it lands |
|---|---|---|
| R1 | Rust, pure Rust where possible, no `unsafe` | §0 rules 2–4, `check-pure-rust.sh`, Phase 0 checks |
| R2 | One or many cameras | `[[cameras]]` config; `retina` RTSP and HTTP-FLV sources (Phase 2) |
| R3 | Detect people, vehicles, animals | tract detector (Phase 4) |
| R4 | Detect general movement | motion (Phase 3), `motion` events (Phase 5) |
| R5 | Name animal species | species classifier (Phase 8) |
| R6 | As fast as possible on a Ryzen 7 5700U | motion gating, 320 px crops, worker pool, build flags (§2, Phases 0 and 11) |
| R7 | Easy UI with bar graphs and clips | dashboard (Phase 10), clips (Phase 6) |
| R8 | Runs in Docker Compose in a Proxmox VM | Step 0.1, Phase 11 |
| R9 | UI in the style of birdsong | reuse birdsong `static/` patterns (Phase 10) |
| R10 | Reolink wired cameras, reliably | HTTP-FLV source (pure Rust) with RTSP as the alternative (Phase 2) |
| R11 | Reolink battery cameras (no stream without the Hub; must not drain batteries) | Hub recording importer: analyse each Hub clip once (Step 7.2) |
| R12 | Wyze cameras | RTSP firmware → RTSP source; others via the `docker-wyze-bridge` sidecar (Steps 0.5, 11.3) |

### 1.2 Non-goals for v1

Model training; face and licence-plate recognition; a 24/7 timeline player; a live WebRTC view;
authentication (LAN only); GPU inference; audio (recordings are video-only).

---

## 2. Architecture

### 2.1 Runtime data flow (all in one `zoologist` binary)

```
                         ┌──── VideoSource: RTSP (retina) or HTTP-FLV (main stream) ────┐
 Camera ─────────────────┤                                                             ▼
                         │                                            Mp4SegmentRecorder (pure Rust:
                         │                                            buffer 10 s of samples → write .mp4 + .idx)
                         │                                                             │
                         └──── VideoSource: RTSP or HTTP-FLV (substream, H.264, ~5 fps) ┐  ▼ data/recordings/…
                                                                                      │
                                         H264Decoder trait ◄──────────────────────────┘
                                   (RustDecoder | FfmpegPipeDecoder)
                                                  │ Frame (I420 YUV)
                                                  ▼
                                    MotionDetector (Y plane only) ── motion boxes
                                                  │
                        crop square region → YUV→RGB → 320×320 tensor
                                                  ▼
           shared ─► DetectorPool: N worker threads, one Arc<tract optimised model>
                                                  │ detections
                                                  ▼
                                   Tracker + EventManager ──► ClipBuilder (pure-Rust remux from segments)
                                        │            │
                              animal crops           ▼ EventUpdate
                                        ▼       Store (redb) ──broadcast──► axum API + SSE + static UI
                           SpeciesPool (1–2 low-priority threads)              ▲
                                                                          Janitor (retention)
```

- Each camera runs as tokio tasks. **All CPU-heavy work** (decode, inference, JPEG, MP4 writing)
  runs on dedicated `std::thread`s or `spawn_blocking`, never directly on the async runtime.
- The detector sees **only motion regions** (plus a "keep-alive" region around each active track
  every 2 s). The species model sees **at most `max_crops_per_event` crops per animal event**.
- Wall-clock timestamps come from frame arrival on **both** streams, so clips line up with
  detections without guessing offsets.
- **Two camera kinds** (STACKS.md §0.5):
  - `kind = "stream"`: continuous live analysis, as in the diagram. The transport is `rtsp` (retina) or
    `flv` (Reolink HTTP-FLV). Both produce the same `StreamItem`s, so everything downstream is shared.
  - `kind = "hub_clips"`: Reolink battery cameras. There is no live stream. A `HubImporter` polls the Hub for new
    recordings, downloads them, and runs **the same** decode → motion → detect → track pipeline over the file
    faster than real time. The downloaded file becomes the event's clip. There is no segment recorder for these cameras.

### 2.2 Cargo workspace layout

```
zoologist/
├── Cargo.toml                    # workspace
├── STACKS.md  IMPLEMENTATION_PLAN.md  README.md
├── config/zoologist.example.toml
├── docs/  DECISIONS.md  API.md  MODELS.md  PROXMOX.md  CAMERAS.md  PERFORMANCE.md  UI_CHECKLIST.md
├── crates/
│   ├── zoologist-core/     # config, domain types, time helpers, YUV/image helpers, errors
│   ├── zoologist-video/    # RTSP (retina) + HTTP-FLV sources, H264Decoder impls, YUV helpers,
│   │                       # MP4 writer + reader, recorder, clip builder, Reolink Hub API client
│   ├── zoologist-vision/   # motion, detector (tract), pre/post-processing, tracker, species classifier
│   ├── zoologist-store/    # redb tables, queries, stats aggregation, retention planning
│   └── zoologist-server/   # `zoologist` binary: pipeline wiring, CLI, HTTP API, static UI
├── spikes/                 # Phase 0 throwaway benchmark binaries (a separate workspace, never shipped)
├── static/                 # index.html, style.css, app.js, favicon.svg
├── models/                 # git-ignored; scripts/fetch-models.sh
├── scripts/  fetch-models.sh  check-pure-rust.sh  fake-camera.sh
├── tools/convert_models/   # Python, dev-time only
├── tools/fixtures/         # small committed fixtures + golden JSON
├── Dockerfile
└── docker-compose.yml
```

### 2.3 Core dependencies (all pure Rust)

Use the latest compatible versions when running Step 1.1 `[VERIFY]`:

```toml
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "1"
bytes = "1"
chrono = { version = "0.4", features = ["serde"] }
chrono-tz = "0.10"
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "fs", "io-util", "process"] }
tokio-util = "0.7"
futures-util = "0.3"
axum = "0.8"
tower-http = { version = "0.7", features = ["fs", "cors", "compression-gzip", "trace"] }
retina = "0.4.20"              # RTSP client; add features = ["h265"] only if an H.265 main stream is recorded
ureq = { version = "3.4", default-features = false, features = ["json"] }  # plain-HTTP client: no TLS, no ring
url = "2"
h264-reader = "0.9"            # SPS parsing (width/height), NAL helpers
tract-onnx = "0.23"            # same line as birdsong
fast_image_resize = "6"
image = { version = "0.25", default-features = false, features = ["jpeg", "png"] }
imageproc = { version = "0.27", default-features = false }
ab_glyph = "0.2"
redb = "4"
tempfile = "3"
# Chosen in Step 0.4 (pure-Rust H.264 decoder): rusty_h264-decoder = "0.16"  [VERIFY] API
```

Workspace profiles:

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
debug = "line-tables-only"   # readable backtraces/profiles at no runtime cost
# panic stays "unwind": a panicking worker thread is logged and restarted instead of killing every camera.

[profile.dev.package."*"]
opt-level = 3                # tract and resize run at full speed in tests
```

`target-cpu` is **not** set globally. tract and fast_image_resize choose AVX2 at runtime, so the
binary still starts, and can print a useful error, on a VM that hides AVX2. The Docker build arg
`TARGET_CPU=x86-64-v3` is an optional extra (Step 11.3).

---

## 3. Shared definitions

### 3.1 Domain types (`zoologist-core`)

```rust
pub type CameraId = String;                       // "driveway", matches ^[a-z0-9_-]+$

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Label { Person, Vehicle, Animal, Motion }

/// Normalised (0.0..=1.0) box in detect-frame coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BBox { pub x1: f32, pub y1: f32, pub x2: f32, pub y2: f32 }
// impl: area, iou, center, union, expand(frac), to_pixels(w,h), clamp

/// One decoded frame in I420 layout (Y plane w*h, then U and V planes (w/2)*(h/2) each).
#[derive(Clone)]
pub struct Frame {
    pub camera_id: CameraId,
    pub seq: u64,
    pub captured_at: DateTime<Utc>,   // wall clock when the source received the access unit (file sources: file start + pts)
    pub width: u32, pub height: u32,  // even numbers
    pub i420: Arc<Vec<u8>>,           // len == w*h*3/2
}
// impl: y_plane(&self) -> &[u8]. Conversions live in zoologist_core::yuv (Step 1.3), so the video,
// vision and server crates can all use them without depending on each other.

pub struct Detection { pub label: Label, pub raw_class: String, pub score: f32, pub bbox: BBox }

pub struct SpeciesGuess {
    pub scientific_name: String, pub common_name: String, pub score: f32,
    pub model_id: String, pub candidates: Vec<(String, f32)>,   // top-5 common names
}
```

### 3.2 Class mapping (`zoologist-vision/src/classes.rs`, unit tested)

| Model | Raw class → `Label` |
|---|---|
| MegaDetector | `animal`→Animal, `person`→Person, `vehicle`→Vehicle |
| COCO | `person`→Person; `bicycle, car, motorcycle, bus, truck, train, boat`→Vehicle; `bird, cat, dog, horse, sheep, cow, elephant, bear, zebra, giraffe`→Animal; everything else is ignored |

### 3.3 Configuration (`config/zoologist.example.toml`)

```toml
[station]
name = "Home"
timezone = "America/New_York"
country = "USA"            # ISO-3166 alpha-3, SpeciesNet geofence
admin1_region = "NY"

[server]
bind = "0.0.0.0:8090"
data_dir = "/data"
cors_allow_origins = ["*"]

[video]
decoder = "rust"           # "rust" | "ffmpeg"  (chosen in Step 0.4)
ffmpeg_path = "ffmpeg"     # only used when decoder = "ffmpeg"

[inference]
detector = "md-spruce"     # key into [models.*]
workers = 3                # detector threads (each ~1 core while busy)
queue_capacity = 12        # pending regions; the oldest is dropped when full
max_regions_per_frame = 2
keepalive_seconds = 2

[models.md-spruce]
path = "/models/md_v1000_spruce.onnx"
kind = "yolov5"            # yolov5 | yolov8 | yolo_e2e   (output decoder, see MODELS.md)
classes = "megadetector"   # megadetector | coco
input_size = 320
score_threshold = 0.35

[species]
enabled = true
model = "speciesnet"       # speciesnet | bioclip
path = "/models/speciesnet.onnx"
labels = "/models/speciesnet_labels.txt"
geofence = "/models/speciesnet_geofence.json"
workers = 1
max_crops_per_event = 3
min_score = 0.5

[motion]
analysis_width = 320       # Y plane is downscaled to this width
threshold = 25
contour_area = 0.002
frame_alpha = 0.02
lightning_fraction = 0.5
motion_event_min_seconds = 3
motion_event_cooldown_seconds = 120

[tracking]
min_hits = 3
max_missed_seconds = 5
iou_match = 0.3
min_event_score = 0.55

[recording]
segment_seconds = 10
keep_segments_hours = 6
pre_capture_seconds = 5
post_capture_seconds = 5

[retention]
clips_days = 30
clips_max_total_mb = 100000

[[reolink_hubs]]
id = "home-hub"
url = "http://192.168.1.10"      # enable HTTP on the Hub (Network → Advanced → Server settings); no TLS client in a pure-Rust build
user = "zoologist"               # a dedicated low-privilege Hub user with playback rights
password = "…"
poll_seconds = 30
lookback_minutes = 30            # on startup, import recordings this far back
analyse_stream = "sub"           # analyse the H.264 sub recording, keep the main recording as the clip
no_detection = "motion"          # "motion" = keep it as a motion event | "discard"
clip_codec_fallback = "sub"      # if the main recording is H.265: "sub" = use the H.264 sub file as the clip | "main" = keep H.265

# --- Reolink wired camera over HTTP-FLV (preferred for <= 5 MP) ---
[[cameras]]
id = "driveway"
name = "Driveway"
kind = "stream"
transport = "flv"                # "flv" | "rtsp"
detect_url = "http://192.168.1.20/flv?port=1935&app=bcs&stream=channel0_sub.bcs&user=USER&password=PASS"
record_url = "http://192.168.1.20/flv?port=1935&app=bcs&stream=channel0_main.bcs&user=USER&password=PASS"
detect_fps = 5            # frames above this rate are decoded but not analysed
record = true
labels = ["person", "vehicle", "animal", "motion"]
# motion_mask = [[0.0,0.0, 0.3,0.0, 0.3,0.08, 0.0,0.08]]   # e.g. Reolink timestamp overlay

# --- Reolink 4K/8MP wired camera over RTSP (main stream may be H.265) ---
[[cameras]]
id = "backyard"
name = "Backyard"
kind = "stream"
transport = "rtsp"
detect_url = "rtsp://USER:PASS@192.168.1.21:554/h264Preview_01_sub"   # newer firmware: Preview_01_sub  [VERIFY]
record_url = "rtsp://USER:PASS@192.168.1.21:554/h264Preview_01_main"
detect_fps = 5
record = true
labels = ["person", "vehicle", "animal", "motion"]

# --- Wyze with RTSP firmware (wz_mini_hacks shown) or via docker-wyze-bridge ---
[[cameras]]
id = "garden-wyze"
name = "Garden (Wyze)"
kind = "stream"
transport = "rtsp"
detect_url = "rtsp://USER:PASS@192.168.1.30:8554/video2_unicast"   # SD
record_url = "rtsp://USER:PASS@192.168.1.30:8554/video1_unicast"   # HD
# via bridge instead: "rtsp://wyze-bridge:8554/<camera-name>" (the bridge's naming) [VERIFY]
detect_fps = 5
record = true
labels = ["person", "vehicle", "animal", "motion"]

# --- Reolink battery camera behind the Home Hub (no live stream) ---
[[cameras]]
id = "trail-argus"
name = "Trail (Argus)"
kind = "hub_clips"
hub = "home-hub"
channel = 0                      # the Hub API channel number (0-based)
labels = ["person", "vehicle", "animal", "motion"]
```

Validation (each rule unit tested): unique ids matching `^[a-z0-9_-]+$`; `kind = "stream"` needs
`transport`, `detect_url`, `record_url` (`rtsp://`/`rtsps://` for rtsp, `http://` for flv, `file://`
for tests); `kind = "hub_clips"` needs an existing `hub` and a `channel`, and must not have URLs; hub
URLs must be `http://` (explain the TLS reason in the error); `detect_fps` 1–15; `workers` 1–8; the timezone parses; thresholds in
0..1; `decoder = "ffmpeg"` requires `ffmpeg_path` to be executable (checked at `run` start). **Never log
credentials.** Use `redact_url()` everywhere.

### 3.4 Database (redb, `zoologist-store`)

```rust
const META: TableDefinition<&str, u64>              = TableDefinition::new("meta");            // "schema_version", "next_event_id"
const EVENTS: TableDefinition<u64, &[u8]>           = TableDefinition::new("events");          // id → JSON EventRecord
const EVENTS_BY_TIME: TableDefinition<(i64, u64), ()> = TableDefinition::new("events_by_time"); // (started_at µs, id)
const SEGMENTS: TableDefinition<(&str, i64), &[u8]> = TableDefinition::new("segments");        // (camera, start µs) → JSON SegmentRecord
```

```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct EventRecord {
    pub id: u64, pub camera_id: String, pub label: Label, pub raw_class: Option<String>,
    pub started_at: DateTime<Utc>, pub ended_at: Option<DateTime<Utc>>,
    pub local_date: NaiveDate, pub local_hour: u8,
    pub top_score: f32, pub median_score: f32, pub best_bbox: Option<BBox>,
    pub species: Option<SpeciesGuess>,
    pub snapshot_path: Option<String>, pub thumb_path: Option<String>,
    pub clip_path: Option<String>, pub clip_bytes: Option<u64>,
    pub clip_state: ClipState,          // Pending | Ready | Failed | Purged
}
#[derive(Serialize, Deserialize, Clone)]
pub struct SegmentRecord { pub path: String, pub index_path: String, pub started_at: DateTime<Utc>, pub ended_at: DateTime<Utc>, pub bytes: u64 }
```

Event ids come from `next_event_id` and are strictly increasing, so `id` order is insertion order.
Aggregations (charts) scan `EVENTS_BY_TIME` over the window and count in Rust. Volume is small (at
most tens of thousands of events in 30 days). Keep an in-memory 10 s cache per stats query.
redb is synchronous: every store call runs inside `spawn_blocking`.

### 3.5 Files under `data_dir`

```
data/zoologist.redb
data/recordings/<camera>/<YYYYMMDD>/<YYYYMMDDTHHMMSS.ffffffZ>.mp4   # H.264, faststart, video only
data/recordings/<camera>/<YYYYMMDD>/<…>.idx                        # JSON sample index (Step 6.2)
data/clips/<YYYY-MM-DD>/<event_id>.mp4
data/snapshots/<YYYY-MM-DD>/<event_id>.jpg
data/thumbs/<YYYY-MM-DD>/<event_id>.jpg
data/latest/<camera>.jpg
```

---

## 4. Test fixtures `[ASK OWNER]`

The owner provides (git-ignored, in `tools/fixtures/owner/`):
- **Raw captures of each camera's substream and main stream**: 30 s each. Make them with
  `zoologist capture` (Step 0.4), which saves retina's H.264 access units exactly as received
  (`.h264` Annex-B + `.ts.json` timestamps). These drive the decoder and recorder tests offline.
- For each FLV camera: a 30 s raw `.flv` capture of the substream (`curl -o sub.flv --max-time 30 '<flv url>'`),
  used by the FLV parser tests.
- 5–10 **Reolink Hub recordings** downloaded through the API (Step 0.5), both `sub` and `main` for the
  same events, from battery cameras, including a PIR trigger with no animal visible.
- 5–10 short clips (10–30 s) including a person, a car, an animal by day, an animal at night (IR), and
  wind/rain only, plus `labels.json` with the expected labels and species per clip.

Committed small fixtures: synthetic H.264 streams (Main and High profile, 640×360, 10 s) and a moving
white square made with ffmpeg on a dev machine (commands in `tools/fixtures/README.md`), plus a few
CC-licensed JPEGs (person, car, deer, fox, bird) with their sources.

**Fake camera for integration tests** (dev machine only): MediaMTX in Docker publishing a looped
fixture (`scripts/fake-camera.sh`). Tests that need it are `#[ignore]`d. Run them with `cargo test -- --ignored`.

---

## Phase 0 — Target-hardware checks (measure before building)

These steps produce **numbers and decisions**, not product code. Put code in `spikes/` (its own
workspace), and put the results in `docs/PERFORMANCE.md` and `docs/DECISIONS.md`.

### Step 0.1 — Proxmox VM and Docker host

**Objective:** A VM that exposes AVX2/FMA to guests and runs Docker.

**Deliverables:** `docs/PROXMOX.md` with the exact steps and the values used:
- Create the VM (adjust ids and storage) `[VERIFY]` against the installed Proxmox version:
  ```bash
  qm create 210 --name zoologist --ostype l26 --machine q35 --cpu host --sockets 1 --cores 12 \
    --memory 6144 --balloon 0 --scsihw virtio-scsi-single \
    --scsi0 local-lvm:32,iothread=1,discard=on,ssd=1 \
    --scsi1 local-lvm:500,iothread=1,discard=on,ssd=1,backup=0 \
    --net0 virtio,bridge=vmbr0 --agent enabled=1 --onboot 1
  ```
  **`--cpu host` is mandatory.** Other CPU types hide AVX2/FMA.
- Guest: Debian 12/13 minimal, `qemu-guest-agent`, Docker CE + compose plugin (official Docker apt
  repo), `scsi1` formatted ext4/xfs and mounted at `/srv/zoologist/data` via `/etc/fstab` by UUID.
- Verify: `grep -o -w -E 'avx2|fma|bmi2' /proc/cpuinfo | sort -u` prints all three; `nproc` = 12.
- Record the host BIOS cTDP setting (15 W / 25 W) if the mini-PC exposes it, and the host CPU governor.

**Acceptance:** the verify commands pass, and `docker run --rm hello-world` works in the VM.

### Step 0.2 — Export candidate models to ONNX

**Status:** done on the dev machine (`tools/convert_models/`, `scripts/fetch-models.sh`, `docs/MODELS.md`). Exported: MegaDetector v1000 sorrel/spruce/larch, YOLO11n/YOLO26n COCO (320 and 640), SpeciesNet always_crop v4.0.3a. The test photos are 10 CC0/PD/CC-BY photos from Wikimedia Commons (`tools/fixtures/images/SOURCES.md`). Correction to the text below: MDv1000 spruce is YOLOv5s (layout `yolov5`), sorrel and larch are YOLO11 (layout `yolov8`).

**Objective:** tract-compatible ONNX files and Python golden outputs.

**Deliverables** in `tools/convert_models/` (venv, `requirements.txt`, README):
- `export_megadetector.py`: MegaDetector v1000 **spruce** and **larch** → ONNX, **fixed input
  `1×3×320×320` and `1×3×640×640`**, float32, opset 17, **no NMS in the graph**, simplified with
  `onnxsim`. Record the architecture and output layout (`[1,N,5+C]` yolov5-style or `[1,4+C,N]` yolov8-style) `[VERIFY]`.
- `export_yolo.py`: `yolov8n` and `yolo26n` (COCO) at 320 and 640, `simplify=True`. For yolo26n, also export with
  the one-to-one end-to-end head **disabled** if tract rejects the default graph `[VERIFY]`. Note the AGPL-3.0 licence.
- `export_speciesnet.py`: the **classifier only** (EfficientNetV2-M) → ONNX, fixed input (`1×480×480×3`
  or `1×3×480×480`; record which) `[VERIFY]`. Write `speciesnet_labels.txt` in output order (keep the full
  taxonomy strings) and convert its geofence data to `speciesnet_geofence.json` `[VERIFY]` source file and format.
- `export_bioclip.py` (BioCLIP v1 ViT-B/16 image tower) and `bioclip_text_embeddings.f32` for an owner
  species list. Optional; can be deferred to Step 8.3.
- `make_goldens.py`: run every exported ONNX in **onnxruntime (Python)** on `tools/fixtures/images/*` with the
  exact pre-processing written in `docs/MODELS.md`, and write `tools/fixtures/golden/<model>_<size>.json`.
- `scripts/fetch-models.sh` + `models/SHA256SUMS`.

**Acceptance:** onnxruntime output matches the source framework (max abs diff < 1e-3). `docs/MODELS.md`
lists, per model: source URL, licence, input shape/layout/normalisation, output layout, checksum.

### Step 0.3 — tract benchmark on the 5700U VM

**Status:** tool done (`spikes/tract-bench`), and it was run on the dev machine: all models load in tract and match onnxruntime (relative error ≤ 1.2e-5). Results and the detector choice are in `docs/PERFORMANCE.md` and DECISIONS #13. **Still to do:** run it on the 5700U (inside Docker is fine; a VM is not required).

**Objective:** Prove every model loads in tract, and measure its speed on the target.

**Deliverables:** `spikes/tract-bench` binary:
`tract-bench <model.onnx> --shape 1,3,320,320 --iters 100 --threads-list 1,2,3,4,6`
- Load with `tract_onnx::onnx().model_for_path(p)?.with_input_fact(0, f32::fact(shape).into())?.into_optimized()?.into_runnable()?`
  `[VERIFY]` the exact API for the pinned tract version.
- For each `T` in the thread list: spawn `T` threads sharing one `Arc` of the runnable model, each running
  `iters` inferences. Report **per-inference latency (p50/p95)** and **total inferences/second**.
- Check that the outputs match `golden/*.json` (same tolerance as Step 4.2), so tract is correct, not just fast.
- Print `is_x86_feature_detected!("avx2")`/`("fma")` at start.

Run it in the VM (in Docker, from the same base image as production) for: MD spruce @320/@640,
MD larch @320, yolov8n @320, yolo26n @320, SpeciesNet @480, BioCLIP-B @224.

**Acceptance:** a results table in `docs/PERFORMANCE.md`, plus a DECISIONS entry that picks:
(a) the default detector and input size (target **p50 ≤ 80 ms @320** on one thread), (b) the default
`inference.workers` (the thread count where inferences/second stops rising, usually 3–4), and (c) whether
tract is fast enough. **If no detector reaches ≥ 20 inferences/s with 3 workers, stop and report to the
owner** with the options: fewer fps, a smaller model, or the `ort` feature (FFI).

### Step 0.4 — RTSP capture and pure-Rust H.264 decode check

**Objective:** Confirm retina works with the owner's cameras, and decide `video.decoder`.

**Deliverables:**
1. `zoologist capture --camera <id> --stream detect|record --seconds 30` (built in Phase 2, see `crates/zoologist-video/src/capture.rs`): connects like the real pipeline, prints codec, profile (from SPS via
   `h264-reader`), resolution, fps, and keyframe interval, and saves 30 s as Annex-B `.h264` +
   `.units.jsonl` (per access unit: byte range, wall time µs, 90 kHz timestamp, keyframe) plus `.info.json`. `zoologist probe` prints fps, keyframe interval, bitrate and decode time.
2. Decoder comparison: `cargo test --release -p zoologist-video -- --ignored --nocapture decoder_report` does this for the
   synthetic fixtures (all bit-exact, 0.3–0.5 ms/frame on an M-series core; DECISIONS #8). Point it at the owner's captures. It decodes the captured `.h264` with each candidate pure-Rust decoder
   (start with `rusty_h264-decoder`, which claims Main/High + CABAC, then `rusty_h264`) `[VERIFY]` their
   APIs and maturity, and with `ffmpeg -f h264 -i pipe:0 -f rawvideo -pix_fmt yuv420p pipe:1` as the reference.
   Report per decoder: frames decoded, **ms per frame**, and **PSNR of the Y plane vs ffmpeg**
   (write a small PSNR function).
3. A capture of every camera: note any camera whose substream is **not H.264** (it must be switched in the camera UI).

**Decision rule** (record in DECISIONS.md):
- A pure-Rust decoder qualifies if it decodes **100 % of frames of every camera's substream** without
  error, has **Y-PSNR ≥ 40 dB** vs ffmpeg (≥ 99 dB means bit-exact), and takes **≤ 15 ms per 640×360 frame**
  on one 5700U thread. → `decoder = "rust"`, and the image has no ffmpeg.
- Otherwise → `decoder = "ffmpeg"` (child process, §0 rule 3), and **tell the owner which check failed**.

**Acceptance:** capture files exist for every camera, the decode table is in PERFORMANCE.md, and the decision is recorded.

### Step 0.5 — Camera inventory and settings (`docs/CAMERAS.md`)

**Objective:** Know exactly how each camera will connect before writing ingest code.

**Deliverables:** `docs/CAMERAS.md` with one row per camera: brand/model, power (PoE/USB/battery),
firmware, IP, **kind/transport**, the substream and main stream codec/resolution/fps/GOP (from `spikes/capture`
or `ffprobe`), whether RTSP and FLV both work, and notes. Plus these checklists, filled in:

- **Reolink wired:** RTSP and HTTP enabled (*Network → Advanced → Server settings*); substream set to
  **H.264, 640×360, 5 fps**; I-frame interval **1× or 2× the fps** (shorter clips and segments); main stream
  set to H.264 if the model allows it (record which models are H.265-only). Test **both** FLV and RTSP for
  10 minutes each with `spikes/capture --minutes 10` and count reconnects and gaps. Choose the transport per camera.
  Create a dedicated camera user for Zoologist.
- **Reolink Home Hub:** enable HTTP; create a low-privilege user with playback rights; with `curl`, run through
  `Login` → `GetChannelstatus` (map channel numbers to battery cameras) → `Search` (one day, `streamType` `sub`
  and `main`) → `Download` of one file each. Save the exact request/response JSON (redacted) in
  `docs/CAMERAS.md`: these are the reference for Step 7.2 `[VERIFY]` field names against your firmware.
  Record the recording container (MP4?) and codecs, and how soon after a PIR event the file appears in `Search`.
- **Wyze:** per camera, which path works: official RTSP firmware, `wz_mini_hacks`, `docker-wyze-bridge`
  (which fork and version), or **none**. Note the client limit (try two simultaneous `ffprobe`s). Battery Wyze
  models: try the bridge. If it fails, mark the camera unsupported for v1 and tell the owner.

**Acceptance:** every camera has a row, a chosen path, or "unsupported (why)". The owner has reviewed the table.

---

## Phase 1 — Foundation

### Step 1.1 — Workspace skeleton and purity check

**Deliverables**
- Workspace `Cargo.toml` (§2.3 deps and profiles) and the five crates from §2.2, each with `#![forbid(unsafe_code)]`.
- `zoologist-server/src/main.rs`: `clap` subcommands `run`, `check-config`, `capture`, `probe`,
  `detect`, `classify`, `bench`, `seed-demo`, `healthcheck` (stubs exit 2).
- `scripts/check-pure-rust.sh`:
  ```bash
  #!/usr/bin/env bash
  set -euo pipefail
  # Crates that are pure Rust despite the -sys suffix or that only declare OS APIs.
  ALLOW='^(linux-raw-sys|windows-sys|windows-targets|libc)$'
  # Crates allowed to use `cc` at build time (tract assembles its own SIMD kernels; see §0 rule 3).
  CC_ALLOW='^(tract-linalg)$'
  bad=$(cargo tree --workspace -e normal,build --prefix none --format '{p}' \
        | awk '{print $1}' | sort -u | grep -E -- '-sys$' | grep -Ev "$ALLOW" || true)
  if [ -n "$bad" ]; then echo "Non-Rust (-sys) crates found:"; echo "$bad"; exit 1; fi
  if cargo tree --workspace -e build -i cc >/dev/null 2>&1; then
    users=$(cargo tree --workspace -e build -i cc --depth 1 --prefix none --format '{p}' \
            | awk '{print $1}' | grep -v '^cc$' | sort -u | grep -Ev "$CC_ALLOW" || true)
    if [ -n "$users" ]; then echo "Crates compiling native code via 'cc':"; echo "$users"; exit 1; fi
  fi
  echo "pure-rust check OK"
  ```
- `deny.toml` (cargo-deny) with `bans.deny` for `ort`, `ort-sys`, `openssl`, `openssl-sys`, `libsqlite3-sys`,
  `ffmpeg-sys-next`, `opencv`, `openh264-sys2`, `ring`, `aws-lc-sys`, and licence allow-lists.
- CI workflow: fmt, clippy, test, `check-pure-rust.sh`, `cargo deny check`. `Makefile` targets `fmt lint test run`.
- `docs/DECISIONS.md` entries: purity policy; `redb` instead of SQLite; the Phase 0 outcomes.

**Acceptance:** everything above passes. `zoologist --help` lists the subcommands.

### Step 1.2 — Config, domain types, time helpers

**Deliverables:** `Config::load` with the §3.3 validation, `redact_url`, the §3.1 types (`BBox` helpers,
`Frame::y_plane`), `local_date_hour(ts, tz)`, `rfc3339_micros(ts)`, `config/zoologist.example.toml`, and
`zoologist check-config`.

**Acceptance tests:** the example config parses; each validation rule has a failing test; `BBox::iou`
(identical → 1, disjoint → 0, half overlap → 1/3 ±1e-6); `redact_url("rtsp://u:p@h/x") == "rtsp://***@h/x"`;
`local_date_hour` is correct across a DST change.

### Step 1.3 — YUV helpers

**Objective:** Fast, safe conversions used everywhere else.

**Deliverables** in `zoologist-core/src/yuv.rs` (done, see DECISIONS #5):
- `downscale_y(y, w, h, out_w) -> Result<(Vec<u8>, u32, u32)>`, a box filter via `fast_image_resize`. It never upscales.
- `yuv_to_rgb` / `rgb_to_yuv` (BT.601 limited range, integer maths) and `rgb_to_i420` (builds test frames).
- `PixelRect` with `aligned(frame_w, frame_h)` (clamp to the frame, even coordinates and sizes).
- `i420_rect_to_rgb(frame, rect, dst)` and `i420_to_rgb_full(frame) -> Vec<u8>` (for snapshots).
- `RgbCropper::crop(frame, region, out, dst)`: crops and resizes the Y, U and V planes separately, then converts
  only the `out × out` result to RGB. Keeps its buffers, so use **one cropper per worker thread**.
- `Letterbox` (scale and padding, with `to_input` / `to_source` mapping) for models trained with letterboxing.

**Acceptance:** solid red/green/blue/grey/black/white frames round-trip within ±2 per channel; crops come from
the requested region; edge regions are clamped; the release-mode benchmark
(`cargo test --release -p zoologist-core -- --ignored --nocapture`) shows a 360×360 → 320×320 crop under 1 ms
(measured 0.30 ms on an M-series core; re-measure on the 5700U in Step 0.3).

---

## Phase 2 — Video ingest

### Step 2.1 — retina stream task

**Status:** done (`source.rs`). Tested against MediaMTX via `scripts/fake-camera.sh`.

**Objective:** A robust per-stream RTSP reader yielding H.264 access units with wall-clock times.

**Deliverables** in `zoologist-video/src/rtsp.rs`:

```rust
pub struct AccessUnit {
    pub received_at: DateTime<Utc>,
    pub ts_90k: i64,               // stream timestamp (RTP or FLV) in 90 kHz units
    pub is_keyframe: bool,
    pub avcc: Bytes,               // length-prefixed NAL units as retina delivers them
}
pub struct StreamInfo { pub codec: String, pub width: u32, pub height: u32, pub avc_config: Bytes /* avcC */ }
pub enum StreamItem { Info(StreamInfo), Unit(AccessUnit) }

/// Connects, SETUPs the first video stream (TCP interleaved), PLAYs, and forwards items.
/// Reconnects with exponential backoff (1,2,4…30 s). Sends Info again whenever parameters change.
pub fn spawn_rtsp_stream(camera: CameraId, url: String, tx: mpsc::Sender<StreamItem>,
                         status: Arc<RwLock<StreamStatus>>, cancel: CancellationToken) -> JoinHandle<()>;
```

- Base it on `retina/examples/client` `[VERIFY]` current API (`Session::describe`, `setup`, `play`,
  `demuxed()`, `CodecItem::VideoFrame`). Credentials come from the URL (`retina::client::Credentials`).
- A stall watchdog: no access unit for 10 s → reconnect.
- Reolink RTSP is known to be flaky (Frigate recommends FLV). Keep TCP transport and check retina's `SessionOptions` for
  camera-quirk workarounds `[VERIFY]`. Record any needed options per camera in CAMERAS.md.
- Reject non-H.264 streams with a clear error in the logs and the status (`state = Unsupported("h265")`),
  with the message "set this camera's stream to H.264".
- `StreamStatus { state, fps_measured, last_unit_at, reconnects, bitrate_kbps }`.

**Acceptance:** an `#[ignore]` MediaMTX test receives ≥ 40 units in 5 s at 10 fps, and `Info` has the right
size. A unit test for the backoff sequence. Stopping MediaMTX mid-test changes the status to backoff, and it recovers when restarted.

### Step 2.2 — HTTP-FLV source (Reolink) and the `VideoSource` switch

**Status:** done (`flv.rs`, `http.rs`, `source.rs`). The HTTP part is a small `std::net` client, because a live stream needs a per-read timeout (DECISIONS #7).

**Objective:** A pure-Rust reader for Reolink's HTTP-FLV streams (more reliable than their RTSP),
producing the same `StreamItem`s as Step 2.1.

**Deliverables** in `zoologist-video/src/flv.rs` and `zoologist-video/src/source.rs`:

```rust
/// Parses an FLV byte stream incrementally. Pure function over bytes: easy to unit test.
pub struct FlvParser { /* buffer, state: Header | TagHeader | TagBody */ }
pub enum FlvItem { AvcConfig(Bytes /* avcC */), Video { dts_ms: u32, cts_ms: i32, keyframe: bool, avcc: Bytes }, Other }
impl FlvParser { pub fn push(&mut self, data: &[u8]) -> Result<Vec<FlvItem>, FlvError>; }

/// Connects with `ureq` (plain HTTP), reads the body in 16 KiB chunks on a blocking thread,
/// parses with FlvParser, and sends StreamItem::Info / StreamItem::Unit. Same reconnect/backoff/
/// watchdog/status rules as spawn_rtsp_stream.
pub fn spawn_flv_stream(camera: CameraId, url: String, tx: mpsc::Sender<StreamItem>,
                        status: Arc<RwLock<StreamStatus>>, cancel: CancellationToken) -> JoinHandle<()>;

/// Chooses spawn_rtsp_stream or spawn_flv_stream from the camera's `transport`.
pub fn spawn_source(cam: &CameraConfig, which: StreamRole, tx, status, cancel) -> JoinHandle<()>;
```

FLV format (write the parser yourself; it is small):
- File header: `"FLV"`, version `1`, flags, a u32 header size (normally 9), then `u32 PreviousTagSize0`.
- Then repeated tags: `u8 type` (8 audio, 9 video, 18 script), `u24 data size`, `u24 timestamp` + `u8
  timestamp_extended` (upper 8 bits), `u24 stream id`, the data, then `u32 previous tag size`.
- Video tag data: first byte = `frame_type (4 bits: 1 keyframe, 2 inter) | codec_id (4 bits: 7 = AVC)`; then
  `u8 AVCPacketType` (0 = sequence header → payload is the avcC, 1 = NALUs in AVCC form, 2 = end of sequence),
  `i24 composition time`, then the payload. Ignore audio and script tags. `codec_id` ≠ 7 → error "not H.264".
- `received_at` = wall clock when the tag was parsed. `ts_90k` = `(dts_ms + cts_ms) × 90`, with the 32-bit FLV clock unwrapped (for durations in the recorder).
- Credentials are in the URL query. **`redact_url` must also hide `user=` and `password=` query values** (add a test).

**Acceptance:** parser unit tests on hand-built byte vectors (header, split across `push` calls at every byte
offset, extended timestamp, a sequence header followed by NALUs, an audio tag skipped). The owner's `.flv` captures parse
completely, and the unit counts match `ffprobe -count_frames`. An `#[ignore]` live test against a real Reolink camera runs 60 s with no reconnects.

### Step 2.3 — Decoder trait and implementations

**Status:** done (`decode.rs`), with both decoders and the per-camera worker.

**Deliverables** in `zoologist-video/src/decode/`:

```rust
pub trait H264Decoder: Send {
    /// Feed one access unit in Annex-B form. Returns zero or more decoded I420 frames in output order.
    fn decode(&mut self, annexb: &[u8]) -> anyhow::Result<Vec<DecodedYuv>>;
}
pub struct DecodedYuv { pub width: u32, pub height: u32, pub i420: Vec<u8> }
pub fn avcc_to_annexb(avcc: &[u8], out: &mut Vec<u8>);        // replace 4-byte lengths with 00 00 00 01
pub fn param_sets_annexb(avc_config: &[u8]) -> Vec<u8>;       // SPS/PPS from avcC, prepended before each keyframe
```

- `RustDecoder`: wraps the crate chosen in Step 0.4.
- `FfmpegPipeDecoder` (compiled always, used only if configured): spawns
  `ffmpeg -hide_banner -loglevel error -flags low_delay -f h264 -i pipe:0 -f rawvideo -pix_fmt yuv420p pipe:1`.
  A writer thread feeds stdin and a reader thread reads exactly `w*h*3/2` bytes per frame. The width/height
  come from the SPS (`h264-reader`). Keep a FIFO of `received_at` values, since frames come out in input order (the cameras use no B-frames; verify in Step 0.4).
- A per-camera decode thread: receives `AccessUnit`s, decodes **every** unit (needed for P-frames), and
  emits a `Frame` only every `1/detect_fps` seconds of `received_at`. If the substream is wider than 640, downscale to 640 wide.

**Acceptance:** decoding the owner substream captures produces the same frame count as ffmpeg and
Y-PSNR ≥ the Step 0.4 threshold. A `detect_fps` throttle test (15 fps input, `detect_fps = 5` → 5 ± 1 frames/s).

### Step 2.4 — Latest frame and `probe`

**Status:** done (`snapshot.rs`, `zoologist probe`, `zoologist capture`).

- The latest frame per camera is held in memory, and every 5 s a JPEG (q80) goes to `data/latest/<camera>.jpg`
  via a temp file + `rename`.
- `zoologist probe --camera <id> --seconds 10` prints the codec, resolution, received fps, analysed fps,
  mean decode ms, and writes `probe_<id>.jpg`.

**Acceptance:** against MediaMTX, the probe output is correct and the JPEG decodes.

---

## Phase 3 — Motion detection

### Step 3.1 — Motion detector (Y plane)

**Deliverables** in `zoologist-vision/src/motion.rs`:

```rust
pub struct MotionDetector { /* bg: Vec<u16> (fixed point ×16), w, h, cfg, mask: Vec<bool>, frames_seen */ }
impl MotionDetector {
    pub fn new(cfg: &MotionConfig, frame_w: u32, frame_h: u32, masks: &[Vec<f32>]) -> Self;
    pub fn process(&mut self, frame: &Frame) -> Vec<BBox>;   // normalised; empty = no motion
}
```

Algorithm (keep exactly this; it is fast and deterministic):
1. `downscale_y` to `analysis_width` (the Y plane already is grayscale, so no colour conversion). 3×3 box blur.
2. No background yet → store it, return empty. Ignore the first 10 frames.
3. `changed = |gray - bg| > threshold`, AND-ed with the inverted motion mask (rasterise the polygons once in `new`).
4. Changed fraction > `lightning_fraction` → reset the background, return empty.
5. Dilate 3×3 once. Connected components via two-pass union-find (write it, ~60 lines). Keep components
   with area ≥ `contour_area × pixels`. Return their boxes (normalised).
6. Background update in integer fixed point: `bg += (gray*16 - bg) * alpha` (alpha as a fraction of 256),
   using `alpha/4` in pixels with motion.

**Acceptance:** a static scene → no boxes; the moving-square fixture → a box on ≥ 90 % of frames, moving in
the right direction; a brightness jump → empty, then quiet 2 frames later; a full mask → never any motion.
`--release` timing, logged: **< 0.5 ms per frame** at 640×360 input.

**Status:** done, using synthetic frames (the moving-square H.264 fixture needs the decoder; add that test in Step 2.3).
Measured 0.39 ms per 640×360 frame on an M-series core (`cargo test --release -p zoologist-vision -- --ignored --nocapture`).

---

## Phase 4 — Object detection (tract)

### Step 4.1 — Region selection

**Status:** done (`zoologist-vision/src/regions.rs`). One addition: an object too big for any square gets the **whole frame** as its region (not square). The detector pool letterboxes non-square regions (`Letterbox` in `zoologist_core::yuv`), so large objects are not cut in half.

**Deliverables** in `zoologist-vision/src/regions.rs`:
`fn select_regions(motion: &[BBox], tracks: &[BBox], frame_w, frame_h, model_size: u32, max: usize) -> Vec<RegionPx>`
- Cluster motion boxes whose expanded (+20 %) boxes overlap. For each cluster (largest first), make a **square**
  around the cluster's union, expanded by 20 %. Its side is at least `model_size` px (never upscale small
  crops more than needed) and at most `min(frame_w, frame_h)`. Shift it inside the frame.
- Also add a square around any track box due for a keep-alive.
- Merge squares that overlap by more than 50 %. Return at most `max` of them.

**Acceptance:** unit tests for a single box, two far-apart boxes (→ 2 regions), a box at the frame edge (shifted
inside), a box larger than the frame height (→ capped), and more than `max` clusters.

### Step 4.2 — Detector trait and tract backend

**Status:** done (`zoologist-vision/src/detector.rs`, `zoologist detect`, `zoologist bench`). The golden tests compare only confident detections (score ≥ 0.5). Weak ones move by a few hundredths because Python and Rust resize photos slightly differently, and exact model numerics are checked by `spikes/tract-bench`. The tests skip themselves when `models/` is not installed (CI).

**Deliverables** in `zoologist-vision/src/detector/`:

```rust
pub struct DetectorModel { /* Arc<tract runnable model>, input_size, kind, class map, threshold */ }
impl DetectorModel {
    pub fn load(cfg: &ModelConfig) -> anyhow::Result<Self>;                 // fixed input shape, into_optimized
    /// `rgb` is input_size × input_size × 3. Returns boxes normalised to the input square.
    pub fn detect(&self, rgb: &[u8]) -> anyhow::Result<Vec<Detection>>;    // &self: safe to share via Arc
}
```

- Pre-processing: HWC u8 → NCHW f32 / 255 into a reused `tract_ndarray::Array4` (thread-local buffer).
  If MODELS.md says the model was trained with letterboxing, squares need none (regions are already square).
- Post-processing decoders: `yolov5` (`cx,cy,w,h,obj,cls…`, score = obj×cls), `yolov8`
  (`[1,4+C,N]`, transposed), and `yolo_e2e` (`[1,N,6]` = x1,y1,x2,y2,score,cls). Class-wise NMS at IoU 0.45
  (write it; sort by score, greedy). Map classes via §3.2.
- `fn region_to_frame(dets, region: RegionPx, frame_w, frame_h) -> Vec<Detection>` maps boxes back to the full frame.
- CLI `zoologist detect --model <key> img.jpg…`: resizes the whole image to a square, prints JSON, and writes
  `img.det.jpg` with boxes drawn (`imageproc` + an embedded DejaVu Sans via `ab_glyph`).
- CLI `zoologist bench --model <key>`: the Step 0.3 benchmark, built into the product.

**Acceptance:** Rust vs `golden/<model>_320.json` per fixture image: same count, IoU ≥ 0.95, |Δscore| < 0.02.
A black image → no detections. Region mapping round-trip test.

### Step 4.3 — Detector pool

**Status:** done, in `zoologist-vision/src/pool.rs` (pure logic with an `ObjectDetector` trait, tested with a fake detector) rather than the server crate. The background queue for Hub imports is included (`submit_background`, blocking, never dropped). Non-square regions are letterboxed with `RgbCropper::crop_letterboxed`.

**Deliverables** in `zoologist-server/src/inference.rs`:

```rust
pub struct DetectJob { pub camera_id: CameraId, pub frame: Frame, pub regions: Vec<RegionPx>,
                       pub reply: oneshot::Sender<DetectResult> }
pub struct DetectResult { pub detections: Vec<Detection>, pub infer_ms: f32, pub dropped: bool }
pub fn spawn_detector_pool(model: Arc<DetectorModel>, workers: usize, capacity: usize) -> DetectorHandle;
impl DetectorHandle { pub fn submit(&self, job: DetectJob); pub fn stats(&self) -> PoolStats; }
```

- `workers` OS threads (named `detect-N`) pull from one shared bounded queue (a `Mutex<VecDeque>` + `Condvar`
  is fine, or `crossbeam-channel` if pure Rust). Each worker converts each region (`i420_crop_to_rgb`) and runs `detect`.
- When the queue is full, **drop the oldest job** (reply `dropped: true`) and count the drop per camera.
- A second **background queue** (`submit_background`, blocking when full, never dropping) for Hub clip analysis (Step 7.2).
  Workers take from it only when the live queue is empty.
- `PoolStats { queue_depth, mean_infer_ms, p95_infer_ms, drops_per_camera, busy_workers }` (a rolling 60 s window).

**Acceptance:** with a fake model sleeping 50 ms, 2 workers, and 3 producers at 10 jobs/s: the queue never
exceeds capacity, drops increase, every reply arrives, and the stats are sensible.

---

## Phase 5 — Tracking and events

### Step 5.1 — IoU tracker

**Status:** done (`zoologist-vision/src/tracker.rs`). Crops are picked **per 1-second slot** (best crop in each second, then the best `max_crops` slots). Replacing a nearby crop let the kept crop slide forward in time while an animal approached the camera, so only one crop survived.

**Deliverables** in `zoologist-vision/src/tracker.rs`:

```rust
pub struct Track {
    pub id: u64, pub label: Label, pub raw_class: String,
    pub first_seen: DateTime<Utc>, pub last_seen: DateTime<Utc>, pub last_detected: DateTime<Utc>,
    pub hits: u32, pub scores: Vec<f32>, pub bbox: BBox, pub velocity: (f32, f32),
    pub crops: Vec<BestCrop>,            // ≤ max_crops_per_event, ≥ 1 s apart, best first
    pub confirmed: bool,
}
pub struct BestCrop { pub frame: Frame, pub bbox: BBox, pub score: f32, pub quality: f32 } // quality = score * sqrt(area)
pub enum TrackEvent { Confirmed(Track), Updated(Track), Ended(Track) }
impl Tracker {
    pub fn update(&mut self, now: DateTime<Utc>, frame: &Frame, dets: &[Detection]) -> Vec<TrackEvent>;
    pub fn tick(&mut self, now: DateTime<Utc>) -> Vec<TrackEvent>;
    pub fn keepalive_due(&self, now: DateTime<Utc>, every: Duration) -> Vec<BBox>;
}
```

Algorithm: predict with velocity; build the IoU matrix (same label only); match greedily by highest IoU ≥ `iou_match`;
update the bbox, velocity (EMA 0.5), scores, and crops; unmatched detections start new tracks. Confirmed when
`hits ≥ min_hits` and `median(scores) ≥ min_event_score` (emit `Confirmed` once). `Updated` at most 1/s, or
when the best crop improves. The track ends when `now - last_detected > max_missed_seconds` (emit `Ended` only if confirmed).
Frames are `Arc`-backed, so storing crops costs no copies.

**Acceptance:** a box moving 2 %/frame → one track, confirmed at hit 3, one `Ended`; two crossing boxes of
different labels never swap; a single-frame false positive → nothing; scores [0.9, 0.3, 0.3, 0.3] → not confirmed.

### Step 5.2 — Event manager (with motion-only events)

**Status:** done, in `zoologist-vision/src/events.rs` (pure logic next to the tracker, so it is tested without the server). A motion event also ends as soon as a confirmed object appears, because the object explains the motion.

**Deliverables** in `zoologist-server/src/events.rs`: turns `TrackEvent`s and motion into
`EventUpdate::{Started, Updated, Ended}` (as in §3.4, keyed by `(camera, track_id)` until the store assigns an id).
Motion events: motion present ≥ `motion_event_min_seconds` with no confirmed track → start; no motion for 5 s → end;
then a per-camera cooldown of `motion_event_cooldown_seconds`. The snapshot is the frame with the largest motion area.
Only labels in the camera's `labels` list are emitted.

**Acceptance:** scripted scenarios (a person walking through; wind only → one motion event, then the cooldown
suppresses the next; an animal pausing for 3 s → still one event) give exactly the expected update sequence.

---

## Phase 6 — Storage, recording, clips (pure Rust)

### Step 6.1 — Store (redb)

**Status:** done (`zoologist-store`). `close_dangling_events` sets `ended_at = started_at` (the real end is unknown after a crash) and marks pending clips `Failed`. The stats cache is left to the API layer (Phase 9). Unidentified animals appear in the species stats with `common_name: None`.

**Deliverables** in `zoologist-store`: `Store::open(path)` (creates the tables, checks `schema_version`, writes
`1` on create), a `HUB_IMPORTS: TableDefinition<(&str, &str), i64>` table ((hub id, recording file name) →
event id, or -1 if discarded) so every Hub recording is imported exactly once, and async wrappers (each uses `spawn_blocking`):

```rust
insert_event(NewEvent) -> u64;   update_event(id, EventPatch);   get_event(id) -> Option<EventRecord>;
list_events(EventQuery { after_id, before_id, limit ≤ 500, order, camera, label, species }) -> Page<EventRecord>;
stats_by_label(since, camera) -> Vec<(Label, u64)>;
stats_by_species(since, camera) -> Vec<SpeciesStat>;   // common, scientific, count, last_seen, best_event_id
stats_hourly(date, tz, camera) -> [HourCounts; 24];
insert_segment(camera, SegmentRecord);   segments_between(camera, from, to) -> Vec<SegmentRecord>;
delete_segments(...); close_dangling_events(now) -> u64;
```

`list_events` walks `EVENTS` by id in either direction and applies the filters while scanning, stopping at
`limit`. The stats functions range-scan `EVENTS_BY_TIME` from `since`. Add a small LRU/TTL cache (10 s) for the stats.

**Acceptance:** temp-dir tests for every function: pagination both ways, each filter, hourly buckets across
midnight and DST in the station timezone, dangling events closed on reopen, and reopening an existing DB.

### Step 6.2 — MP4 writer and reader

**Status:** done (`mp4w.rs`, `mp4r.rs`). ffprobe reports no warnings on written files. They decode pixel-identical to the raw stream and play in Chromium (checked in the browser pane; Firefox still to check). The reader handles ffmpeg files with `moov` at the end. H.265 (`hvc1`) is written but untested until a camera needs it.

**Objective:** Write playable, browser-friendly MP4 files from access units, and read samples back out of
MP4 files (our segments and Reolink Hub recordings), in pure Rust.

**Deliverables** in `zoologist-video/src/mp4w.rs`:

```rust
pub struct Sample { pub data: Bytes /* AVCC */, pub duration_90k: u32, pub is_key: bool, pub wall_us: i64 }
pub struct SampleIndexEntry { pub offset: u64, pub size: u32, pub duration_90k: u32, pub is_key: bool, pub wall_us: i64 }
/// Writes ftyp + moov + mdat (moov first = "faststart"). Returns the index of where each sample landed.
pub fn write_mp4<W: Write>(out: W, info: &StreamInfo, samples: &[Sample]) -> io::Result<Vec<SampleIndexEntry>>;
```

- Boxes (write each as a function that returns `Vec<u8>`; write a `fn bx(fourcc, payload) -> Vec<u8>` helper
  and a `full_box(fourcc, version, flags, payload)` helper): `ftyp(isom, 0x200, [isom, iso2, avc1, mp41])`,
  `moov{ mvhd, trak{ tkhd, mdia{ mdhd(timescale 90000), hdlr(vide), minf{ vmhd, dinf{dref{url }},
  stbl{ stsd{avc1{avcC}}, stts, stss, stsc(1 chunk per sample), stsz, co64 }}}}}`, `mdat`.
- Faststart: build `moov` once with placeholder offsets to learn its size, then compute the real
  `co64` offsets (`ftyp.len + moov.len + 8 + running sample offset`) and build it again.
- Use `retina/examples/client/src/mp4.rs` (MIT/Apache-2.0) as the reference for box fields `[VERIFY]` and credit it in a comment.
- The first sample must be a keyframe (the caller guarantees this; assert with an error).
- **H.265:** if `docs/CAMERAS.md` lists any H.265-only main stream, also support `hvc1` + `hvcC` sample entries
  (the codec comes from `StreamInfo.codec`; retina's H.265 support and its `extra_data` format need checking `[VERIFY]`).
  Otherwise, record the H.264 substream for that camera (`record_url` = the sub URL) and note it in CAMERAS.md.
  H.265 clips may not play in Firefox: the UI shows a "download" link as a fallback.
- **Reader** in `zoologist-video/src/mp4r.rs`:
  `pub fn read_mp4_index(path: &Path) -> Result<(StreamInfo, Vec<SampleIndexEntry>)>`. It walks the top-level boxes
  (seek past `mdat`, so it works whether `moov` is first or last), finds the first video `trak`, and builds the sample
  list from `stsd` (avcC/hvcC, width/height), `stts` (durations), `ctts` (if present, keep composition offsets),
  `stss` (keyframes; if absent, every sample is a key), `stsc` + `stsz` + `stco`/`co64` (offsets). Timescale from `mdhd`;
  convert durations to 90 kHz. `wall_us` is filled in by the caller (file start time + pts). Fragmented MP4
  (`moof`/`trun`) is **only** needed if Step 0.5 shows the Hub produces it; return a clear error otherwise.
  `pub fn read_samples(path, entries) -> impl Iterator<Item = Result<Sample>>` reads the sample bytes by offset.
**Acceptance:** writing the owner's captured main-stream units (from Step 0.4) produces a file that
`ffprobe -v error` accepts with the correct duration (±1 frame), frame count, and resolution, and that plays
in Chrome and Firefox (manual check, noted in the PR). `ffprobe` tests are `#[ignore]` if ffprobe is missing.
A unit test checks the box sizes and nesting of a tiny synthetic file. **Round trip:** `read_mp4_index(write_mp4(x))`
returns the same samples. The reader parses every Hub recording fixture, and its sample counts match
`ffprobe -count_frames`.

### Step 6.3 — Segment recorder

**Status:** done (`recorder.rs`). The recorder hands written segments to the pipeline over a channel, so the video crate does not depend on the store. Wall-clock times are anchored **once per stream session** (not per segment), so segments join without gaps. They are re-anchored only if stream time drifts more than 0.5 s from arrival time. The `.idx` file also stores the stream parameters, so clips need only `.idx` + `.mp4`.

**Deliverables** in `zoologist-video/src/recorder.rs`: `spawn_recorder(camera, record_url, dir, segment_seconds, store, cancel)`.
- Uses its own `spawn_source` (RTSP or FLV, Step 2.2) on `record_url`. Buffers `Sample`s in memory (duration = the RTP-timestamp
  difference to the next unit).
- When `segment_seconds` have passed **and** a keyframe arrives, write the buffered samples (on a blocking thread)
  to `<dir>/<YYYYMMDD>/<first wall time>.mp4` plus a `.idx` (JSON `Vec<SampleIndexEntry>`), insert a `SegmentRecord`,
  and start a new buffer that begins with that keyframe. Create date directories as needed.
- On a reconnect or parameter change, flush the current buffer as a (short) segment first.
- Memory guard: if the buffer exceeds 64 MB (a high-bitrate camera with a long GOP), flush at the next keyframe anyway, and warn.

**Acceptance:** an `#[ignore]` MediaMTX test with `segment_seconds = 2` gives ≥ 3 playable segments in 9 s whose
wall-time ranges are contiguous (gap < 1 frame). A unit test with synthetic units checks the split points always fall on keyframes.

### Step 6.4 — Clip builder, snapshot, thumbnail

**Status:** builder, snapshot and thumbnail done (`clips.rs`). Snapshots outline the box but do not draw a text label: the UI shows the label, and this avoids shipping a font. **Clip-job scheduling** (wait until the segments are written, at most 2 at a time) moves to Step 7.1, because it needs the store and the event flow.

**Deliverables** in `zoologist-video/src/clips.rs`:
- `fn build_clip(segments: &[SegmentRecord], from: DateTime<Utc>, to: DateTime<Utc>, out: &Path) -> Result<u64>`:
  read each segment's `.idx`, pick samples from the **last keyframe at or before `from`** through the last sample
  ≤ `to`, read their bytes by offset (`File::seek` + `read_exact`), and write with `write_mp4` (same `avcC` as
  the segment; if it differs between segments, split at the change and keep only the part containing the event).
  **No decoding, no re-encoding**, so it is fast.
- Scheduling: when an event ends, run the job at `ended_at + post_capture + segment_seconds + 2 s` (all
  segments are written by then). Clip window = `[started_at - pre_capture, ended_at + post_capture]`.
  At most 2 concurrent jobs. On success, set `clip_path`/`clip_bytes`/`Ready`; on error, set `Failed` and log.
- `write_snapshot(frame, bbox, label_text, path)`: full frame (`i420_to_rgb_full`), box and label drawn, JPEG q85.
  `write_thumb(frame, bbox, path)`: square crop of the box +20 %, 320 px, JPEG q80. The snapshot is written at
  Started, and the snapshot and thumbnail are refreshed when the best crop improves (at most every 2 s). Use a pure-Rust
  JPEG encoder (`image`'s encoder, or `jpeg-encoder` if faster; measure).

**Acceptance:** from test segments, a 5 s window gives a clip whose ffprobe duration is 5 s + (distance to the
previous keyframe) ± 1 frame; a window spanning 3 segments works; a window with no segments → `Failed`.
The snapshot and thumbnail dimensions are correct.

---

## Phase 7 — End-to-end pipeline

### Step 7.1 — `zoologist run`

**Status:** done. The pipeline is in `pipeline.rs`, with per-camera analysis in `analysis.rs` and the event writer (store, pictures, clip jobs, species jobs) in `writer.rs`. Shared state for the API is in `app.rs`. `--fast-files` plays `file://` sources as fast as analysis allows and exits when they end; `tests/pipeline.rs` uses it on `tools/fixtures/fox_walk_640x360_10fps.h264` and expects an animal event with a clip and species *Vulpes vulpes* (skipped without models). Species jobs are submitted from `writer.rs`, not from a separate `species.rs`. `kind = "hub_clips"` cameras are skipped with a warning until Step 7.2.

**Deliverables** in `zoologist-server/src/pipeline.rs`:
- Startup checks: log `avx2`/`fma` detection. If missing, **exit with the message "Enable CPU type 'host' for this
  VM (see docs/PROXMOX.md)"** unless `--allow-slow-cpu`. Log the decoder backend, detector, workers, and the number of cameras.
- Per `kind = "stream"` camera: source (RTSP or FLV, detect URL) → decode thread → frames → motion → `select_regions` → `DetectorHandle::submit` →
  tracker → event manager → store + clip scheduler + species queue. Frames are handled **in order per
  camera**; while a detect job is in flight for that camera, newer frames still run motion, but only the newest
  waiting frame is kept for detection (count the skips).
- Per camera: a recorder (if `record = true`) and the latest-frame writer.
- `broadcast::Sender<ApiEvent>` for SSE.
- Graceful shutdown (SIGINT/SIGTERM): stop the streams, flush recorder buffers to segments, end active events,
  finish pending clip jobs (up to 10 s), close the store.
- A log line every 60 s per camera: received fps, analysed fps, decode ms, motion %, detect jobs, drops,
  events. Globally: detector p50/p95, queue depth, species queue depth.
- `--fast-files` for `file://` fixture cameras (read Annex-B + `.ts.json` captures as fast as possible) so
  end-to-end tests run offline.

**Acceptance:** an offline end-to-end test on an owner capture containing a person gives ≥ 1 `person` event with a
snapshot, a thumbnail, and a clip. Then 60 min on the real cameras in the VM: no panics, RSS growth < 50 MB,
correct events, and the per-camera log lines look healthy.

### Step 7.2 — Reolink Home Hub importer (battery cameras)

**Status:** done, and running against the owner's Home Hub Pro (firmware v3.3.0.466). Findings and choices:
- **Login:** this firmware uses a digest login ("Version 1"), and every later command is AES-128-CFB encrypted with anti-replay counters (`reolink_hub/crypto.rs`, checked against reference values). The classic login is kept as a fallback.
- **Downloads:** `CheckDownload`, then a GET of `cmd=download`, at about real-time speed. The search's `size` is rounded to whole MiB.
- **File format:** recordings are fragmented MP4, which `mp4r` now reads, including files cut short.
- **Clips:** the analysed sub recording (H.264) is the clip. The main recording is H.265, so `clip_codec_fallback` is reserved for now.
- **Importer (`hub_import.rs`):** it runs as one thread per Hub, with low-priority detection and full-frame tiles for the first 2 s. Each recording's events are tidied before they are stored (DECISIONS #17). Status is reported under `hubs` in `/health` and on the camera tiles.
- **Always-on cameras connected to a Hub:** these are `kind = "stream"` cameras with `hub = "…"`, which fills in the Hub's credentials. They use `h264Preview_01_sub` for both detect and record, because main is H.265.
- **Tests:** `tests/hub_import.rs` runs the whole pipeline against a fake Hub that serves a fragmented fox clip.
- **Still to do:** the 24 h battery-drain check from the acceptance list.

**Objective:** Get person/vehicle/animal/species events for battery cameras **without streaming them**, by
analysing each recording the Hub makes.

**Deliverables**
1. `zoologist-video/src/reolink_hub.rs`: a small blocking API client over `ureq` (plain HTTP). Base the exact
   JSON on the requests and responses captured in Step 0.5 `[VERIFY]` against your firmware:
   ```rust
   pub struct HubClient { base: String, user: String, password: String, token: Option<(String, Instant)> }
   impl HubClient {
       pub fn login(&mut self) -> Result<()>;   // POST /cgi-bin/api.cgi?cmd=Login  [{"cmd":"Login","param":{"User":{"userName":..,"password":..}}}]
                                                // token lease (usually ~3600 s): renew 5 min early, and on a "please login first" error
       pub fn search(&mut self, channel: u8, stream: &str, from: DateTime<Tz>, to: DateTime<Tz>)  // Tz = station.timezone -> Result<Vec<HubFile>>;
                                                // POST ?cmd=Search&token=… {"Search":{"channel":ch,"onlyStatus":0,"streamType":"sub"|"main","StartTime":{…},"EndTime":{…}}}
       pub fn download(&mut self, file_name: &str, dest: &Path) -> Result<u64>;
                                                // GET ?cmd=Download&source=<name>&output=<name>&token=…  streamed to dest.tmp, then renamed
   }
   pub struct HubFile { pub name: String, pub start: DateTime<Utc>, pub end: DateTime<Utc>, pub size: u64, pub stream: String }
   ```
   The Hub's times are in its local time zone: convert them with `station.timezone`. Never log the password or token.
   Use a 30 s timeout per call and at most 1 download at a time per Hub (Hubs are slow; do not overload them).
2. `zoologist-server/src/hub_import.rs`: one task per Hub. Every `poll_seconds`, for each `hub_clips` camera:
   `search` from `max(last_seen_end, now - lookback)` to now for `analyse_stream` (sub). Skip files already in
   `HUB_IMPORTS`, and files whose `end` is less than 10 s ago (the Hub may still be writing them). For each new file:
   - Download the **sub** file to `data/tmp/`, then build its index with `read_mp4_index`.
   - **Offline analysis:** decode every sample (Step 2.3 decoder), keep frames spaced ≥ `1/detect_fps` apart by pts,
     and run the same `MotionDetector` → `select_regions` → detector pool → `Tracker` → `EventManager` code as live
     cameras, with `captured_at = file.start + pts`. (Tracker/EventManager take `now` as a parameter, so no code
     changes are needed. That was the point of passing `now`.) Submit detect jobs with **low priority**: live
     cameras must never wait behind a Hub file. Give the pool a second, low-priority queue that workers read only
     when the live queue is empty.
     PIR clips start with the animal already in view: seed the motion background from the **first frame** and treat the
     first 2 s of frames as "motion everywhere" so the detector runs on the full frame early.
   - Events: each confirmed track becomes an event as usual. If there is no confirmed track and `no_detection = "motion"`,
     create one `motion` event spanning the file. If it is `"discard"`, record -1 in `HUB_IMPORTS`.
   - **Clip:** download the matching **main** file (same start time; match by the closest `start` within 2 s) into
     `data/clips/<date>/<event_id>.mp4`. If the main stream is H.265 and `clip_codec_fallback = "sub"`, use the sub file instead.
     All events from one Hub file share that file. Set `clip_state = Ready` right away.
   - Delete the temp sub file (or keep it as the clip, per the rule above). Record the import in `HUB_IMPORTS`.
3. Status per Hub in `/health`: `{id, state, last_poll_at, last_import_at, pending_files, errors}`.
4. CLI `zoologist hub-test --hub <id>` logs in, lists channels and today's files, and downloads the newest sub file.

**Acceptance:** unit tests for Hub time conversion and the dedupe/"still being written" rules, using recorded JSON responses
from Step 0.5 as fixtures (a tiny fake Hub made with `axum` in the test serves them and the fixture MP4s). The offline
analysis of the owner's Hub fixtures gives the expected labels per clip. Live: trigger a battery camera's PIR by walking
past it. A `person` event appears in the UI within `poll_seconds + 60 s`, with a playable clip. Leave the importer running
for 24 h and confirm the camera's battery drain is unchanged compared with the Reolink app (the importer never wakes the camera).

---

## Phase 8 — Species classification

### Step 8.1 — Classifier trait and SpeciesNet (tract)

**Status:** done (`zoologist-vision/src/species.rs`, `zoologist classify`). The decision rules copy SpeciesNet's own (`ensemble_prediction_combiner.py`, `geofence_utils.py`): the species is named above 0.8, or above `min_score` (0.65) when the detector agrees. Otherwise it rolls up to genus → family → order → class above `min_score`. A geofenced species rolls up from family level. Blank, human and vehicle never count towards a group. The taxonomy file (`speciesnet_taxonomy.txt`) is needed for roll-ups. Golden test: same top-1 as onnxruntime on every photo, and a ringtail "seen" in New York becomes "Procyonidae family".

**Deliverables** in `zoologist-vision/src/species/`:

```rust
pub trait SpeciesClassifier: Send + Sync {
    fn id(&self) -> &str;
    fn classify(&self, rgb: &[u8], size: u32, k: usize) -> anyhow::Result<Vec<(usize, f32)>>;
    fn label(&self, idx: usize) -> &SpeciesLabel;   // scientific, common, taxonomy path
    fn input_size(&self) -> u32;
}
```

- SpeciesNet pre-processing exactly as in MODELS.md. Softmax if the model outputs logits.
- Geofence: zero out labels not allowed for `station.country`/`admin1_region`, then renormalise; roll blocked
  species up to their genus/family label as SpeciesNet does `[VERIFY]`.
- Non-animal winners (`blank`, `human`, `vehicle`): no species; the event keeps its coarse label.
- `zoologist classify img.jpg…` prints the top-5.

**Acceptance:** golden test on the fixture crops: same top-1, |Δp| < 0.02. The geofence test: a species blocked for
the configured country never appears.

### Step 8.2 — Species pool and voting

**Status:** the pool and voting are done (`spawn_species_pool`: a bounded queue that drops, never blocks, and a quality-weighted average of the crops' probabilities). Submitting jobs from events is wired in Step 7.1.

**Deliverables** in `zoologist-server/src/species.rs`:
- `species.workers` threads (default 1), with a bounded queue (32). **Low priority**: detection is never starved,
  because the species pool has fewer threads than the spare cores. On Linux, optionally lower the thread's
  priority with `nice` — use a pure-Rust crate only if one exists without `unsafe` in our code; otherwise skip.
- When an animal event is confirmed, submit its first crop (a provisional species while it is live). When it ends,
  submit its remaining crops.
- Voting: per-label probabilities summed over the crops, weighted by crop quality, divided by the total weight;
  the top label wins if ≥ `min_score`; otherwise the lowest common taxon of the top-3, if any; otherwise none.
  Store it on the event and broadcast `Updated`.

**Acceptance:** a voting unit test with fixed vectors. An owner animal clip gives the expected species in the top-3.
Under load (the benchmark script from Step 11.4), detector p95 latency rises by < 20 % while species jobs run.

### Step 8.3 — BioCLIP backend (optional) `[ASK OWNER]`

BioCLIP v1 (ViT-B/16) image tower in tract → L2-normalised embedding → cosine similarity with the precomputed text
embeddings for the owner's species list → softmax (temperature from the model's `logit_scale` `[VERIFY]`) → top-k.
Select it with `species.model = "bioclip"`. Acceptance matches 8.1, with a BioCLIP golden file.

---

## Phase 9 — HTTP API

### Step 9.1 — REST endpoints

**Status:** done (`zoologist-server/src/api/`, `docs/API.md`). Differences from the table: `/health` reports the detector's `mean_ms` and `p95_ms` (the pool keeps a mean, not a median), and `/cameras` also returns `kind`. `/stats/labels` always lists all four labels, in display order, with zeros included, so the bars keep their places. `thumb.jpg` falls back to the snapshot for events without a box.

Under `/api/v1`. Errors are `{"error": "..."}` with 400/404. RFC 3339 UTC timestamps with microseconds.

| Method & path | Returns |
|---|---|
| `GET /health` | `{station, uptime_s, cpu:{avx2, fma}, decoder, cameras:[{id,name,detect:{state,fps,analysed_fps,decode_ms,reconnects}, record:{state,last_segment_at}, drops}], detector:{id, workers, p50_ms, p95_ms, queue_depth}, species:{id, queue_depth}, disk:{recordings_mb, clips_mb, free_mb}}` |
| `GET /cameras` | `[{id, name, labels, record}]` |
| `GET /cameras/{id}/latest.jpg` | JPEG, `Cache-Control: no-store` |
| `GET /events?after_id&before_id&limit&order&camera&label&species` | `{items, next_after_id, next_before_id}` |
| `GET /events/{id}` | Event |
| `GET /events/{id}/clip.mp4` | `video/mp4` with **Range** support (`tower_http::services::ServeFile`) |
| `GET /events/{id}/snapshot.jpg`, `/thumb.jpg` | JPEG |
| `GET /stats/labels?window=1h\|6h\|24h\|7d\|30d&camera` | `{window, items:[{label,count}]}` |
| `GET /stats/species?window&camera` | `{window, items:[{common_name, scientific_name, count, last_seen, best_event_id}]}` |
| `GET /stats/hourly?date&camera` | `{date, hours:[{hour, person, vehicle, animal, motion}]}` |

The Event JSON is the `EventRecord` plus `clip_url`/`snapshot_url`/`thumb_url` (null when missing) and `active`.
`docs/API.md` has a curl example for each route.
**Acceptance:** `oneshot` tests for every route, including 400s, 404s, and a Range request → 206.

### Step 9.2 — SSE stream

**Status:** done (`api/stream.rs`). The stream ends when Zoologist shuts down, so a graceful shutdown does not wait for open browser tabs.

`GET /api/v1/stream`: `event: started|updated|ended`, `id: <event id>`, `data: <Event JSON>`. On
`Last-Event-ID`, replay events with a higher id first. Keep-alive comment every 15 s (birdsong's approach).
**Acceptance:** a subscribe → insert → receive test; a reconnect with `Last-Event-ID` receives the missed events.

---

## Phase 10 — Web UI

Template: `~/git/birdsong/static/`. **Copy its CSS variables (light and dark), card grid, segmented buttons, SVG
chart helpers (`el`, `svg`, `niceMax`, `colour`), `<dialog>` viewer, and SSE live indicator.** No framework, no
build step. Served by `ServeDir` at `/`.

### Step 10.1 — Layout and charts

**Status:** done (`static/`). The page reads the station name and time zone from a new `GET /api/v1/config`. The hourly chart is drawn at its container's width, so it stays readable on phones.

1. **Header:** station name. Status line "Watching N cameras · M events today · detector X ms". Amber/red when a
   camera is down or the detector queue keeps dropping.
2. **Filter bar** (sticky): camera select (All + each camera) and window buttons `1h 6h 24h 7d 30d` (default 24h).
3. **Activity:** horizontal bars per label (Person 🧍, Vehicle 🚗, Animal 🦌, Motion 〰️) with fixed colours. Clicking one filters Recent events.
4. **Animals:** horizontal bars per species (top 12 + "Other"; "Unidentified animal" for animals without a species). Clicking filters by species.
5. **By hour** (wide): 24 stacked columns by label for the chosen date (no future dates), with `<title>` tooltips.
6. **Recent events** (wide): a responsive grid of tiles with a lazy-loaded thumbnail, a label badge, the species, the camera,
   "14:03 · 5 min ago", the duration, and a pulsing LIVE badge while active. Filter chips (All / Person / Vehicle /
   Animal / Motion). "Load more" pages with `before_id`.
7. **Cameras** (wide): a tile per camera with `latest.jpg` (refreshed every 10 s), a detect/record state dot,
   analysed fps, and today's counts. **Hub (battery) cameras** show a 🔋 badge, the last event's snapshot instead of a live
   frame, "last import 3 min ago", and the Hub's status.
8. **Species** (wide): table with Species · Detections · Last seen · Best score · ▶ best clip.

### Step 10.2 — Clip viewer and live updates

**Status:** done. `zoologist seed-demo --config <cfg> [--fixtures tools/fixtures]` writes 200 events into an **empty** database. Their pictures are made from the fixture photos by the same snapshot and thumbnail code as the pipeline, with the boxes MegaDetector finds in each photo. Events with a clip share one clip cut from the fox fixture. `docs/UI_CHECKLIST.md` records what was checked by hand. Items that need a real camera or the Hub importer are still open.

- A tile opens a `<dialog>` with a title (label/species), metadata (camera, start, duration, score, top-3 species %),
  `<video controls autoplay muted playsinline>` for the clip, and the snapshot below. `Pending` → show the snapshot and
  "Clip is being prepared…", and poll every 3 s. `Failed`/`Purged` → "Clip not available". Esc/× closes it and
  pauses the video; ←/→ go to the previous/next event.
- `EventSource('/api/v1/stream')`: `started` adds a tile at the top (briefly highlighted); `updated`/`ended` replace
  the tile; chart refreshes are debounced to at most one every 5 s; live indicator as in birdsong.
- No horizontal page scroll at 400 px; readable in dark mode; keyboard reachable with a visible focus ring; `aria-label`s on the charts.

**Deliverables:** `static/*`, `docs/UI_CHECKLIST.md` (one checkbox per behaviour above), and
`zoologist seed-demo` (~200 fake events across labels, cameras, species, and hours, using fixture images as thumbnails).
**Acceptance:** every checklist item passes on seeded data, with no console errors.

---

## Phase 11 — Operations and performance

### Step 11.1 — Retention janitor

**Status:** done (`janitor.rs`, DECISIONS #16). It runs 30 s after startup, then every 10 min. `zoologist janitor --config … [--dry-run]` runs one pass by hand.

Every 10 min: delete segments older than `keep_segments_hours` unless a `Pending` clip needs them; purge
clips/snapshots/thumbnails older than `clips_days` (set `Purged` and null the paths; keep the event); enforce
`clips_max_total_mb` oldest-first (motion events before others of the same age); remove empty directories. Also log a
warning when free space on `data_dir` is < 5 %.
**Acceptance:** temp-dir tests per rule, plus a dry-run mode that lists what would be deleted.

### Step 11.2 — `healthcheck` subcommand

**Status:** done (`healthcheck.rs`).

`zoologist healthcheck [--url http://127.0.0.1:8090/api/v1/health]`: a minimal HTTP/1.0 GET over
`std::net::TcpStream` (no HTTP client crate). Exit 0 on `200`, otherwise 1. Used by the Docker `HEALTHCHECK`,
because distroless images have no curl.

### Step 11.3 — Docker image and Compose

**Status:** `Dockerfile`, `docker-compose.yml`, `.dockerignore` and `docs/PROXMOX.md` are written. The build uses BuildKit cache mounts instead of cargo-chef (DECISIONS #15). Compose uses a named volume for `/data` by default. Built and run on the dev machine (linux/arm64): the container becomes `healthy`, detects the fox fixture with clip and species, and survives `docker restart` with its data. Debug line tables are stripped by default (`--build-arg STRIP=none` keeps them for `perf`). The image is 21 MB compressed and about 76 MB unpacked (36.5 MB binary plus the distroless base), so the "< 60 MB" target holds for the download, not on disk. **Still to do:** `docker compose up` in the VM (linux/amd64).

**Dockerfile** (multi-stage):
```dockerfile
FROM rust:1-bookworm AS build
ARG TARGET_CPU=""            # optional: x86-64-v3
WORKDIR /src
COPY . .
RUN if [ -n "$TARGET_CPU" ]; then export RUSTFLAGS="-C target-cpu=$TARGET_CPU"; fi; \
    cargo build --release --locked -p zoologist-server && \
    cp target/release/zoologist /zoologist

# Default: pure-Rust decoder → no ffmpeg needed
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=build /zoologist /usr/local/bin/zoologist
COPY static /app/static
ENTRYPOINT ["/usr/local/bin/zoologist"]
CMD ["run", "--config", "/config/zoologist.toml"]
HEALTHCHECK --interval=30s --timeout=5s CMD ["/usr/local/bin/zoologist", "healthcheck"]

# Only if Step 0.4 chose decoder = "ffmpeg"
FROM debian:bookworm-slim AS runtime-ffmpeg
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /zoologist /usr/local/bin/zoologist
COPY static /app/static
USER 65532
ENTRYPOINT ["/usr/local/bin/zoologist"]
CMD ["run", "--config", "/config/zoologist.toml"]
HEALTHCHECK --interval=30s --timeout=5s CMD ["/usr/local/bin/zoologist", "healthcheck"]
```
Use `cargo-chef` or a dependency-only layer to cache builds `[VERIFY]`. `static_dir` is configurable (default `/app/static`).

**docker-compose.yml**
```yaml
services:
  zoologist:
    build: { context: ., target: runtime }        # or runtime-ffmpeg
    image: zoologist:latest
    restart: unless-stopped
    init: true
    ports: ["8090:8090"]
    environment: { TZ: "America/New_York", RUST_LOG: "info" }
    volumes:
      - ./config:/config:ro
      - ./models:/models:ro
      - /srv/zoologist/data:/data                 # the VM's separate recordings disk
    logging: { driver: json-file, options: { max-size: "10m", max-file: "3" } }
    # Optional: cap CPU so the VM stays responsive
    # cpus: "10"

  # Optional sidecar for Wyze cameras without RTSP firmware (Python; separate container, not linked into zoologist).
  # Use the maintained fork chosen in docs/CAMERAS.md. Cameras then use rtsp://wyze-bridge:8554/<name>.  [VERIFY]
  # wyze-bridge:
  #   image: <fork image>:<pinned tag>
  #   restart: unless-stopped
  #   environment: { WYZE_EMAIL: "...", WYZE_PASSWORD: "...", API_ID: "...", API_KEY: "..." }
  #   ports: ["8554:8554"]

  # Optional restreamer for cameras that allow only 1-2 clients (e.g. Wyze RTSP firmware).
  # mediamtx:
  #   image: bluenviron/mediamtx:<pinned tag>
  #   restart: unless-stopped
  #   volumes: ["./config/mediamtx.yml:/mediamtx.yml:ro"]
```
The data directory must be writable by uid 65532: `chown -R 65532:65532 /srv/zoologist/data`.

**Acceptance:** `docker compose up -d` in the VM → the UI loads, cameras stream, the health check is `healthy`,
the image is < 60 MB (distroless variant), and `docker compose restart` recovers cleanly.

### Step 11.4 — Performance pass on the 5700U

**Status:** `scripts/perf-log.sh` is written. The measurements need the 5700U.

1. `scripts/perf-log.sh`: curls `/health` every minute into a CSV for 24 h. Also record `docker stats` and the VM's CPU from the Proxmox graphs.
2. **Targets:** average VM CPU < 40 %; per-camera drops < 5 %; detector p95 < 150 ms; event → clip ready
   < `post_capture + segment_seconds + 10 s`; UI load < 1 s.
3. If over budget, apply **one lever at a time**, re-measure, and record each in DECISIONS/PERFORMANCE:
   substream fps 5 → 3 (in the camera UI) → `max_regions_per_frame = 1` → a smaller MD variant → tune `inference.workers` →
   the `TARGET_CPU=x86-64-v3` build → raise cTDP to 25 W on the host → **(owner approval)** the `ort` feature.
4. `perf record`/`cargo flamegraph` in the VM for the top 3 hot spots (the release profile keeps line tables).
5. Accuracy tuning with a week of events: per-camera `motion_mask`, `min_event_score`, `min_hits`.

**Acceptance:** the targets are met, or the owner accepts documented trade-offs.

### Step 11.5 — `ort` feature (only with owner approval) `[ASK OWNER]`

Add `DetectorBackend::Ort` behind cargo feature `ort` (off by default; `check-pure-rust.sh` runs without it).
Same trait, same goldens. Build it in a separate Docker target. Only if Step 11.4 shows tract cannot meet the targets.

---

## Phase 12 — Follow-ups (each becomes its own ticket later)

1. Zones (named polygons; events record the zones they entered; UI filter).
2. Notifications: MQTT (`rumqttc`, pure Rust) with Home Assistant discovery; ntfy/Gotify webhooks (pure-Rust HTTP client without TLS or with `rustls` + a pure-Rust crypto provider `[VERIFY]`).
3. Review and relabel in the viewer (false positive / correct species), stored for threshold tuning and training data.
4. Local fine-tuned classifier (MobileNetV3/EfficientNet-B0) on the owner's labelled crops, run in tract.
5. Camera audio → birdsong's pipeline for sound-based bird IDs in the same dashboard.
6. **Live battery Reolink cameras:** a `bairelay` (pure-Rust Baichuan bridge) or Neolink sidecar that wakes the camera on PIR
   and exposes RTSP only while there is motion, which gives lower latency than Hub import. Evaluate `bairelay-neolink-core` as an
   in-process library (check the licence: Neolink is AGPL-3.0) `[VERIFY]`.
7. Use Reolink's own AI/motion events (Hub/camera API or ONVIF events) only as a *hint* to run the detector sooner, never as the answer.
8. iGPU inference with `burn` + `wgpu` (Vulkan). Needs Vega 8 passthrough into the VM (hard on this APU; see STACKS §7) or a move to LXC.
9. Daily digest with the best clip per species.

---

## Appendix A — Definition of done (every step)

- [ ] fmt, clippy `-D warnings`, tests, `check-pure-rust.sh`, and `cargo deny` pass.
- [ ] No `unsafe` (guaranteed by `forbid`), no new FFI crates.
- [ ] Public items documented; new behaviour tested.
- [ ] DECISIONS/API/MODELS/PERFORMANCE docs updated where touched.
- [ ] No credentials in logs, fixtures, or commits. The commit follows §0 rule 10.

## Appendix B — Glossary

- **Substream / main stream:** a low-resolution stream for analysis and a high-resolution stream only recorded (never decoded).
- **Access unit:** one compressed video frame (one or more NAL units). **Keyframe (IDR):** decodable on its own; segments and clips start at one.
- **AVCC vs Annex-B:** two ways of framing H.264 NAL units (4-byte length prefix vs `00 00 00 01` start codes). MP4 uses AVCC; most decoders take Annex-B.
- **I420:** YUV 4:2:0 planar: a full-resolution Y (brightness) plane and quarter-resolution U and V planes. Motion uses only Y.
- **Faststart:** `moov` before `mdat`, so browsers can start playing before the whole file downloads.
- **NMS:** removes overlapping duplicate boxes. **IoU:** intersection ÷ union of two boxes.
- **Track / Event:** the same object across frames / a confirmed track (or motion period) stored with a clip.
- **Geofence:** species allowed at the station's location.
- **CPU type `host` (Proxmox):** passes the real CPU features (AVX2/FMA) to the VM; required for fast inference.
