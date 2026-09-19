#!/usr/bin/env bash
# Starts a fake RTSP camera for integration tests (development machine only):
# MediaMTX in Docker, with ffmpeg publishing a looped fixture to rtsp://127.0.0.1:8554/test.
# Stop it with: scripts/fake-camera.sh stop
set -euo pipefail
cd "$(dirname "$0")/.."

NAME=zoologist-fake-camera
FIXTURE=${FIXTURE:-tools/fixtures/testsrc_high_640x360_10fps.flv}

if [ "${1:-}" = "stop" ]; then
  pkill -f "rtsp://127.0.0.1:8554/test" || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  echo "fake camera stopped"
  exit 0
fi

docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --rm --name "$NAME" -p 8554:8554 bluenviron/mediamtx:latest >/dev/null
sleep 2
# Loop a container file (FLV): ffmpeg cannot seek back in a raw .h264 file to loop it.
nohup ffmpeg -loglevel error -re -stream_loop -1 -i "$FIXTURE" -c copy \
  -f rtsp -rtsp_transport tcp rtsp://127.0.0.1:8554/test >/dev/null 2>&1 &
sleep 2
echo "fake camera at rtsp://127.0.0.1:8554/test (stop with: $0 stop)"
