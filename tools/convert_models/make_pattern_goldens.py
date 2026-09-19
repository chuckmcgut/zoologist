"""Reference outputs for checking tract against onnxruntime (plan Step 0.3).

For every models/*.onnx, runs onnxruntime on a fixed input pattern, x[i] = ((i * 31) % 256) / 255
in the model's own input layout, and writes the output as raw little-endian f32 to
models/golden/<name>.f32. `cargo run -p tract-bench` computes the same input and compares.
"""

from pathlib import Path

import numpy as np
import onnxruntime as ort

MODELS = Path(__file__).resolve().parents[2] / "models"


def pattern(shape):
    n = int(np.prod(shape))
    return (((np.arange(n, dtype=np.int64) * 31) % 256) / 255.0).astype(np.float32).reshape(shape)


def main():
    out_dir = MODELS / "golden"
    out_dir.mkdir(exist_ok=True)
    for path in sorted(MODELS.glob("*.onnx")):
        session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
        inp = session.get_inputs()[0]
        y = session.run(None, {inp.name: pattern(inp.shape)})[0]
        y.astype("<f4").tofile(out_dir / f"{path.stem}.f32")
        print(f"{path.stem}: input {inp.shape} -> output {list(y.shape)}")


if __name__ == "__main__":
    main()
