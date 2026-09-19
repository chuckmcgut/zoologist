# Zoologist — Technology Stack Survey

Candidate technology for a Rust application that watches several local RTSP cameras and detects
**people, vehicles, animals (with species), and general motion**, then shows bar charts and video
clips in a web UI in the style of `~/git/birdsong`.

`[VERIFY]` marks numbers or facts that must be measured or re-checked before relying on them. All
CPU timings in this file are **estimates** until Phase 0 of the implementation plan measures them.

---

## 0. Owner constraints

| # | Constraint | Consequence |
|---|---|---|
| C1 | **Target: AMD Ryzen 7 5700U** (Zen 2 "Lucienne", 8 cores / 16 threads, 1.8–4.3 GHz, **15 W TDP**, AVX2+FMA, no AVX-512, Radeon Vega 8 iGPU) | A laptop-class chip. Sustained all-core clocks are ~2.5–3 GHz, so plan for **~55–65 % of a desktop 5600X's throughput**. Every design choice below is about using less CPU |
| C2 | **As fast as possible** | Detector runs only on motion, on small crops at 320 px. Species model runs only a few times per animal. Parallel inference workers. Motion runs on the Y (luma) plane without converting colour. `lto = "fat"`, plus an optional `target-cpu = x86-64-v3` build |
| C3 | **Avoid `unsafe`; pure Rust where possible** | Our crates use `#![forbid(unsafe_code)]`. Dependencies are pure-Rust crates, and `cargo deny` bans `-sys`/FFI crates. Section 2 lists the places where pure Rust falls short |
| C4 | **Docker Compose inside a Proxmox VM** | VM CPU type must be `host` (otherwise no AVX2). Static or distroless image. Section 7 covers VM sizing |

---

## 0.5 The owner's cameras: how each one gets into Zoologist

| Camera | Local stream? | Recommended path (pure Rust in **bold**) | Notes |
|---|---|---|---|
| **Reolink wired (PoE/Ethernet)** | Yes: RTSP, **HTTP-FLV**, RTMP | **HTTP-FLV** for ≤ 5 MP models (`http://<ip>/flv?port=1935&app=bcs&stream=channel0_sub.bcs&user=…&password=…`); **RTSP via `retina`** for 8 MP+ or if FLV fails (`rtsp://…:554/h264Preview_01_sub`, newer firmware `Preview_01_sub`) | Frigate's docs say Reolink **FLV is more reliable than its RTSP** (dropouts, I-frame hitching on new firmware). FLV is a simple container, so a pure-Rust reader is ~200 lines over a plain HTTP stream. Turn on RTSP/HTTP in *Network → Advanced → Server settings*. Set the substream to **H.264, 640×360, 5 fps**, and the I-frame interval to 1× or 2× fps. 4K/8MP main streams are often **H.265 only**; see §2 |
| **Reolink battery (Argus etc.)** | **Not standalone.** Through the **Reolink Home Hub**, yes: RTSP per channel (`Preview_<ch+1>_main/sub`) | **Import the Hub's recordings.** Poll the Hub's HTTP API (`Login` → `Search` → `Download`), then run the downloaded clips through the same motion → detect → species pipeline offline | Streaming a battery camera continuously **drains its battery in days**. The Hub's PIR-triggered recordings are fine as a *trigger and storage*. Zoologist replaces only the Hub's poor *classification*. Latency is roughly 1–2 minutes after the event. Pure Rust: an HTTP JSON client (no TLS: enable HTTP on the Hub) plus our MP4 reader |
| Reolink battery, **live** (optional, later) | via Baichuan protocol (port 9000) | **`bairelay`** (a pure-Rust Baichuan → RTSP/MQTT bridge with a local motion-push/wake-up replacement) or Neolink (Rust + GStreamer) as a sidecar | Wakes the camera only on PIR motion. Newer and less proven: a follow-up, not v1 `[VERIFY]` |
| **Wyze wired (USB) with RTSP firmware** | Yes: official RTSP firmware (v2/v3/Pan: `rtsp://…/live`) or `wz_mini_hacks` (`rtsp://…:8554/video1_unicast` HD, `video2_unicast` SD) | **RTSP via `retina`** on the SD stream for detection and the HD stream for recording | Wyze RTSP firmware is old and often allows only 1–2 clients. If it drops, put a restreamer (MediaMTX/go2rtc) in front. H.264 |
| **Wyze without RTSP firmware** (v3/v4/OG/Pan, some outdoor) | Via **`docker-wyze-bridge`** (Python sidecar using Wyze's P2P protocol, needs a Wyze account/API key) | Bridge → local RTSP → `retina` | Not Rust, but runs as a separate container, so our binary stays pure. Forks are active in 2026 `[VERIFY]` which one to use |
| **Wyze battery** (Battery Cam Pro, Outdoor) | Mostly **no** | Bridge support varies by model, and the Battery Cam Pro was reported **unsupported** `[VERIFY]`. The bridge can surface Wyze *cloud* motion events | Treat as **best effort / out of scope for v1** unless the Phase 0 inventory shows the bridge works for your models |

**Implication for the design:** Zoologist has two kinds of camera:
1. **Continuous stream cameras** (Reolink wired, Wyze RTSP/bridge): live motion → detect → track, with our own recording.
2. **Clip-import cameras** (Reolink battery via Hub): no live stream. Each Hub recording is analysed once, and its
   file becomes the event clip.

---

## 1. TL;DR recommendation

| Layer | Recommended (pure Rust) | Fallback if the Phase 0 check fails |
|---|---|---|
| RTSP client | **`retina`** (pure Rust, used in production by Moonfire NVR) | — |
| Reolink HTTP-FLV client | **Our own FLV tag reader** over a pure-Rust HTTP client (`ureq` without TLS) | RTSP via retina |
| Reolink Hub (battery cams) | **Our own Hub API client** (Login/Search/Download) + **our own MP4 reader** | — |
| Wyze without RTSP firmware | `docker-wyze-bridge` sidecar (not Rust, separate container) → RTSP | Skip those cameras |
| Recording | **Our own MP4 writer** (plain faststart MP4, one file per ~10 s segment) fed by the stream source (no decode, no re-encode). Based on retina's `examples/client` MP4 writer | `ffmpeg -c copy` child process |
| H.264 decode (substream only) | **Pure-Rust decoder (`rusty_h264` family)**, *if* it decodes your cameras' substreams correctly and fast enough | `ffmpeg` child process fed Annex-B H.264 on stdin, raw YUV out on stdout. No FFI, no `unsafe` in our code |
| Motion | Hand-written frame differencing on the **Y plane** | — |
| Resize / colour convert | `fast_image_resize` (pure Rust, SIMD), hand-written YUV→RGB for crops only | `image` crate |
| Inference | **`tract-onnx`** (pure Rust, AVX2/FMA kernels) with a **pool of worker threads** | `ort` behind a cargo feature, **off by default** (FFI to C++, needs owner approval) |
| Detector model | **MegaDetector v1000-sorrel** (YOLO11s) at **320 px on motion crops**, chosen from measurements (docs/PERFORMANCE.md) | MegaDetector v1000-spruce (YOLOv5s), about 2× faster, less accurate |
| Species model | **SpeciesNet** classifier (EfficientNetV2-M), geofenced | BioCLIP (v1, ViT-B) zero-shot for lower CPU cost |
| Tracker | Hand-written IoU/SORT | — |
| Database | **`redb`** (pure-Rust embedded key-value store) | SQLite via `sqlx` (C library) |
| Images | `image` + `jpeg-encoder`/`zune-jpeg` (pure Rust) | — |
| HTTP / live | `axum` + Server-Sent Events + plain JS/SVG UI (birdsong's pattern) | — |
| Deployment | Docker Compose in a Debian VM. **Distroless image, no ffmpeg** if the pure-Rust decoder passes | `debian:bookworm-slim` + ffmpeg if the decode fallback is used |

This fits a 15 W laptop chip because **almost all frames never reach a neural network**. Motion
detection filters them out first. Frames that do reach the detector are cropped to the moving
region and scaled to 320×320. See §6 for the budget.

---

## 2. Pure-Rust feasibility per layer (read this first)

| Layer | Pure Rust? | Status | Notes / risk |
|---|---|---|---|
| RTSP/RTP client | ✅ | **Mature.** `retina` handles ONVIF camera quirks and runs in Moonfire NVR 24/7 | Supports H.264. H.265 support is a cargo feature (`h265`, on by default in 0.4.20; Zoologist turns it off until a camera needs it) |
| MP4 recording / clip remux | ✅ | **Feasible, moderate work.** retina's example client has an MP4 writer to adapt. `mp4-atom` / `mp4` crates exist `[VERIFY]` | Writing correct `moov`/`moof` boxes is fiddly. The plan gives exact steps and tests with `ffprobe` (dev-only) |
| **H.264 decode** | ⚠️ → looking good | **Young but promising.** On the synthetic Main and High (CABAC) fixtures, `rusty_h264-decoder` 0.16 is bit-exact with ffmpeg at 0.3–0.5 ms per 640×360 frame (DECISIONS #8). Its README still lists High-profile 8×8 CABAC residuals as unsupported, so real camera streams must be checked in Phase 0. Earlier assessment: `rusty_h264` (Constrained Baseline, `forbid(unsafe_code)`, bit-exact to openh264) and `rusty_h264-decoder` (claims Main/High with CABAC and B-slices) are new crates. Speed is unknown, probably 2–5× slower than ffmpeg's hand-written assembly `[VERIFY]` | Camera substreams are usually **Main/High profile with CABAC**, so a Baseline-only decoder is not enough. **Phase 0 tests it on your real substreams.** If it fails, use ffmpeg as a child process for decoding only. That is not linked into our binary and adds no `unsafe` to our code |
| H.265 decode | ❌ | No usable pure-Rust decoder | **Set every camera's substream to H.264.** Main streams may be H.265 for recording (no decode needed), but browsers other than Safari and recent Chrome may not play those clips. Prefer H.264 main streams too |
| Neural inference | ✅ (Rust + its own assembly) | **Mature for CNNs.** `tract` (Sonos) runs YOLO-style and EfficientNet models. Its fast AVX2/FMA kernels are **hand-written assembly inside the `tract-linalg` crate**, assembled with `cc` at build time. That is not a C library and not FFI, but the build needs an assembler/C toolchain | ~1.5–3× slower than ONNX Runtime on x86 `[VERIFY]`. Each inference uses mostly one thread, so we run **N parallel workers**. Transformer models (RF-DETR, BioCLIP ViT-L) run but are slow. Not every ONNX op is supported (birdsong found STFT missing). Phase 0 checks every model loads |
| GPU inference on the Vega 8 | ⚠️ | `burn` + `wgpu` (Vulkan) is pure Rust. `tract` has no Vulkan backend | Needs iGPU passthrough into the VM, which is hard on this APU (§7). **Not in v1.** Listed as a follow-up |
| Image resize / JPEG | ✅ | `fast_image_resize`, `image`, `zune-jpeg`, `jpeg-encoder` | These use SIMD with internal `unsafe`. That is acceptable (dependency, not our code) |
| Database | ✅ with `redb` / ⚠️ with SQLite | `redb` is pure Rust, stable (4.x in 2026), ACID | SQLite (via `sqlx` or `rusqlite`) is C. With `redb` we write the chart aggregation in Rust. Event volume is small (thousands per day), so that is easy |
| Web server | ✅ | `axum`, `tokio`, `tower-http` | No TLS needed on the LAN, so no `ring`/`aws-lc` C/assembly code |
| Time zones | ✅ | `chrono` + `chrono-tz` | — |

**Summary of what is problematic:**
1. **Video decoding** is the only layer that may have to leave Rust (an ffmpeg child process). The plan checks the pure-Rust decoder first.
2. **H.265 cannot be decoded in pure Rust.** Configure the camera substreams as H.264.
3. **Pure-Rust inference (`tract`) is slower than ONNX Runtime.** We make up for it with motion gating, 320 px crops, and parallel workers. If it is still too slow, `ort` is a one-line cargo feature, but it adds C++ over FFI.
4. **Camera-side limits, not Rust limits:** Reolink battery cameras have no stream without the Hub, and
   are analysed from Hub recordings. Wyze cameras without RTSP firmware need the (Python) wyze-bridge
   sidecar. Some Wyze battery models may not work at all. 4K/8MP Reolink main streams are often
   H.265: recording them needs `hvc1` support in our MP4 writer (no decoding needed), and those clips may
   not play in Firefox. Alternatively, record the H.264 substream for those cameras.
5. **"Pure Rust" has one build-time nuance:** tract's speed comes from assembly kernels shipped in `tract-linalg`, so the
   build image needs a C toolchain. The purity check allowlists exactly that crate.
6. **Dependencies still contain `unsafe` internally** (tokio, tract's SIMD kernels, fast_image_resize, redb). "No unsafe" applies to **our** code, enforced by `#![forbid(unsafe_code)]`. Dependencies are vetted with `cargo deny`.

---

## 3. Reference systems

### 3.1 Frigate NVR (architecture blueprint)
Open-source NVR (MIT) built on Python, ffmpeg, go2rtc, and OpenVINO/ONNX. Current release is 0.17. Ideas we copy:
- The **substream for detection**, the **main stream for recording**.
- **Motion first.** The detector runs only on motion regions, **cropped to a square around the motion** at
  320 px. This keeps small or distant objects visible to a small model.
- **Tracker** (norfair) with `min_score` / `threshold` (median score over the track) / `min_frames`.
- **Continuous 10 s segment recording**, a rolling retention window, and event clips cut from segments.
- Motion masks, zones, and handling of stationary objects.
- 0.16 added bird classification. 0.17 added locally trained **custom object/state classification** (MobileNetV2).

Hybrid alternative: run Frigate for detection and recording, and write only the Rust species classifier and
dashboard (reading Frigate's MQTT and HTTP API). That is the fastest route to results, but most of the system would not be Rust.

### 3.2 Moonfire NVR (Rust reference)
Rust NVR (GPL-3) by the author of `retina`. It has no detection. **Study its recording, segment, MP4
building, and RTSP-quirk handling.** Also read `retina/examples/client` (MIT/Apache-2.0), which writes `.mp4` files.

### 3.3 Other projects (ideas only)
Viseron, Scrypted, CodeProject.AI + Blue Iris, Agent DVR, ZoneMinder, go2rtc (restream proxy),
AddaxAI (camera-trap GUI around MegaDetector), Whos-At-My-Feeder (Frigate + bird classifier).

---

## 4. Inference runtimes (Rust)

| Runtime | Pure Rust | x86 CPU speed | Notes |
|---|---|---|---|
| **`tract-onnx`** | **yes** | Good for CNNs. Mostly single-threaded per inference, so run several `SimpleState`s on several threads sharing one `Arc` optimised model | Birdsong already uses it. Call `into_optimized()` with **fixed input shapes**. Check op support per model (Phase 0). Newer versions may offer multithreaded matmul `[VERIFY]` |
| `burn` (ndarray / wgpu backends) | yes | ndarray backend slower than tract. wgpu backend could use the Vega iGPU | ONNX import generates Rust code. Unreliable for large models. Follow-up only |
| `candle` | yes | Moderate | Models hand-written in Rust (YOLOv8 and CLIP examples exist). More code than tract |
| `ort` (ONNX Runtime) | **no** (C++ over FFI) | Best (MLAS kernels, int8) | **Optional cargo feature, off by default.** Use it only if the owner accepts FFI and Phase 0 shows tract is too slow |
| `openvino`, `tch` | no | — | Rejected (FFI, large runtimes) |

---

## 5. Models

### 5.1 Object detectors (person / vehicle / animal)

| Model | Classes | License | 5700U tract cost @320 (1 thread) `[VERIFY]` | Fit |
|---|---|---|---|---|
| **MegaDetector v1000** (redwood / cedar / larch / sorrel / spruce) | **animal, person, vehicle**, exactly our three | Upstream inference code: GPL-3.0 (spruce YOLOv5s, cedar YOLOv9c, redwood YOLOv5x6) or AGPL-3.0 (sorrel YOLO11s, larch YOLO11L). Only the Python package is MIT. Fine for private use | spruce: ~40–100 ms. larch: ~2× that | **Best fit.** Trained on camera traps including IR/night. `.pt` releases need a dev-time ONNX export. Check each variant's license and architecture |
| YOLO26n / YOLO11n / YOLOv8n (COCO) | 80 COCO classes, mapped to person/vehicle/animal | **AGPL-3.0** (fine for private home use) | ~40–100 ms | Easiest export and most example code. Coarse animal classes. Weaker on IR wildlife. YOLO26's NMS-free head may use ops tract lacks `[VERIFY]`; export without the end-to-end head if needed |
| MegaDetector v5a | same three | MIT | ~2–5 s at 1280. **Too slow** | — |
| RF-DETR Nano | COCO | Apache-2.0 | transformer, likely ~200–400 ms on tract | Too heavy for continuous detection on this CPU |
| SSD MobileNet v2 | COCO | Apache-2.0 | ~15–30 ms | Smoke test only |

**Decision method:** Phase 0 benchmarks spruce, larch, and YOLO26n/YOLOv8n in tract on the 5700U VM, then
checks accuracy on your own day and night clips.

### 5.2 Species classifiers (animal crops only, a few per event)

| Model | Output | License | 5700U tract cost `[VERIFY]` | Notes |
|---|---|---|---|---|
| **SpeciesNet** | ~2,000+ labels (species, taxa, blank/human/vehicle) | Apache-2.0 | EfficientNetV2-M @480: **~1–3 s per crop** | Trained on 65M camera-trap images. Geofence by country and state. Converted from PyTorch to ONNX at dev time. Runs in a low-priority background queue, so the cost is acceptable |
| BioCLIP (v1, ViT-B/16) | zero-shot over your species list | MIT | ~0.5–1.5 s | Lighter option. Precompute the text embeddings in Python and ship only the image tower |
| BioCLIP 2 (ViT-L/14) | zero-shot | MIT | ~3–8 s | Best zero-shot accuracy, but heavy for this CPU. Optional |
| Local fine-tune (MobileNetV3 / EfficientNet-B0) | your classes | yours | ~20–50 ms | Follow-up, once there are labelled crops |

### 5.3 Motion
Running-average background subtraction on a **downscaled Y (luma) plane**. No colour conversion,
~0.1–0.3 ms per frame. Masks for timestamps and trees. Reset the background when more than 50 % of pixels
change at once (IR switch or lightning).

### 5.4 Tracking
Hand-written IoU + constant-velocity SORT (~300 lines), with an optional ByteTrack low-score second pass.

---

## 6. Performance budget — Ryzen 7 5700U in a VM with 12 vCPUs `[VERIFY in Phase 0]`

Assumptions: **6 cameras**, substream **640×360 H.264 set to 5 fps in the camera's own settings**
(so every decoded frame is used), and motion on ~30 % of frames.

| Work | Estimate | Cores |
|---|---|---|
| RTSP receive, 6 substreams + 6 main streams (retina) | tiny | ~0.1 |
| Recording 6 main streams (MP4 remux, no decode) | tiny | ~0.1 |
| Decode 6 × 640×360 @ 5 fps (pure Rust: ~5–10 ms/frame; ffmpeg: ~1–2 ms) | 30 frames/s | 0.05–0.3 |
| Motion on Y plane, 30 frames/s | <0.5 ms each | ~0.02 |
| Detector @320 on motion crops: 30 × 0.3 = **~9 inferences/s × ~70 ms** | 630 ms/s | **~0.7** (spread over 3 workers) |
| Keep-alive detections for active tracks (1 per 2 s per track) | small | ~0.1 |
| SpeciesNet: ~3 crops per animal event, ~1 event/min, ~2 s each | 6 s/min | ~0.1 average, bursts of 1 core |
| Web / DB / JPEG snapshots | small | ~0.1 |
| **Total** | | **~1.5–2 cores average**, leaving headroom for bursts (people walking past all cameras at once) |

Levers, in order, if it is over budget: substream at 3–4 fps → a smaller MegaDetector variant →
detect only on motion crops, never full frames → fewer detector workers (lower peak latency) → the `ort` feature → an accelerator.

**Build flags:** `[profile.release] lto = "fat", codegen-units = 1` (panic stays `unwind` so a crashed
worker thread can be restarted). `target-cpu` is **not** set by default: tract and fast_image_resize pick
AVX2/FMA kernels at runtime, and a default build can still start and print a helpful error on a VM that
hides AVX2. An optional `RUSTFLAGS="-C target-cpu=x86-64-v3"` build (Zen 2 supports it) gives a few
percent more in our own loops. `[profile.dev.package."*"] opt-level = 3`, so tract runs fast in tests.

**Host tuning (optional):** if the mini-PC BIOS exposes cTDP, raising it from 15 W to 25 W gives about
20–30 % more sustained throughput. Use the `performance` CPU governor on the Proxmox host if latency matters more than power.

---

## 7. Deployment on Proxmox (VM + Docker Compose)

| Setting | Value | Why |
|---|---|---|
| Guest OS | Debian 12/13 minimal, Docker CE + compose plugin, `qemu-guest-agent` | Small and well supported |
| **CPU type** | **`host`** | The default `x86-64-v2-AES`/`kvm64` **hides AVX2/FMA**. tract then falls back to slow generic kernels, and an optional `x86-64-v3` build would crash with SIGILL. `zoologist run` checks for AVX2/FMA at startup and exits with a clear message |
| vCPUs | 12 (of 16 threads), 1 socket | Leaves 4 threads for the host and other guests. Inference workers default to 3, decode and the rest use the remainder |
| RAM | 6 GB (ballooning off) | Models ~0.4 GB, frame buffers, redb cache, page cache for clips |
| System disk | 32 GB, VirtIO SCSI single, `iothread=1`, `discard=on`, `cache=none` | — |
| Recordings disk | A **separate** virtual disk (e.g. 500 GB+), same options, mounted at `/srv/zoologist/data` | So a full recordings disk cannot fill the OS disk. Can be excluded from Proxmox backups |
| Network | VirtIO NIC on a bridge (`vmbr0`) that reaches the camera VLAN | retina uses RTSP over TCP (interleaved), so no UDP port ranges are needed |
| iGPU | **Not passed through in v1** | VM passthrough of the 5700U's Vega 8 needs `vfio-pci` binding and extracted VBIOS/UEFI ROMs for the GPU and its audio function. It has been reported to work but is fragile, and the host loses the GPU. The pure-Rust stack does not use it. (An LXC container can share `/dev/dri` easily, but the owner chose a VM) |

Optional sidecar containers in the same Compose file (not Rust, but not linked into Zoologist either):
`docker-wyze-bridge` for Wyze cameras without RTSP firmware, and MediaMTX/go2rtc as a restreamer for
cameras that allow only one client.

Compose: one service `zoologist`, `restart: unless-stopped`, `init: true` (reaps child processes if the ffmpeg
fallback is used), volumes for `data/` (rw) and `models/` and `config/` (ro), port 8090,
`TZ` set, Docker log rotation (`max-size: 10m`), and a health check through a `zoologist healthcheck`
subcommand (distroless images have no curl).

---

## 8. Storage, API, and UI (reused from birdsong)

| Concern | Choice |
|---|---|
| Database | `redb` 4 tables: `events` (id → JSON), `events_by_time` ((started_at_µs, id) → ()), `segments`, `meta` (schema version) |
| Files | `data/recordings/<cam>/<YYYYMMDD>/<ts>.mp4`, `data/clips/<date>/<id>.mp4`, `data/snapshots/…jpg`, `data/thumbs/…jpg`, `data/latest/<cam>.jpg` |
| Retention | Janitor: segments older than N hours, clips older than N days, and a disk cap |
| HTTP | `axum` 0.8, `tower-http` `ServeDir`/`ServeFile` (HTTP Range for video seeking), gzip |
| Live | Server-Sent Events with `Last-Event-ID` replay |
| Front end | Plain HTML/CSS/JS, SVG bar charts, `<dialog>` + `<video>`. Copy birdsong's `static/` design tokens |

---

## 9. Architecture options compared

| Option | Description | Verdict |
|---|---|---|
| **A. Pure-Rust-first (recommended)** | retina + our MP4 writer + pure-Rust H.264 + tract + redb. ffmpeg is used only as a decode fallback | Meets C3 as far as the ecosystem allows |
| B. Rust + ffmpeg processes | ffmpeg for ingest, decode, and recording (the earlier version of this plan) | Simpler and very robust, but more non-Rust code |
| C. Rust + `ort` | Option A with ONNX Runtime | ~2× faster inference, but C++ over FFI |
| D. Frigate + Rust companion | Frigate does the vision work; Rust does species and the UI | Fastest to working, least Rust |

---

## Sources

- Frigate: <https://github.com/blakeblackshear/frigate/releases>, <https://docs.frigate.video/configuration/custom_classification/object_classification/>
- MegaDetector: <https://github.com/agentmorris/MegaDetector/blob/main/megadetector.md>
- SpeciesNet: <https://github.com/google/cameratrapai>
- BioCLIP 2: <https://huggingface.co/imageomics/bioclip-2>
- YOLO26: <https://docs.ultralytics.com/models/yolo26>; RF-DETR: <https://github.com/roboflow/rf-detr>
- tract: <https://github.com/sonos/tract>, <https://github.com/sonos/tract/discussions/716>
- retina: <https://github.com/scottlamb/retina>
- Pure-Rust H.264: <https://lib.rs/crates/rusty_h264>, <https://crates.io/crates/h264-reader>
- ort: <https://ort.pyke.io/>
- Reolink: <https://docs.frigate.video/configuration/camera_specific/> (FLV vs RTSP),
  <https://support.reolink.com/articles/360004441753-Can-Reolink-Battery-Powered-Cameras-Work-with-3rd-Party-Software/>,
  <https://www.smartrtsp.com/guides/reolink-rtsp-onvif-model-compatibility>, <https://github.com/verheesj/reolink-api>
- Battery Reolink live bridges: <https://github.com/pacorreia/bairelay>, <https://github.com/QuantumEntangledAndy/neolink>
- Wyze: <https://github.com/IDisposable/docker-wyze-bridge>, <https://www.smartrtsp.com/guides/wyze-bridge-rtsp>
- Proxmox 5700U iGPU passthrough: <https://forum.proxmox.com/threads/amd-ryzen-5700u-7735hs-igpu-passthrough-windows-11.142811/>
