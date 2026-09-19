#!/usr/bin/env bash
# Rebuilds models/ from the original weights (dev machine; needs Python 3.12 and uv, ~2 GB).
# See docs/MODELS.md.
set -euo pipefail
cd "$(dirname "$0")/.."
VENV=tools/convert_models/.venv
if [ ! -x "$VENV/bin/python" ]; then
  uv venv --python 3.12 "$VENV"
  VIRTUAL_ENV="$VENV" uv pip install torch torchvision ultralytics megadetector
  VIRTUAL_ENV="$VENV" uv pip install speciesnet
  VIRTUAL_ENV="$VENV" uv pip install onnx onnxruntime onnxslim pillow numpy
fi
"$VENV/bin/python" tools/convert_models/export_detectors.py md_v1000_sorrel md_v1000_spruce "$@"
"$VENV/bin/python" tools/convert_models/export_speciesnet.py
"$VENV/bin/python" tools/convert_models/make_pattern_goldens.py
echo "models ready:"; ls -la models/*.onnx
