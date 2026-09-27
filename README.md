# Zoologist

Watches your security cameras and tells you what walked past: people, vehicles, animals (with the
species), or just movement. It keeps a short video clip of each event and shows everything on a simple
web dashboard with bar charts.

- Written in Rust, pure-Rust dependencies, no `unsafe` code.
- Made for a small home server (tested target: AMD Ryzen 7 5700U in a Proxmox VM, Docker Compose).
- Works with Reolink wired cameras (HTTP-FLV or RTSP), Reolink battery cameras through the Reolink
  Home Hub (their recordings are analysed after the fact), and Wyze cameras with RTSP.

**Status:** the pipeline, API and dashboard work end to end. Real-camera tuning and the Reolink Hub importer are
still to do. See [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md) for the plan and
[`STACKS.md`](STACKS.md) for why these technologies were chosen.

## Running it

```bash
cp config/zoologist.example.toml config/zoologist.toml   # edit station and cameras
scripts/fetch-models.sh                                   # exports the models (Python), see docs/MODELS.md
docker compose pull && docker compose up -d              # dashboard on http://localhost:8090
```

On Proxmox, see [`docs/PROXMOX.md`](docs/PROXMOX.md) (the VM's CPU type must be `host`). The HTTP API is described in
[`docs/API.md`](docs/API.md).

To try the dashboard without cameras, point `server.data_dir` at an empty directory and run
`zoologist seed-demo --config <file>`, then `zoologist run --config <file>`.

## Development

```bash
make check                                   # fmt, clippy, tests, pure-Rust check, cargo deny
cargo run -p zoologist-server -- check-config --config config/zoologist.example.toml
```
