# Decision log

One entry per non-obvious decision. Newest at the bottom. Format: Decision / Why / Alternatives rejected.

## 1. 2026-09-18: Pure-Rust dependencies, no `unsafe` in our code

- **Decision.** Every crate has `#![forbid(unsafe_code)]`. Dependencies must be Rust crates: no `*-sys`
  bindings to C/C++ libraries and nothing that compiles C with the `cc` crate. `scripts/check-pure-rust.sh`
  and the `cargo deny` ban list enforce this in CI. The check runs against the deployment target
  (`x86_64-unknown-linux-gnu`), so macOS-only OS bindings on a developer machine do not count.
  Two exceptions: `tract-linalg` may use `cc`, because it assembles tract's own hand-written SIMD kernels
  (assembly inside a Rust crate, not a C library, no FFI). The `ffmpeg` executable may run as a child process
  if the pure-Rust H.264 decoder fails the Phase 0 check.
- **Why.** Owner requirement: avoid `unsafe`, stay pure Rust where possible, and say so where that is not possible.
- **Rejected.** Banning every dependency that contains `unsafe` internally (impossible: std, tokio and
  every SIMD crate do). `ort`/ONNX Runtime by default (C++ via FFI; kept as an opt-in feature, plan Step 11.5).

## 2. 2026-09-18: redb instead of SQLite

- **Decision.** Events and segments live in `redb` tables with JSON values. Chart statistics are computed
  in Rust over a time-ordered index table.
- **Why.** SQLite is a C library. Event volume is small (thousands per day), so aggregating in Rust is cheap
  and simple.
- **Rejected.** `sqlx`/`rusqlite` with bundled SQLite (C). Turso/Limbo (a Rust SQLite rewrite, not stable yet).

## 3. 2026-09-18: Config parsed with `toml` directly, all problems reported at once

- **Decision.** `Config::load` uses `toml::from_str` with `deny_unknown_fields`, then `validate()` collects
  every problem into one list. No environment-variable overrides.
- **Why.** One file is easy to reason about. Reporting every mistake at once saves round trips when
  setting up many cameras. Typos in key names are caught instead of silently ignored.
- **Rejected.** The `config` crate with `ZOOLOGIST__*` env overrides (birdsong's approach). Not needed:
  the Docker setup mounts the config file.

## 4. 2026-09-18: Release profile keeps `panic = "unwind"`

- **Decision.** `lto = "fat"`, `codegen-units = 1`, `debug = "line-tables-only"`, and no `panic = "abort"`.
  `target-cpu` is not set globally.
- **Why.** With `unwind`, a panicking worker thread (decoder, detector) can be logged and restarted instead
  of stopping every camera, and the speed difference is negligible. Leaving `target-cpu` unset lets the
  binary start on a VM that hides AVX2 and print a helpful message. tract and fast_image_resize choose
  AVX2 kernels at runtime anyway.
- **Rejected.** `panic = "abort"` (the plan's first draft). A global `-C target-cpu=x86-64-v3` (kept as an
  optional Docker build argument).

## 5. 2026-09-18: YUV helpers live in `zoologist-core`; crops are resized per plane

- **Decision.** Colour conversion and cropping (`zoologist_core::yuv`) sit in the core crate, so the video,
  vision and server crates share them without depending on each other. `RgbCropper` crops and resizes the
  Y, U and V planes separately with fast_image_resize (single-channel SIMD), then converts only the small
  output to RGB.
- **Why.** Converting the region to RGB first and then resizing took 1.19 ms per 360→320 crop on an
  M-series core, over the plan's 1 ms budget. Resizing the planes first takes 0.30 ms.
- **Rejected.** Converting the whole frame to RGB once per frame (wasted work: most frames never reach
  the detector).

## 6. 2026-09-18: Phase 0 outcomes

- **Pending.** Filled in when the owner's VM and cameras are available: decoder choice (Step 0.4), detector
  and worker count (Step 0.3), camera transports (Step 0.5).

## 7. 2026-09-18: HTTP-FLV is read with a small `std::net` HTTP client

- **Decision.** `zoologist-video/src/http.rs` sends a plain HTTP/1.1 GET over `TcpStream` and
  handles chunked bodies itself (~150 lines, tested against a local server).
- **Why.** A live stream needs a timeout on each read, to detect a stalled camera within 10 s. ureq 3
  only offers whole-body timeouts, which cannot work for an endless stream.
- **Rejected.** ureq (no per-read timeout). hyper (much more code for the same result).

## 8. 2026-09-18: The pure-Rust H.264 decoder is the default (pending Phase 0 on real cameras)

- **Decision.** `video.decoder = "rust"` (`rusty_h264-decoder` 0.16, BSD-2, `forbid(unsafe_code)`) by default.
  The `ffmpeg` child-process decoder stays available as `decoder = "ffmpeg"`.
- **Why.** On the committed fixtures (Main and High profile with CABAC, 640×360), every frame decodes
  bit-exact against ffmpeg (Y-PSNR 99 dB = identical) at 0.3–0.5 ms per frame on an M-series core.
  Its README says High-profile 8×8 CABAC residuals are not supported yet, and camera content is harder
  than test patterns, so Step 0.4 must repeat this on the owner's captures before it is final.
- **Rejected.** Making ffmpeg the default before measuring.

## 9. 2026-09-18: No `-fflags nobuffer` for the ffmpeg decoder

- **Decision.** The ffmpeg decode command is `-flags low_delay -f h264 -i pipe:0 -f rawvideo -pix_fmt yuv420p pipe:1`.
- **Why.** Measured: with `-fflags nobuffer`, ffmpeg returned 40 of 100 frames from piped raw H.264.
  Without it, all 100.
- **Rejected.** The plan's original flags.

## 10. 2026-09-18: Capture format = Annex-B file + JSON index

- **Decision.** `zoologist capture` writes `x.h264` (Annex-B, SPS/PPS before each keyframe),
  `x.units.jsonl` (byte range, arrival time and timestamp per frame) and `x.info.json`.
- **Why.** Any tool (ffprobe, ffplay, the decoder comparison) can read the `.h264` file, and the
  index replays the stream with its real arrival timing for offline pipeline tests.
- **Rejected.** A custom binary format (not readable by other tools). MP4 (does not keep arrival times).

## 11. 2026-09-18: Recording time is anchored per stream session

- **Decision.** A sample's wall-clock time is `anchor_wall + (stream_ts − anchor_ts)`. The anchor is set at
  the first frame after (re)connecting, and reset only if this drifts more than 0.5 s from arrival time.
- **Why.** Anchoring each segment separately let network jitter open small gaps and overlaps between
  segments, and moved clip start times by a few milliseconds. Arrival times alone are jittery. Stream
  timestamps alone drift from the wall clock over days (camera clocks are not exact).
- **Rejected.** Arrival time per frame. One anchor for the whole run.

## 12. 2026-09-18: Hand-written MP4 writer and reader

- **Decision.** `mp4w.rs` (~300 lines) writes faststart MP4s and `mp4r.rs` reads sample tables. No MP4 crate.
- **Why.** We need one video track, a few boxes, and exact control of faststart and offsets. The reader
  has to accept Reolink Hub files, whose details we control by testing, not by trusting a crate. Checked
  against ffprobe and ffmpeg (pixel-identical decode) and in a browser.
- **Rejected.** `mp4`/`mp4-atom` crates (more surface than needed, and a writer tuned for fragmented output).

## 13. 2026-09-18: Default detector is MegaDetector v1000-sorrel at 320 px on motion crops

- **Decision.** `md_v1000_sorrel_320.onnx` (YOLO11s, classes animal/person/vehicle). `md_v1000_spruce_320`
  (YOLOv5s) is the fallback when the CPU cannot keep up.
- **Why.** On the test photos (docs/PERFORMANCE.md) the COCO models (YOLO11n, YOLO26n) miss or mislabel most
  wildlife ("horse" for a deer, nothing for a coyote or a night animal). Among MegaDetector models, sorrel's
  relative accuracy is 0.967 against spruce's 0.864, at half the speed (55 vs 29 ms on an M1 core).
  Larch is barely more accurate but 3× slower still. At 320 on a whole photo, small night animals are missed.
  At 640-equivalent pixel density they are found, and Zoologist's native-resolution motion crops provide that.
  The owner's own clips (Step 0.4/5.2) must confirm this choice.
- **Rejected.** COCO YOLO models (poor on wildlife). MegaDetector larch/cedar/redwood (too slow for a 15 W CPU).
  Note: the plan and STACKS earlier called MegaDetector "MIT". Only its Python package is. The v1000
  models' upstream inference code is GPL (YOLOv5/YOLOv9 variants) or AGPL (YOLO11 variants).

## 14. 2026-09-18: SpeciesNet "always_crop" v4.0.3a is the species model

- **Decision.** Export the `always_crop` SpeciesNet classifier to ONNX (NHWC 480×480, 2,498 labels) and ship its
  geofence file.
- **Why.** It classifies a crop around one animal, which is exactly what Zoologist has. On the test photos it is
  right every time (cat, coyote, white-tailed deer, red fox, wild turkey, and a ringtail in an infrared night
  shot that the photo's title called a deer). tract runs it at ~0.45 s per crop on an M1 core.
- **Rejected.** The "full_image" variant (expects whole camera-trap frames). BioCLIP (kept as an option).


## 15. 2026-09-19: Docker build with BuildKit cache mounts, data in a named volume by default

- **Decision.** The Dockerfile caches the cargo registry and `target/` with `RUN --mount=type=cache` instead of
  using `cargo-chef`. The runtime image is `distroless/cc-debian12:nonroot`, and `/data` is created in it,
  owned by uid 65532. Compose uses a named volume for `/data`, and `docs/PROXMOX.md` shows how to switch to
  a host directory on a separate disk.
- **Why.** Cache mounts need no extra tool and give the same incremental rebuilds on one machine. A named volume
  copies the image's `/data` ownership, so the first `docker compose up` works without a `chown`.
- **Rejected.** `cargo-chef` (one more tool to pin; its benefit is layer caching across machines, which a
  single home server does not need). A root container (not needed: the only write access is `/data`).

## 16. 2026-09-19: The janitor trims clips by day, motion first, and keeps pictures

- **Decision.** Over `clips_max_total_mb`, clips are deleted oldest local day first, and within a day motion
  events before person/vehicle/animal events. Only the clip goes; the snapshot and thumbnail stay (they are
  small). Segments that a pending clip may still need are never deleted.
- **Why.** Motion-only events are the least useful, but a busy motion day should not wipe out older animal
  clips. Keeping pictures means the event list and species table still show something for every event.

## 17. 2026-09-19: Battery cameras are imported per recording, and each recording is tidied

- **Decision.** The Hub importer analyses each finished recording (the H.264 sub stream) with the live cameras'
  code, at low priority. It keeps that file as the clip of every event found in it. Before the events are stored:
  - motion events are dropped when the recording also has an object event;
  - several tracks of one label become one event.
  An "animal" that SpeciesNet calls human or vehicle with more than 0.8 probability is relabelled
  (for every camera, not only Hub cameras).
- **Why.** On the owner's recordings (Home Hub Pro, dual-lens 1536×432 panoramas at ~14 fps), the first import
  gave 33 events for 17 recordings. Many were duplicates:
  - motion events ahead of track confirmation;
  - one car split into three tracks when it came close;
  - people that MegaDetector called "animal" on some frames.
  With these rules there are 24 events, and the ones checked by eye are right. A PIR trigger rarely shows two
  separate objects of the same kind.
- **Also.** For the first 2 s of a recording, the detector sees the whole frame in square tiles, because PIR clips
  start with the subject already in view. Detections from overlapping regions are merged when one box mostly
  contains the other, and when two labels cover the same place.
- **Rejected.** Streaming battery cameras (wakes them and drains the battery). Using the main recording as the
  clip (H.265 5120×1440 on the owner's cameras; browsers cannot play it).

## 18. 2026-09-19: Analyse frames up to 1536 wide (was 640)

- **Decision.** `video.analysis_max_width` (default 1536) replaces the fixed 640-pixel limit on analysis frames.
- **Why.** The owner's Hub cameras send 1536×432 panoramas and 896×512 frames. At 640
  wide, distant subjects shrink below what MegaDetector finds reliably, and species crops lose detail. The
  detector runs on crops around motion, so its cost barely depends on frame size. Re-importing a day of
  recordings changed two of them. A distant person that 640 called "animal" became a person. A parked trailer,
  previously too small to register, became a short false "animal" (0.68). Detector time stayed at about 77 ms
  mean, and nothing was dropped.
- **Trade-off.** Larger frames use more memory (about 1 MB per 1536×432 frame kept for snapshots and crops), and
  static objects are found more often. Motion masks or `tracking.min_event_score` handle those.

## 19. 2026-09-19: `decoder = "rust"` is confirmed on the owner's cameras (Step 0.4)

- **Decision.** Keep the pure-Rust decoder (`rusty_h264-decoder`) as the default. The Docker image needs no ffmpeg.
- **Why.** Every live stream of the owner's cameras decodes **bit-exactly** against ffmpeg: 100 % of frames and
  0 errors (docs/PERFORMANCE.md). That covers the Reolink stream through the Home Hub, and all four streams of
  the hybrid thermal camera, from 256×192 to 1280×720. It also covers a Home Hub battery recording. The slowest
  stream takes 8.2 ms per frame on an M1 core, which is within the plan's ≤ 15 ms per frame on the 5700U. The
  5700U figure still has to be measured (Step 11.4).
- **Not covered.** H.265 streams (the Hub's main recordings, the Reolink main stream) are never decoded:
  Zoologist analyses the H.264 sub streams and keeps H.264 clips.
- **Seen on the way.** The hybrid thermal camera sometimes starts an RTSP session in the middle of a fragmented
  frame ("FU-A has start bit unset"). The source reconnects within seconds: about once per stream in 20 minutes.

## 20. 2026-09-19: Parked objects end their event and start no new ones

- **Decision.** `tracking.stationary_seconds` (default 60) and `stationary_forget_minutes` (default 30). An object
  that has not moved (IoU ≥ 0.8 with where it stopped) for that long ends its event and keeps being followed
  silently. Its place is remembered, so finding it again starts no event until it really moves.
- **Why.** A truck parked next to a camera made 35 "vehicle" events in a day, all from the same box: the detector
  lost it for a few seconds, the track expired, and the next detection started a new event. With the rule, that
  camera made 0 events in the next 54 minutes. Parked objects also no longer count as "an object is active", so
  motion elsewhere still makes motion events.

## 21. 2026-09-19: SpeciesNet's non-animal classes are used together

- **Decision.** An "animal" event is relabelled when the classifier is sure it is not an animal: "human" plus
  "vehicle" above 0.8 give a person or vehicle event, and "blank" above 0.9 turns it into a motion event.
- **Why.** Real examples from the owner's cameras: sun glare through trees scored "blank" 97.7 %, and a person on
  a quad bike scored vehicle 71.7 % plus human 22.2 %. Neither passed the old rule (one class above 0.8), so both
  were stored as "unidentified animal". The threshold for "blank" is higher because a real animal in night
  infrared or heavy blur can also score as blank: a dark, blurry shape at blank 60 % stays an animal.

## 22. 2026-09-24: A vehicle is an event only once it really moves

- **Decision.** `tracking.min_movement` (default 0.2) and `require_movement` (default `["vehicle"]`). A vehicle
  track becomes an event only after its box has moved away from where it was first seen, for at least 0.6 s:
  - either the whole box shifted by 0.2 × its diagonal (both opposite edges moved the same way);
  - or it grew or shrank a lot with the same shape, its left and right edges moving apart (or together) about
    equally, which is driving towards or away from the camera.

  Edges on the picture's border are ignored. A parked object that wakes up has to move again in the same way.
  Animals and people are unaffected: a deer standing still is still an event.
- **Why.** Decision 20 was not enough. Parked vehicles close to a camera kept making events because the detector
  boxes them differently all the time: the windshield, the whole truck, the truck plus the vehicle beside it.
  Each new box looked like movement or like a new object. Measured with `zoologist replay` on the owner's saved
  clips (every vehicle event in them was checked by eye):
  - the Reolink camera, 31 clips: 41 vehicle events before, 1 after, which is the one real arrival;
  - the thermal camera's colour stream, 10 clips of a truck parked nose-on: 30 before, 2 after. The 2 come from a
    vehicle cut off by the picture's edge whose box jumps up and down.

  Checking only the box's centre removed about a third of them. Checking its edges removed nearly all.
- **Cost.** Motion events went up (13 → 25 on the Reolink camera): a parked vehicle no longer counts as "an object
  is active", so sun and shadows now make motion events there. Cameras facing trees may want `motion` removed from
  their labels.
- **Tool.** `zoologist replay --config C --camera ID [--min-movement X] CLIPS...` runs saved clips through a
  camera's analysis and prints the events. It reads only the clip files: it does not touch the database or run the
  janitor.

## 23. 2026-09-26: A stream used for detection and recording is opened once

- **Decision.** When a camera's `detect_url` and `record_url` are the same, Zoologist opens one connection and
  splits it. The recorder gets every frame. The analysis must never hold up the recording, so when it falls
  behind it skips frames up to the next keyframe, where a decoder can pick up again.
- **Why.** Two of the owner's cameras detect and record from the same stream (a Reolink sub stream at 1536×432,
  the hybrid camera's 1280×720 colour stream). Each was sent twice: double the network traffic and double the work
  for the camera. Some cameras also allow only a few clients.

## 24. 2026-09-26: Memory stays flat: clips are copied frame by frame, events are capped

- **Decision.**
  - Clips are written straight from the recording files, one frame at a time. The file's index is built from
    the segment indexes (sizes and times only), so a clip costs a few MB of memory however long it is.
  - `recording.max_event_minutes` (default 5) ends a longer event and continues it as a new one, so no clip is
    longer than that.
  - The Docker image sets `MALLOC_ARENA_MAX=2`.
  - redb's page cache is capped at 64 MB (its default is 1 GiB).
- **Why.** On the Proxmox VM the container grew to 6 GB and the VM had to be rebooted. The container used
  957 MB of its own memory 20 minutes after starting, and 1.7 GB after 25. The hybrid camera's colour stream was
  making motion events from wind in the trees, 41 in 4 hours, many 5–16 minutes long, with clips of 100–350 MB.
  Each clip was read into memory in full to be written. glibc keeps a memory pool per thread and rarely returns
  freed memory to the system, so with a dozen threads every large clip raised the container's memory for good.
- **Also.** That camera no longer makes motion events (its labels in the owner's config), like the Reolink
  camera before it.

## 25. 2026-09-26: A patched copy of the H.264 decoder, and the system allocator

- **Decision.**
  - `rusty_h264-decoder` 0.16.0 is used from `vendor/rusty_h264-decoder`, through `[patch.crates-io]`. It
    has a one-line fix for a memory leak (see `vendor/rusty_h264-decoder/PATCHED.md`). Remove the copy once
    upstream releases a fix.
  - The decoder's default `global-alloc` feature is turned off. That feature makes its `rusty_alloc` the
    allocator of the whole program. The program uses the system allocator now; the decoder's SIMD kernels
    (`asm`) stay on.
- **Why.**
  - The VM's container grew by about 110 MB a minute, to 4.6 GB of its own memory in 30 minutes. Only the
    hybrid camera's colour stream leaked: 1280×720, High profile, CABAC. Container and thermal stayed flat.
  - macOS's allocation logging traced it to one place: 851,179 live allocations after 100 s, all from
    `decode_slice_cabac_inner`. That path took a new box for every P macroblock without residual, which is
    most of a still scene. A later flush then parked each box in a reuse pool that this path never drew
    from, so the pool grew forever.
  - With the fix, memory stays flat on the same stream.
  - The decoder's own allocator took memory in 128 MB blocks and hid which code was allocating. Glibc's
    allocator also makes `MALLOC_ARENA_MAX` (decision 24) take effect.

## 26. 2026-09-28: An "animal" that never moves and cannot be named is motion

- **Decision.** The tracker records whether an object ever really moved (the same edge-based test as for
  vehicles, decision 22), for every label. When an animal event ends and the species classifier has no
  confident answer, an animal that never moved is stored as **motion**
  (`species.still_unnamed_as_motion`, on by default). Named animals are kept, moving or not (a bird on a
  branch). The event is relabelled, not deleted. `reclassify` applies the same rule, and also treats a
  clip in which a second look finds no animal as motion.
- **Why.**
  - The owner marked 15 of the Reolink camera's night-time "animals" as "nothing". They came from five
    spots, each giving exactly the same box every time: a stump, a dark bush, reflections by a tarp.
  - Re-analysing all 60 of that camera's "animal" clips found no animal in any of them. The classifier
    had no confident answer for 23, called 4 "blank", and the detector did not find the others again.
  - Replayed with the rule (`zoologist replay --species`), 35 of 37 animal events became motion.
  - The two left, and two "person" events, are insects flying close to the infrared light. They move,
    so this rule keeps them.
  - A walking fox, and the two crows the owner confirmed, stayed animals ("Red fox"; "Bird").
- **Limits.**
  - A real animal that stands still for its whole visit and cannot be named is stored as motion. It is
    still in the list, under Motion.
  - Crows at 86×48 pixels on a 1536×432 sub stream can only be named "Bird": the classifier's own
    candidates split between American crow and common raven.

## 27. 2026-09-29: A strict second opinion on people; insects need a camera fix

- **Decision.** Person events go to the species classifier too (`species.check_people`, on by default).
  Only "surely nothing there" changes them: "blank" of at least 0.95 and "human" under 0.02 makes the
  event motion. Any other answer leaves the person as it is.
- **Why so strict.** All 7 "person" events of the Reolink camera on the server were false: 5 insects
  flying through the infrared light, a gas cylinder and a jacket. But on 82 saved clips of real people
  (Reolink and hybrid colour camera), the classifier's averaged answer was up to 0.88 "blank" for a real
  person: a partial or blurred view, a person in infrared at night. So any threshold below 0.9 removes real
  people, and 0.95 keeps a margin. With it:
  - none of the 82 real people is touched;
  - the clearest insect (0.975 blank, 0.002 human) becomes motion;
  - a fainter insect (0.69 blank) and the gas cylinder (0.75 human) stay "person".
  Sharpness and brightness did not separate insects from people either: every infrared picture is soft.
- **Insects.** These are an infrared problem: a camera's own infrared lights attract insects and light
  them up right in front of the lens. The reliable fixes are outside Zoologist: an external infrared
  light away from the camera, or the camera's infrared off with some visible light.

## 28. 2026-09-30: Sharp views of animals from the main streams

- **Decision.** `species.snapshots` (on by default). The species classifier also gets views of each animal
  from the camera's main stream, which is H.265 and several times sharper:
  - **Live cameras behind a Reolink Home Hub** (the Reolink camera here). While an animal is in view, up to
    3 snapshots (`cmd=Snap`, `snapType=main`) are taken, 4 s apart, when the event starts and when its best
    view improves. These are 7680×2160 JPEGs, where the sub stream is 1536×432, and take about 2 s each.
    The area 2.5 × the animal's box is cut out, the detector finds the animal in it again, and that crop is
    voted with twice the weight. This is pure Rust: the Hub decodes its own stream.
  - **Battery cameras** (Hub recordings). When a recording has an animal, the main recording (H.265,
    5120×1440 or 3840×2160) is downloaded. ffmpeg takes the frames of the animal's best views, which are
    handled the same way. This needs ffmpeg, so there is a second image, `…-ffmpeg`. Without ffmpeg,
    animals are named from the sub stream as before.
- **Why.** Species names stayed coarse: a crow 86×48 px was "Bird", a deer "Cervidae family" (white-tailed
  deer 55 %). The resolution changes the owner made in the Reolink app only apply to the H.265 main
  streams; the H.264 sub streams keep a fixed size. The main snapshot shows exactly the sub stream's view
  (correlation 0.94, no offset), and text on a box 20 m away becomes readable. On the owner's deer
  recording, two sharp views from the main recording turned "Cervidae family" into "White-tailed deer".
- **Limits.** Battery cameras are never asked for snapshots, because that would wake them. Snapshots of a
  live camera take about 2 s, so a fast animal may have moved; the detector looks for it in an area 2.5 ×
  its box, and a snapshot without the animal is ignored.

## 29. 2026-10-01: Learn from the "Wrong" button: spots marked "nothing"

- **Decision.** `species.learn_from_marks` (on by default). When a person or animal event ends and the
  object never moved, its box is compared with the events of the same camera that someone marked as
  "nothing there". If the boxes overlap by half (IoU at least 0.5), the event is stored as motion and not
  checked further.
- **Why.** The same objects kept coming back as the same "person" or "animal", in exactly the same box:
  an upside-down blue camp chair (three times), a gas cylinder, a green electrical box. The owner marked
  them, but the marks taught Zoologist nothing. Of the Reolink camera's 67 remaining person and animal
  events, 41 overlap another "nothing" mark. 22 of those were marked themselves, and the other 19 are
  the night-time stump and dark-patch "animals" already known to be false.
- **Safety.** Only objects that never moved are affected: a person who walks to the chair's spot moved on
  the way. Only "nothing there" marks count; a mark saying "it was a person" does not. Marks are
  per camera.


## 30. 2026-10-02: A walk test: demoted events and vehicles on the road cameras

The owner drove in and walked past all five cameras. Two things were wrong.

- **Decision 1.** When a person or animal event turns out to be nothing (the classifier's second opinion,
  the still-and-unnamed rule, or a spot marked "nothing"), it becomes motion only on a camera whose
  `labels` include motion. On any other camera the event is removed, with its snapshot, thumbnail and
  clip, and the dashboard takes it out of the list (a `removed` message on the live stream).
- **Why.** The Container camera has no motion events, because sun and shadows move there all day. The
  label filter only ran when a track started, so the power meter (three times in three minutes) and a
  second, "animal" track of the owner standing by the truck still appeared as motion.
- **Limit.** "Name again" and the `reclassify` command still store such an event as motion: someone asked
  about that event, so it is not deleted under them. A Hub recording is not deleted either, because it is
  the clip of every event found in it.

- **Decision 2.** A camera can have its own `require_movement`, replacing `tracking.require_movement`.
  `require_movement = []` is for a camera that looks at a road where nothing parks.
- **Why.** Both battery cameras stored the truck as motion. On Garden Fork it drives straight at the
  camera and fills the picture within two seconds; on Garden Road the camera woke late and saw the truck
  for 0.6 s. The detector scored it 0.90 to 0.95 in both, and the movement rule rejected it. Replayed with
  the rule off for those cameras, both recordings give a vehicle event.
- **Not done.** Loosening the rule itself. The box of the owner's parked truck was logged growing sevenfold
  between two detections, as much as this truck's approach, so any looser rule brings the parked-truck
  events back on the cameras that face it.
