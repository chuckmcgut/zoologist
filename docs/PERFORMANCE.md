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

Throughput scales almost linearly up to 4 workers. Expect the Ryzen 7 5700U (Zen 2, 15 W, AVX2) to be
roughly 1.5–2.5× slower per core. **Re-run this on the target machine** (Step 0.3 proper).

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
roughly halve the cost.
