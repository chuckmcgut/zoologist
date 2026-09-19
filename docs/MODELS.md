# Models

All models run in **tract** (pure Rust). They are exported once, at dev time, with the Python tools in
`tools/convert_models/` (never shipped). `models/` is git-ignored; rebuild it with
`scripts/fetch-models.sh`. Checksums are in `models/SHA256SUMS`.

## Object detectors

Input for all: `images` float32 `[1, 3, S, S]`, RGB, values 0..1, **no letterboxing** (Zoologist feeds
square crops, stretched to S×S; a non-square region is letterboxed by the caller). No NMS in the
graph: Zoologist applies class-wise NMS (IoU 0.45) itself.

| File | Source | Architecture | Classes | Output layout | Licence of the upstream inference code |
|---|---|---|---|---|---|
| `md_v1000_sorrel_{320,640}.onnx` | [MegaDetector v1000-sorrel](https://github.com/agentmorris/MegaDetector/releases/tag/v1000.0) (trained at 960) | YOLO11s | animal, person, vehicle | `yolov8`: `[1, 7, N]` = cx, cy, w, h (pixels) + 3 class scores | AGPL-3.0 |
| `md_v1000_spruce_{320,640}.onnx` | MegaDetector v1000-spruce (trained at 640) | YOLOv5s | animal, person, vehicle | `yolov5`: `[1, N, 8]` = cx, cy, w, h (pixels), objectness, 3 class scores | GPL-3.0 |
| `md_v1000_larch_{320,640}.onnx` | MegaDetector v1000-larch (trained at 640) | YOLO11L | animal, person, vehicle | `yolov8` | AGPL-3.0 |
| `yolo11n_coco_*`, `yolo26n_coco_*` | Ultralytics COCO weights | YOLO11n / YOLO26n | 80 COCO classes (names in ONNX metadata) | `yolov8`: `[1, 84, N]` | AGPL-3.0 |

N = 2100 at 320 and 8400 at 640 for the YOLO11 family, 6300 / 25200 for YOLOv5. MegaDetector's own
release notes list relative accuracy (MDv5a = 1.0): sorrel 0.967, larch 0.969, spruce 0.864.

The licence column is the licence of the upstream inference code, as MegaDetector's release notes put it.
Using the weights in private, non-distributed home software is fine. Check it before redistributing.

## Species classifier

| File | Source | Input | Output |
|---|---|---|---|
| `speciesnet.onnx` | [SpeciesNet](https://github.com/google/cameratrapai) `kaggle:google/speciesnet/pyTorch/v4.0.3a/1`, the **always_crop** variant (EfficientNetV2-M), Apache-2.0 | `images` float32 `[1, 480, 480, 3]` **NHWC**, RGB, 0..1: a crop around one animal, stretched to 480×480 | `logits` `[1, 2498]`, apply softmax |
| `speciesnet_labels.txt` | same | one line per output: `uuid;class;order;family;genus;species;common name` | |
| `speciesnet_geofence.json` | same (`geofence_release.20260609.json`) | `{"class;order;family;genus;species": {"allow": {ISO3: [admin1…]}, "block": {…}}}` | used in Step 8.1 |

## Checks

- Every ONNX file matches PyTorch in onnxruntime to a relative error below 1e-5 (`export_*.py`).
- tract matches onnxruntime to a relative error ≤ 1.2e-5 on a fixed input pattern
  (`spikes/tract-bench`, references from `make_pattern_goldens.py`).
- `tools/fixtures/golden/*.json`: detections and species top-5 on the test photos
  (`make_goldens.py`), which the Rust tests compare against.
