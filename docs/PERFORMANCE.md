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
