"""Reference detections and species results on the test photos (plan Step 0.2).

Dev-time only. For each detector in models/*_{320,640}.onnx and each photo in
tools/fixtures/images/*.jpg, runs onnxruntime with exactly the pre- and post-processing Zoologist
uses, and writes tools/fixtures/golden/<model>.json. The Rust tests compare against these.

Pre-processing: the whole photo, RGB, stretched to SxS with bilinear filtering, /255, NCHW.
Post-processing: decode (yolov5 or yolov8 layout), score >= 0.2, class-wise NMS at IoU 0.45,
boxes normalised to 0..1.

SpeciesNet: the top-scoring animal box of md_v1000_sorrel_640 is cropped (with 10 % margin),
stretched to 480x480, /255, NHWC; top-5 labels are written to golden/speciesnet.json.
"""

import json
from pathlib import Path

import numpy as np
import onnxruntime as ort
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
MODELS = ROOT / "models"
IMAGES = ROOT / "tools" / "fixtures" / "images"
GOLDEN = ROOT / "tools" / "fixtures" / "golden"
SCORE = 0.2
IOU = 0.45
MD_CLASSES = ["animal", "person", "vehicle"]


def coco_names():
    # Ultralytics stores class names in the ONNX metadata.
    return None


def preprocess(img: Image.Image, size: int) -> np.ndarray:
    x = np.asarray(img.convert("RGB").resize((size, size), Image.BILINEAR), dtype=np.float32) / 255.0
    return x.transpose(2, 0, 1)[None]


def iou(a, b):
    ix = max(0.0, min(a[2], b[2]) - max(a[0], b[0]))
    iy = max(0.0, min(a[3], b[3]) - max(a[1], b[1]))
    inter = ix * iy
    union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter
    return inter / union if union > 0 else 0.0


def decode(out: np.ndarray, size: int):
    """Returns [(class_index, score, [x1, y1, x2, y2] normalised)] after NMS."""
    out = out[0]
    if out.shape[0] < out.shape[1]:  # yolov8 layout [4 + C, N]
        out = out.T
        boxes, cls = out[:, :4], out[:, 4:]
        scores = cls.max(1)
        classes = cls.argmax(1)
    else:  # yolov5 layout [N, 5 + C]
        boxes, obj, cls = out[:, :4], out[:, 4], out[:, 5:]
        classes = cls.argmax(1)
        scores = obj * cls.max(1)
    keep = scores >= SCORE
    dets = []
    for (cx, cy, w, h), s, c in zip(boxes[keep], scores[keep], classes[keep]):
        box = [(cx - w / 2) / size, (cy - h / 2) / size, (cx + w / 2) / size, (cy + h / 2) / size]
        dets.append((int(c), float(s), [float(min(max(v, 0.0), 1.0)) for v in box]))
    dets.sort(key=lambda d: -d[1])
    kept = []
    for d in dets:
        if all(k[0] != d[0] or iou(k[2], d[2]) <= IOU for k in kept):
            kept.append(d)
    return kept


def class_names(session, n):
    meta = session.get_modelmeta().custom_metadata_map
    if "names" in meta:  # ultralytics: "{0: 'person', 1: 'bicycle', ...}"
        names = eval(meta["names"])  # trusted: our own export
        if len(names) == n:
            return [names[i] for i in range(n)]
    return MD_CLASSES if n == 3 else [str(i) for i in range(n)]


def main():
    GOLDEN.mkdir(parents=True, exist_ok=True)
    photos = sorted(IMAGES.glob("*.jpg"))
    best_animal = {}
    for model_path in sorted(MODELS.glob("*_[36][24]0.onnx")):
        size = int(model_path.stem.rsplit("_", 1)[1])
        session = ort.InferenceSession(str(model_path), providers=["CPUExecutionProvider"])
        n_classes = None
        result = {}
        for photo in photos:
            img = Image.open(photo)
            out = session.run(None, {session.get_inputs()[0].name: preprocess(img, size)})[0]
            if n_classes is None:
                n_classes = min(out.shape[1:]) - (5 if out.shape[1] > out.shape[2] else 4)
                names = class_names(session, n_classes)
            dets = decode(out, size)
            result[photo.name] = [
                {"class": names[c], "score": round(s, 4), "bbox": [round(v, 5) for v in b]}
                for c, s, b in dets
            ]
            if model_path.stem == "md_v1000_sorrel_640":
                animals = [d for d in dets if d[0] == 0]
                if animals:
                    best_animal[photo.name] = animals[0][2]
        (GOLDEN / f"{model_path.stem}.json").write_text(json.dumps(result, indent=1))
        summary = ", ".join(
            f"{k.removesuffix('.jpg')}:{'/'.join(sorted({d['class'] for d in v})) or '-'}"
            for k, v in result.items()
        )
        print(f"{model_path.stem}: {summary}")

    # SpeciesNet on the best animal crop.
    session = ort.InferenceSession(str(MODELS / "speciesnet.onnx"), providers=["CPUExecutionProvider"])
    labels = (MODELS / "speciesnet_labels.txt").read_text().splitlines()
    species = {}
    for photo in photos:
        box = best_animal.get(photo.name)
        if box is None:
            continue
        img = Image.open(photo).convert("RGB")
        w, h = img.size
        mx, my = (box[2] - box[0]) * 0.1, (box[3] - box[1]) * 0.1
        crop = img.crop((
            max(0, (box[0] - mx) * w), max(0, (box[1] - my) * h),
            min(w, (box[2] + mx) * w), min(h, (box[3] + my) * h),
        )).resize((480, 480), Image.BILINEAR)
        x = (np.asarray(crop, dtype=np.float32) / 255.0)[None]
        logits = session.run(None, {"images": x})[0][0]
        p = np.exp(logits - logits.max())
        p /= p.sum()
        top = np.argsort(-p)[:5]
        species[photo.name] = {
            "bbox": [round(v, 5) for v in box],
            "top5": [{"index": int(i), "label": labels[i].split(";")[-1], "score": round(float(p[i]), 4)} for i in top],
        }
        print(f"speciesnet {photo.stem}: " + ", ".join(f"{t['label']} {t['score']:.2f}" for t in species[photo.name]["top5"][:3]))
    (GOLDEN / "speciesnet.json").write_text(json.dumps(species, indent=1))


if __name__ == "__main__":
    main()
