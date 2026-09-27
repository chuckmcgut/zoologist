# Running Zoologist on Proxmox

Zoologist runs as one Docker container, and Docker Compose is all it needs. On Proxmox, put it in a small Debian VM (an
LXC container works too, but Docker inside LXC needs extra settings).

## 1. The VM

| Setting | Value | Why |
|---|---|---|
| **CPU type** | **`host`** | Exposes AVX2/FMA. Without them, detection is several times slower and Zoologist refuses to start (unless `--allow-slow-cpu`). |
| Cores | 6–8 | The detector uses `inference.workers` cores while busy, plus decoding and species. |
| Memory | 4 GB | About 1 GB for models and buffers; the rest is page cache for clips. |
| System disk | 16 GB | OS, Docker and the image. |
| Data disk | a separate virtual disk (e.g. 200 GB+), mounted at `/srv/zoologist/data` | Recordings and clips; easy to grow or move. |
| Network | bridged, on the cameras' LAN (or a routed VLAN) | RTSP and HTTP-FLV must reach the cameras. |

The *CPU type* is under VM → Hardware → Processors → Type. The default `x86-64-v2-AES` hides AVX2.

Inside the VM, check it:

```bash
grep -o -w -e avx2 -e fma /proc/cpuinfo | sort -u   # should print avx2 and fma
```

## 2. Docker

Install Docker Engine and the Compose plugin from Docker's Debian repository
(<https://docs.docker.com/engine/install/debian/>), then:

```bash
sudo mkdir -p /srv/zoologist/data
sudo chown -R 65532:65532 /srv/zoologist/data   # the container runs as uid 65532
```

## 3. Zoologist

```bash
git clone <this repository> zoologist && cd zoologist
cp config/zoologist.example.toml config/zoologist.toml
nano config/zoologist.toml          # station, cameras, passwords
```

The models are exported from their original weights by `scripts/fetch-models.sh`. This needs Python 3.12, `uv` and about 2 GB,
so it is easier to run it on a desktop and copy the result into the VM:

```bash
scripts/fetch-models.sh                                   # on the desktop
rsync -a models/ vm:zoologist/models/                     # *.onnx, labels, taxonomy, geofence
(cd models && sha256sum -c --ignore-missing SHA256SUMS)     # in the VM
```

In `docker-compose.yml`, swap the `zoologist-data` volume for the data disk:

```yaml
    volumes:
      - ./config:/config:ro
      - ./models:/models:ro
      - /srv/zoologist/data:/data
```

Then:

```bash
docker compose pull                 # ghcr.io/chuckmcgut/zoologist:latest (linux/amd64)
docker compose run --rm zoologist check-config --config /config/zoologist.toml
docker compose up -d
docker compose ps                   # "healthy" after about a minute
docker compose logs -f zoologist
```

The dashboard is on `http://<vm-address>:8090`.

The image is built and pushed by GitHub Actions on every push to `main` (`.github/workflows/image.yml`). If the
package is private, log in once with a GitHub token that has `read:packages`:
`docker login ghcr.io -u <github-user>`. To update: `docker compose pull && docker compose up -d`.

## Useful commands

```bash
docker compose run --rm zoologist probe --config /config/zoologist.toml --camera driveway
docker compose run --rm zoologist janitor --config /config/zoologist.toml --dry-run
docker compose run --rm zoologist bench --config /config/zoologist.toml
docker compose restart zoologist    # open events are closed and clips finished before it stops
```

## Troubleshooting

- **"this CPU does not expose AVX2/FMA"**: set the VM's CPU type to `host` and restart the VM (a reboot inside the VM is not enough).
- **`permission denied` under `/data`**: run the `chown` above for the data directory.
- **A camera stays in `backoff`**: run `probe` for it. Reolink cameras allow only a few streams at once, so
  close other viewers or put MediaMTX in front (see `docker-compose.yml`).
- **"detector falling behind" in the status line**: see `docs/PERFORMANCE.md`. Lower the substream fps in the camera, or set
  `max_regions_per_frame = 1`, or use `md-spruce`.
