#!/usr/bin/env bash
# Samples /api/v1/health every minute into a CSV, one row per camera (plan Step 11.4).
# Needs curl and jq.
#
#   scripts/perf-log.sh [http://vm:8090] [perf.csv] [hours]
#
# Columns: time, camera, detect state, stream fps, analysed fps, decode ms, dropped decoded frames,
# dropped detector jobs, detector mean ms, detector p95 ms, detector queue, species queue,
# container CPU % and memory (from `docker stats`, empty when docker is not available here).
set -euo pipefail
BASE="${1:-http://127.0.0.1:8090}"
OUT="${2:-perf-$(date +%Y%m%d-%H%M).csv}"
HOURS="${3:-24}"

echo "time,camera,state,fps,analysed_fps,decode_ms,decode_drops,detector_drops,det_mean_ms,det_p95_ms,det_queue,species_queue,cpu_pct,mem" > "$OUT"
end=$(( $(date +%s) + HOURS * 3600 ))
while [ "$(date +%s)" -lt "$end" ]; do
  now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  stats=""
  if command -v docker >/dev/null 2>&1; then
    stats=$(docker stats --no-stream --format '{{.CPUPerc}},{{.MemUsage}}' zoologist-zoologist-1 2>/dev/null \
      | sed 's/%//; s/ \/ .*//' || true)
  fi
  if health=$(curl -fsS --max-time 10 "$BASE/api/v1/health"); then
    echo "$health" | jq -r --arg now "$now" --arg stats "${stats:-,}" '
      .detector as $d | .species as $s |
      .cameras[] | [
        $now, .id, .detect.state, .detect.fps, .detect.analysed_fps, .detect.decode_ms,
        .drops.decoded_frames, .drops.detector_jobs,
        ($d.mean_ms // ""), ($d.p95_ms // ""), ($d.queue_depth // ""), ($s.queue_depth // "")
      ] | map(tostring) | join(",") + "," + $stats' >> "$OUT"
  else
    echo "$now,,unreachable,,,,,,,,,,${stats:-,}" >> "$OUT"
  fi
  sleep 60
done
echo "wrote $OUT"
