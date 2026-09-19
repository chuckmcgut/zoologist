"""Export the candidate object detectors to ONNX for tract (plan Step 0.2).

Dev-time only. Run from the repository root:

    tools/convert_models/.venv/bin/python tools/convert_models/export_detectors.py

Writes models/<name>_<size>.onnx with a fixed input shape 1x3xSxS (float32, RGB, 0..1), no NMS in
the graph, simplified, opset 17. Each export is checked against PyTorch with onnxruntime.

Downloads what it needs into models/src/ (MegaDetector v1000 releases, Ultralytics COCO weights).
"""

import os
import shutil
import sys
import tempfile
import urllib.request
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import torch

ROOT = Path(__file__).resolve().parents[2]
MODELS = ROOT / "models"
SRC = MODELS / "src"
SIZES = [320, 640]
MD_URL = "https://github.com/agentmorris/MegaDetector/releases/download/v1000.0/md_v1000.0.0-{}.pt"

# name -> (kind, source file, output layout for docs/MODELS.md)
MODELS_TO_EXPORT = {
    "md_v1000_spruce": ("yolov5", "md_v1000.0.0-spruce.pt"),
    "md_v1000_sorrel": ("ultralytics", "md_v1000.0.0-sorrel.pt"),
    "md_v1000_larch": ("ultralytics", "md_v1000.0.0-larch.pt"),
    "yolo11n_coco": ("ultralytics", "yolo11n.pt"),
    "yolo26n_coco": ("ultralytics", "yolo26n.pt"),
}


def fetch(name: str) -> Path:
    path = SRC / name
    if path.exists():
        return path
    SRC.mkdir(parents=True, exist_ok=True)
    if name.startswith("md_v1000"):
        variant = name.split("-")[1].removesuffix(".pt")
        print(f"downloading {name}")
        urllib.request.urlretrieve(MD_URL.format(variant), path)
    else:
        # Ultralytics downloads its own COCO weights into the working directory.
        from ultralytics import YOLO

        with tempfile.TemporaryDirectory() as tmp:
            cwd = os.getcwd()
            os.chdir(tmp)
            try:
                YOLO(name)
                shutil.move(os.path.join(tmp, name), path)
            finally:
                os.chdir(cwd)
    return path


class FirstOutput(torch.nn.Module):
    """YOLOv5 returns (predictions, raw feature maps); export only the predictions."""

    def __init__(self, model):
        super().__init__()
        self.model = model

    def forward(self, x):
        return self.model(x)[0]


def load_yolov5(path: Path):
    import yolov5  # the pip package carries the original yolov5 "models" module

    sys.path.insert(0, os.path.dirname(yolov5.__file__))
    ckpt = torch.load(path, map_location="cpu", weights_only=False)
    model = (ckpt.get("ema") or ckpt["model"]).float().fuse().eval()
    for m in model.modules():
        if hasattr(m, "inplace"):
            m.inplace = False
    return FirstOutput(model).eval()


def export_yolov5(path: Path, size: int, out: Path) -> torch.nn.Module:
    model = load_yolov5(path)
    dummy = torch.zeros(1, 3, size, size)
    torch.onnx.export(
        model,
        dummy,
        str(out),
        opset_version=17,
        input_names=["images"],
        output_names=["output"],
        dynamo=False,
    )
    return model


def export_ultralytics(path: Path, size: int, out: Path) -> torch.nn.Module:
    from ultralytics import YOLO

    with tempfile.TemporaryDirectory() as tmp:
        tmp_pt = Path(tmp) / path.name
        shutil.copy(path, tmp_pt)
        yolo = YOLO(str(tmp_pt))
        exported = yolo.export(format="onnx", imgsz=size, opset=17, simplify=True, dynamic=False)
        shutil.move(exported, out)
    model = YOLO(str(path)).model.float().eval()

    class Wrapped(torch.nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, x):
            y = self.m(x)
            return y[0] if isinstance(y, (list, tuple)) else y

    return Wrapped(model).eval()


def simplify(path: Path):
    import onnxslim

    model = onnxslim.slim(onnx.load(str(path)))
    onnx.save(model, str(path))


def check(model: torch.nn.Module, path: Path, size: int) -> tuple[float, tuple]:
    rng = np.random.default_rng(0)
    x = rng.random((1, 3, size, size), dtype=np.float32)
    with torch.no_grad():
        expected = model(torch.from_numpy(x)).numpy()
    session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
    actual = session.run(None, {session.get_inputs()[0].name: x})[0]
    # Relative to the output's range: box coordinates are in pixels (up to 640).
    scale = max(1.0, float(np.abs(expected).max()))
    return float(np.abs(expected - actual).max()) / scale, actual.shape


def main():
    MODELS.mkdir(exist_ok=True)
    only = set(sys.argv[1:])
    for name, (kind, source) in MODELS_TO_EXPORT.items():
        if only and name not in only:
            continue
        src = fetch(source)
        for size in SIZES:
            out = MODELS / f"{name}_{size}.onnx"
            model = (export_yolov5 if kind == "yolov5" else export_ultralytics)(src, size, out)
            simplify(out)
            diff, shape = check(model, out, size)
            ops = sorted({n.op_type for n in onnx.load(str(out)).graph.node})
            print(f"{out.name}: output {list(shape)}, max relative |torch-onnx| = {diff:.2e}")
            print(f"    ops: {' '.join(ops)}")
            assert diff < 1e-4, f"{out.name}: ONNX differs from PyTorch by {diff} (relative)"


if __name__ == "__main__":
    main()
