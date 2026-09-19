"""Export the SpeciesNet classifier to ONNX for tract (plan Step 0.2).

Dev-time only. Run from the repository root:

    tools/convert_models/.venv/bin/python tools/convert_models/export_speciesnet.py

Uses the "always_crop" SpeciesNet variant (v4.0.3a), which classifies a crop around one animal,
exactly how Zoologist uses it. Writes:

- models/speciesnet.onnx: input `images` [1, 480, 480, 3] float32 RGB in 0..1 (NHWC), output
  logits [1, N] (apply softmax);
- models/speciesnet_labels.txt: one label per output, "uuid;class;order;family;genus;species;common name";
- models/speciesnet_taxonomy.txt: every taxon (including families etc.) in the label format, for roll-ups;
- models/speciesnet_geofence.json: the package's geofence rules (copied as-is).
"""

import json
import shutil
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import torch

ROOT = Path(__file__).resolve().parents[2]
MODELS = ROOT / "models"
MODEL_NAME = "kaggle:google/speciesnet/pyTorch/v4.0.3a/1"
SIZE = 480


def main():
    from speciesnet.utils import ModelInfo

    info = ModelInfo(MODEL_NAME)
    print("model type:", info.type_)
    print("classifier:", info.classifier)
    assert info.type_ == "always_crop", info.type_
    model = torch.load(info.classifier, map_location="cpu", weights_only=False).eval()
    for p in model.parameters():
        p.requires_grad = False

    out = MODELS / "speciesnet.onnx"
    dummy = torch.zeros(1, SIZE, SIZE, 3)
    torch.onnx.export(
        model,
        dummy,
        str(out),
        opset_version=17,
        input_names=["images"],
        output_names=["logits"],
        dynamo=False,
    )
    import onnxslim

    onnx.save(onnxslim.slim(onnx.load(str(out))), str(out))

    # Check against PyTorch on random input.
    x = np.random.default_rng(0).random((1, SIZE, SIZE, 3), dtype=np.float32)
    with torch.no_grad():
        expected = model(torch.from_numpy(x)).numpy()
    session = ort.InferenceSession(str(out), providers=["CPUExecutionProvider"])
    actual = session.run(None, {"images": x})[0]
    diff = float(np.abs(expected - actual).max())
    ops = sorted({n.op_type for n in onnx.load(str(out)).graph.node})
    print(f"{out.name}: output {list(actual.shape)}, max |torch-onnx| = {diff:.2e}")
    print(f"    ops: {' '.join(ops)}")
    assert diff < 1e-3, diff

    shutil.copy(info.classifier_labels, MODELS / "speciesnet_labels.txt")
    labels = Path(info.classifier_labels).read_text().splitlines()
    assert len(labels) == actual.shape[1], (len(labels), actual.shape)
    print(f"labels: {len(labels)}, e.g. {labels[0]!r}")

    taxonomy = getattr(info, "taxonomy", None)
    if taxonomy and Path(taxonomy).exists():
        shutil.copy(taxonomy, MODELS / "speciesnet_taxonomy.txt")
        print(f"taxonomy: {len(Path(taxonomy).read_text().splitlines())} entries")
    geofence = getattr(info, "geofence", None)
    if geofence and Path(geofence).exists():
        data = json.loads(Path(geofence).read_text())
        (MODELS / "speciesnet_geofence.json").write_text(json.dumps(data))
        print(f"geofence rules: {len(data)} entries")
    for field in ("taxonomy", "geofence", "detector"):
        print(f"{field}: {getattr(info, field, None)}")


if __name__ == "__main__":
    main()
