# Performance measurements

## 2026-09-18: tract on an Apple M1 Max (development machine, not the target)

`spikes/tract-bench/target/release/tract-bench --iters 10 --threads 1,4 models/*.onnx`

| Model | max rel. diff vs onnxruntime | 1 thread p50 | 4 threads, inferences/s |
|---|---|---|---|
| yolo26n_coco_320 | 5.7e-6 | 21.7 ms | 163 |
| yolo11n_coco_320 | 4.3e-6 | 28.0 ms | 133 |
| md_v1000_spruce_320 | 1.2e-5 | 29.2 ms | 124 |
| md_v1000_sorrel_320 | 9.9e-7 | 55.3 ms | 65 |
| md_v1000_larch_320 | 9.2e-7 | 158 ms | 15 |
| md_v1000_spruce_640 | 4.5e-6 | 113 ms | 29 |
| md_v1000_sorrel_640 | 1.0e-6 | 228 ms | 15 |
| speciesnet (480) | 2.0e-6 | 446 ms | 8 |

Throughput scales almost linearly up to 4 workers. The planned target was a Ryzen 7 5700U; the production
machine is now an i5-12600H (next section).

## 2026-11-18: detector speed on the production machine (Intel i5-12600H)

The Zoologist VM moved from the Ryzen 7 5700U to an **Intel Core i5-12600H** (Alder Lake: 4 performance cores
with 2 threads each, plus 8 efficiency cores, so 16 threads). The VM sees all 16 threads, with AVX2, FMA and
AVX-VNNI; Intel disables AVX-512 on this chip. The 5700U was never benchmarked before the move.

Run inside the live container (so the pipeline was running alongside, at a load average of about 0.3) with the
published generic x86-64 image (`sha-9c87f43`):

```sh
docker compose exec -T zoologist /usr/local/bin/zoologist bench --model md-sorrel --iterations 20 --threads 1,2,4,8,12
```

| Workers | sorrel 320: p50 ms | p95 ms | detections/s | sorrel 640: p50 ms | p95 ms | detections/s |
|---|---|---|---|---|---|---|
| 1 | 98.9 | 103.4 | 10.1 | 401 | 421 | 2.5 |
| 2 | 103.9 | 107.5 | 19.0 | 415 | 444 | 4.8 |
| 4 | 107.1 | 114.4 | 36.0 | 436 | 446 | 9.1 |
| 8 | 112.1 | 234.5 | 47.3 | 462 | 941 | 11.0 |
| 12 | 133.8 | 262.5 | 54.6 | 679 | 1137 | 12.8 |

The live pipeline agrees: after 3.8 hours, `/health` reported a detector mean of 103 ms and p95 of 113 ms, with no
dropped frames or detector jobs. Decoding took 0.80 ms per frame for the Reolink container camera and 0.38 ms for
the NC200 thermal substream. The NC200 colour stream was down, so it could not be measured.

Findings:
- **Up to 4 workers, throughput scales linearly** and p95 stays flat. With 8 or more, p95 doubles while throughput
  grows by only another 30–50 %. The likely cause is that the extra workers land on efficiency cores or on the
  second thread of a busy performance core.
- **One sorrel-320 inference takes 99 ms, 1.8× the M1 Max's 55 ms.** This is slower than expected for this chip.
  Possible causes, none checked yet: the host's power limit or CPU governor, the VM's threads running on
  efficiency cores, or the generic x86-64 build (tract chooses its AVX2/FMA kernels at runtime, but the code
  around them is compiled for plain x86-64).
- **Live demand at worst:** three streaming cameras × `detect_fps = 5` × `max_regions_per_frame = 2` = 30
  regions/s when all of them see motion at once. 3 workers manage about 27/s and **4 workers about 36/s**, so 4 is
  the setting that covers the worst case.
- **sorrel 640 is too slow for live streams** (9 per second with 4 workers). It suits Hub imports, which run in
  the background queue and can wait.
- **`detect_fps = 10` on every camera** would double the worst case to 60 regions/s, more than even 12 workers
  manage. Raise it on one camera at most.
- The plan's target of a detector p95 below 150 ms (Step 11.4) is met.

## 2026-09-18: detection quality on the test photos (whole photo stretched to S×S)

| Photo | sorrel 320 | sorrel 640 | spruce 320 | spruce 640 | larch 640 | yolo26n 640 (COCO) |
|---|---|---|---|---|---|---|
| deer | animal | animal | animal | animal | animal | "horse" |
| fox | animal | animal | animal | animal | animal | "horse" |
| coyote (camera trap) | animal | animal | animal | animal | animal | "bird", "umbrella" |
| ringtail, night | – | animal | animal | animal | animal | "person" |
| ringtail, night IR (small) | – | animal | – | animal | animal | "baseball bat" |
| cat | animal | animal | – | animal | animal | "bird" |
| turkey | animal | animal | animal | animal | animal | bird |
| person | person | person | – | person | person | person |
| car | vehicle | vehicle | vehicle | vehicle | vehicle | truck |
| bird at feeder (close-up) | animal | "person" | "person" | animal | animal | bird |

Findings:
- The COCO models are not usable for wildlife: they miss or mislabel most animals.
- At 320 on the **whole** photo, small animals are missed. At 640-equivalent pixel density all
  MegaDetector models find them. Zoologist's motion crops keep the camera's native pixel density (a 320 px
  crop from a 640 px wide substream), which matches the 640 whole-photo case at a quarter of the cost.
- Close-up feeder birds confuse MegaDetector (it is trained on camera traps). A feeder camera may need a
  COCO or bird model later (follow-up).

SpeciesNet top-1 on the sorrel-640 animal crop: domestic cat 1.00, coyote 0.99, white-tailed deer 1.00,
red fox 0.98, ringtail 0.99 and 1.00 (both night photos), wild turkey 0.77.

## 2026-09-19: H.264 decoding on the owner's cameras (Step 0.4)

30 s captured from each live stream with `zoologist capture`, plus one Hub battery-camera recording. Each input is
decoded from the same Annex-B bytes by `rusty_h264-decoder` and by ffmpeg (the reference). Run it again with:

```sh
cargo test --release -p zoologist-video -- --ignored --nocapture decoder_report
```

Measured on the Apple M1 Max, one thread. The captures stay in `tools/fixtures/owner/`, which is not in git.

| Stream | Size | fps | Bitrate | Keyframe every | Frames (rust/ffmpeg) | Errors | ms/frame | min Y-PSNR |
|---|---|---|---|---|---|---|---|---|
| Reolink via Home Hub RTSP, sub | 1536×432 | 20 | 0.4 Mbit/s | 4.0 s | 566/566 | 0 | 0.70 | bit-exact |
| Hybrid thermal camera, colour main | 1280×720 | 29 | 9.7 Mbit/s | 1.7 s | 791/791 | 0 | 8.24 | bit-exact |
| Hybrid thermal camera, thermal sub | 256×192 | 23 | 0.7 Mbit/s | 2.2 s | 673/673 | 0 | 0.45 | bit-exact |
| Hybrid thermal camera, thermal main | 512×384 | 23 | 3.0 Mbit/s | 2.2 s | 676/676 | 0 | 1.81 | bit-exact |
| Home Hub battery recording, sub (fragmented MP4) | 1536×432 | 15 | - | - | 135/135 | 0 | 0.95 | bit-exact |
| Home Hub battery recording, main | 5120×1440 | - | - | - | H.265: not decoded | | | |

Decoding time follows the **bitrate** more than the pixel count: the 9.7 Mbit/s colour stream costs 12× more per
frame than the 1536×432 stream at 0.4 Mbit/s. Every frame has to be decoded (later frames depend on earlier ones),
so that stream uses about 8 ms × 29 fps ≈ 24 % of one M1 core. The 5700U is probably about twice as slow per
core, which is still inside the plan's budget (≤ 15 ms per frame). Capping that camera at about 4 Mbit/s would
roughly halve the cost. (It was capped at about 4 Mbit/s on 2026-09-26. For figures from the production
machine, see the i5-12600H section above.)
