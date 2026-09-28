# Zoologist

Zoologist watches the cameras around your home and keeps a diary of what went past: a person walking
up the drive, a car arriving, a fox crossing the yard at 3 a.m., or just the wind in the trees. For each
visit it saves a short video clip and a snapshot, names the animal's species when it can, and shows it
all on a web dashboard. The dashboard has activity charts by hour and by species, a list of recent
events, and live views of the cameras.

It runs entirely on your own machine. Nothing is sent to a cloud service, no account is needed, and
the cameras only have to be reachable on your local network.

## How it works

1. **Watching.** Each camera's stream is read over RTSP or HTTP-FLV and decoded in software. A Reolink
   Home Hub is supported too: always-on cameras are watched live through the Hub, and battery cameras'
   recordings are downloaded and analysed after each trigger, so the camera is never woken up.
2. **Motion first.** A light motion detector looks at every analysed frame (5 per second by default) and
   only sends the parts that changed to the object detector. A still scene costs almost nothing.
3. **Finding things.** [MegaDetector](https://github.com/agentmorris/MegaDetector) looks for people,
   vehicles and animals in those regions. It is a model trained on millions of camera-trap images, so
   it copes with night-time infrared, rain, and animals half hidden in grass.
4. **Following them.** A tracker follows each object from frame to frame and turns a visit into one
   event. A parked car does not become an event at all: vehicles have to actually move, and anything
   that stops for a minute ends its event quietly. Movement with no object in it becomes a "motion"
   event, which you can turn off per camera.
5. **Naming animals.** The best views of each animal go to
   [SpeciesNet](https://github.com/google/cameratrapai), Google's camera-trap classifier (about 2,500
   species and groups). A location filter keeps the answers to animals that live in your region, and
   when the model is unsure it answers with a broader group ("bird", "deer family") instead of guessing.
6. **Keeping the evidence.** Each camera is recorded continuously into short segments kept for a few
   hours. When an event ends, its clip is cut from those segments, with a few seconds before and after.
   Old clips are removed by age and by total size, and the events themselves are kept.

Everything is written in Rust with pure-Rust dependencies: no FFmpeg, no OpenCV, no ONNX Runtime and no
`unsafe` code in Zoologist itself. The models run in [tract](https://github.com/sonos/tract), H.264 is
decoded by [rusty_h264](https://github.com/remade-with-rust/rusty_h264), and events are stored in
[redb](https://github.com/cberner/redb). The result is a single program in a small Docker image.

## Cameras

| Camera | How Zoologist gets the video |
|---|---|
| Reolink wired cameras | RTSP or HTTP-FLV, straight from the camera |
| Reolink cameras on a Home Hub (always on) | RTSP through the Hub, using the Hub's login |
| Reolink battery cameras on a Home Hub | the Hub's recordings, downloaded and analysed after each trigger |
| Any RTSP camera with H.264 | RTSP (e.g. thermal/colour cameras, Wyze with RTSP firmware or docker-wyze-bridge) |

Video must be H.264; many cameras offer it on their sub stream when the main stream is H.265. The
`onvif` and `probe` commands (below) show what a camera sends.

## Installing with Docker

You need a 64-bit Linux machine with Docker and Docker Compose, on the same network as the cameras.
The processor must support AVX2, which any x86-64 processor from the last several years does.
Plan for about 1.5 GB of memory, about one CPU core for three or four cameras, and disk space for
clips: 10–60 GB, depending on how busy your cameras are and how long you keep clips.

1. **Get the files.** Clone the repository, or copy `docker-compose.yml`, `config/zoologist.example.toml`
   and `scripts/` to a folder on the server:

   ```bash
   git clone https://github.com/chuckmcgut/zoologist.git
   cd zoologist
   ```

2. **Get the models.** The detector and species models are not in the repository (a few hundred MB, and
   their licences are listed in [`docs/MODELS.md`](docs/MODELS.md)). `scripts/fetch-models.sh`
   downloads the original weights and converts them into `models/`. It needs Python 3.12 and
   [uv](https://docs.astral.sh/uv/), and downloads about 2 GB the first time. You can run it on
   any computer and copy the `models/` folder to the server afterwards.

   ```bash
   scripts/fetch-models.sh
   ```

3. **Write the configuration.** Copy the example and edit it:

   ```bash
   cp config/zoologist.example.toml config/zoologist.toml
   ```

   - `[station]`: your time zone, country and state or region. The species filter uses them.
   - `[[cameras]]`: one block per camera, with its stream addresses. The example file shows Reolink,
     RTSP, Wyze and Home Hub cameras.
   - `[[reolink_hubs]]`: your Home Hub, if you have one.
   - `labels`: which kinds of events each camera makes. Remove `"motion"` for a view full of trees.
   - `motion_mask`: areas to ignore, such as a timestamp overlay.
   - `[retention]` and `[recording]`: how long, and how many gigabytes of, clips and recordings to keep.

   Paths may stay relative (`models/…`): inside the container they point to the mounted folders.
   Check the file with:

   ```bash
   docker compose run --rm zoologist check-config --config /config/zoologist.toml
   ```

4. **Start it.**

   ```bash
   docker compose pull
   docker compose up -d
   ```

   The dashboard is on `http://<server>:8090`. `docker compose ps` shows "healthy" after about a
   minute, and `docker compose logs -f` shows what it is doing.

The image is published as `ghcr.io/chuckmcgut/zoologist` for x86-64 each time `main` changes. If the
package is private, log in once with a GitHub token that has the `read:packages` scope:
`docker login ghcr.io -u <github-user>`. To update later, run `docker compose pull && docker compose up -d`.

By default, data goes to a Docker volume. To keep it in a folder instead (for backups, or on a bigger
disk), change the data mount in `docker-compose.yml` to that folder and make it writable for the
container's user: `sudo chown -R 65532:65532 <folder>`. [`docs/PROXMOX.md`](docs/PROXMOX.md) describes a
complete setup in a virtual machine.

## Building from source

Zoologist needs Rust 1.91 or newer ([rustup](https://rustup.rs)) and nothing else: there are no system
libraries to install. It builds on Linux and macOS, on x86-64 and ARM.

```bash
cargo build --release
./target/release/zoologist run --config config/zoologist.toml
```

With a config written for a checkout (`data_dir = "data"`, models in `models/`), the dashboard is on
`http://localhost:8090`. `make run` does the same through Cargo.

**Trying it without cameras.** A camera can be a file: set a camera's `detect_url` and `record_url` to
`file://<repo>/tools/fixtures/fox_walk_640x360_10fps.h264?fps=10`, point `server.data_dir` at an empty
folder, and fill it with demo events before the first start:

```bash
./target/release/zoologist seed-demo --config demo.toml
./target/release/zoologist run --config demo.toml
```

**Tests and checks.** `make check` runs everything CI runs: formatting, clippy, the tests, a check that
no C or C++ library has crept into the dependencies, and `cargo deny` for licences and advisories. Some
tests need the models or FFmpeg and skip themselves without them.

**Your own Docker image.** In `docker-compose.yml`, comment out `image:` and uncomment `build:`, then run
`docker compose up -d --build`. To build an x86-64 image on another kind of machine (such as an ARM
Mac), use `docker buildx build --platform linux/amd64 -t zoologist .`.

**Project layout.**

| Crate | What it does |
|---|---|
| `zoologist-core` | configuration, shared types, image helpers |
| `zoologist-video` | camera sources (RTSP, FLV, files), H.264 decoding, MP4 reading and writing, recording, clips, the Reolink Hub and ONVIF clients |
| `zoologist-vision` | motion detection, the object detector, tracking, events, the species classifier |
| `zoologist-store` | the event database, statistics and retention |
| `zoologist-server` | the `zoologist` program: pipeline, web server, API and command-line tools |

`static/` is the dashboard (plain HTML, CSS and JavaScript, no build step; built into the program), and `vendor/` holds a
patched copy of the H.264 decoder until an upstream fix is released (see its `PATCHED.md`).

## Commands

`zoologist run` is the program itself. The other commands help with setting it up and looking after
it:

| Command | Use |
|---|---|
| `check-config` | validate a config file and list its cameras |
| `onvif <host:port>` | list an ONVIF camera's streams, codecs, resolutions and RTSP addresses |
| `probe --camera ID` | connect to a camera for a few seconds and report what it sends, with a snapshot |
| `detect`, `classify` | run the detector or the species classifier on photos |
| `bench` | measure how fast the detector is on this machine |
| `replay --camera ID CLIPS…` | run saved clips through a camera's analysis, to try settings on real footage |
| `reclassify` | name the animals of stored events again from their clips (prints; `--yes` stores) |
| `prune` | delete chosen old events with their clips (lists them; `--yes` deletes) |
| `janitor --dry-run` | show what the retention rules would delete now |

`reclassify` and `prune` change the database, so stop Zoologist first. In Docker, run any command
with `docker compose run --rm zoologist <command> --config /config/zoologist.toml …`.

## More documentation

- [`docs/API.md`](docs/API.md): the HTTP API used by the dashboard.
- [`docs/MODELS.md`](docs/MODELS.md): the models, their inputs and outputs, and their licences.
- [`docs/PROXMOX.md`](docs/PROXMOX.md): running it in a virtual machine.
- [`docs/DECISIONS.md`](docs/DECISIONS.md): design decisions and the measurements behind them.
- [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md): decoder and detector speed.
- [`STACKS.md`](STACKS.md) and [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md): why these
  technologies were chosen, and how the project was built up step by step.

## Licence

Zoologist's code is MIT-licensed. The models have their own licences, listed in
[`docs/MODELS.md`](docs/MODELS.md). Check them before you redistribute the models or an image that
contains them.
