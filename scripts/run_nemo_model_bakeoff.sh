#!/usr/bin/env bash
set -euo pipefail

SOURCE="${BASH_SOURCE[0]}"
while [[ -L "$SOURCE" ]]; do
  DIR="$(cd -P "$(dirname "$SOURCE")" && pwd)"
  SOURCE="$(readlink "$SOURCE")"
  [[ "$SOURCE" != /* ]] && SOURCE="$DIR/$SOURCE"
done
REPO_DIR="$(cd -P "$(dirname "$SOURCE")/.." && pwd)"

RIFF_BIN="${RIFF_BIN:-$REPO_DIR/target/release/riff}"
PYTHON_BIN="${PYTHON_BIN:-$(command -v python3 || command -v python || true)}"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
OUT_DIR="${OUT_DIR:-$REPO_DIR/tmp/nemo-model-bakeoff-$TIMESTAMP}"
WORK_ROOT_BASE="${WORK_ROOT_BASE:-/tmp/riff-model-bakeoff-$TIMESTAMP}"
BASE_PORT="${BASE_PORT:-8875}"
ITERATIONS=1
DURATIONS=(5 15 30)
MODELS=(
  "nvidia/parakeet-tdt_ctc-110m"
  "nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc"
  "nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms"
  "nvidia/stt_en_fastconformer_ctc_large"
)

usage() {
  cat <<'EOF'
Usage: scripts/run_nemo_model_bakeoff.sh [options]

Runs the real warm-server 5/15/30 `riff start` / `riff stop` benchmark across
multiple NVIDIA NeMo ASR models.

Options:
  --models a,b,c              Comma-separated Hugging Face model ids
  --durations 5,15,30         Comma-separated durations in seconds
  --iterations N              Repeats each duration N times per model
  --out-dir PATH              Write report artifacts to PATH
  --work-root-base PATH       Base directory for per-model RIFF_ROOT workspaces
  --base-port N               First server port to use (one port per model)
  --riff-bin PATH             Override riff executable path
  -h, --help                  Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --models)
      IFS=',' read -r -a MODELS <<< "${2:-}"
      shift 2
      ;;
    --durations)
      IFS=',' read -r -a DURATIONS <<< "${2:-}"
      shift 2
      ;;
    --iterations)
      ITERATIONS="${2:-}"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="${2:-}"
      shift 2
      ;;
    --work-root-base)
      WORK_ROOT_BASE="${2:-}"
      shift 2
      ;;
    --base-port)
      BASE_PORT="${2:-}"
      shift 2
      ;;
    --riff-bin)
      RIFF_BIN="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$PYTHON_BIN" ]]; then
  echo "python3 or python is required." >&2
  exit 1
fi

if ! [[ "$ITERATIONS" =~ ^[1-9][0-9]*$ ]]; then
  echo "--iterations must be a positive integer." >&2
  exit 1
fi

if ! [[ "$BASE_PORT" =~ ^[0-9]+$ ]]; then
  echo "--base-port must be numeric." >&2
  exit 1
fi

mkdir -p "$OUT_DIR" "$WORK_ROOT_BASE"
RESULTS_JSONL="$OUT_DIR/results.jsonl"
REPORT_JSON="$OUT_DIR/report.json"
SUMMARY_TXT="$OUT_DIR/summary.txt"
: > "$RESULTS_JSONL"

if [[ ! -x "$RIFF_BIN" || "$RIFF_BIN" == "$REPO_DIR/target/release/riff" ]]; then
  echo "Building current release binary..."
  (cd "$REPO_DIR" && cargo build --release >/dev/null)
fi

measure_command() {
  local __ms_var="$1"
  local __status_var="$2"
  shift 2
  local start_ns end_ns status
  start_ns="$("$PYTHON_BIN" -c 'import time; print(time.perf_counter_ns())')"
  set +e
  "$@"
  status=$?
  set -e
  end_ns="$("$PYTHON_BIN" -c 'import time; print(time.perf_counter_ns())')"
  printf -v "$__ms_var" '%s' "$("$PYTHON_BIN" - "$start_ns" "$end_ns" <<'PY'
import sys
start_ns = int(sys.argv[1])
end_ns = int(sys.argv[2])
print(round((end_ns - start_ns) / 1_000_000, 3))
PY
)"
  printf -v "$__status_var" '%s' "$status"
}

slugify_model() {
  printf '%s' "$1" | tr '/:@' '---' | tr -cd '[:alnum:]._-' 
}

run_case() {
  local model="$1"
  local port="$2"
  local duration_sec="$3"
  local iteration="$4"
  local slug="$5"
  local root_dir="$6"
  local case_dir="$OUT_DIR/$slug/duration-${duration_sec}s-run-${iteration}"
  local start_json="$case_dir/start.json"
  local stop_json="$case_dir/stop.json"
  local start_wall_ms start_status stop_wall_ms stop_status

  mkdir -p "$case_dir"
  echo "Running model=${model} duration=${duration_sec}s iteration=${iteration}"

  local env_vars=(
    RIFF_ROOT="$root_dir"
    RIFF_BEEP=0
    RIFF_WEB_SERVER=0
    RIFF_PARAKEET_MODEL="$model"
    RIFF_PARAKEET_SERVER_URL="http://127.0.0.1:$port"
  )

  measure_command start_wall_ms start_status \
    env "${env_vars[@]}" \
    "$RIFF_BIN" --quiet --json start > "$start_json"

  if [[ "$start_status" -ne 0 ]]; then
    "$PYTHON_BIN" - "$RESULTS_JSONL" "$model" "$port" "$duration_sec" "$iteration" "$slug" "$root_dir" "$start_wall_ms" <<'PY'
import json, sys
from pathlib import Path
results = Path(sys.argv[1])
record = {
    "model": sys.argv[2],
    "server_port": int(sys.argv[3]),
    "duration_sec": int(sys.argv[4]),
    "iteration": int(sys.argv[5]),
    "slug": sys.argv[6],
    "riff_root": sys.argv[7],
    "status": "start_failed",
    "start_wall_ms": float(sys.argv[8]),
}
with results.open("a") as fh:
    fh.write(json.dumps(record) + "\n")
PY
    return
  fi

  sleep "$duration_sec"

  measure_command stop_wall_ms stop_status \
    env "${env_vars[@]}" \
    "$RIFF_BIN" --quiet --json stop > "$stop_json"

  "$PYTHON_BIN" - "$RESULTS_JSONL" "$model" "$port" "$duration_sec" "$iteration" "$slug" "$root_dir" "$start_wall_ms" "$stop_wall_ms" "$start_json" "$stop_json" "$stop_status" <<'PY'
import json, sys
from pathlib import Path

results = Path(sys.argv[1])
model = sys.argv[2]
port = int(sys.argv[3])
duration_sec = int(sys.argv[4])
iteration = int(sys.argv[5])
slug = sys.argv[6]
riff_root = sys.argv[7]
start_wall_ms = float(sys.argv[8])
stop_wall_ms = float(sys.argv[9])
start = json.load(open(sys.argv[10]))
stop = json.load(open(sys.argv[11])) if Path(sys.argv[11]).exists() and Path(sys.argv[11]).stat().st_size > 0 else {}
stop_status = int(sys.argv[12])

trans = stop.get("transcription") or {}
tperf = trans.get("perf") or {}
phases = stop.get("phases") or {}

record = {
    "model": model,
    "server_port": port,
    "duration_sec": duration_sec,
    "iteration": iteration,
    "slug": slug,
    "riff_root": riff_root,
    "status": "ok" if stop_status == 0 else "stop_failed",
    "start_wall_ms": start_wall_ms,
    "start_reported_ms": start.get("startup_ms"),
    "stop_wall_ms": stop_wall_ms,
    "stop_reported_ms": stop.get("stop_ms"),
    "start_phases": start.get("phases", {}),
    "stop_phases": phases,
    "transcription_method": trans.get("method"),
    "transcription_status": trans.get("status"),
    "batch_size": trans.get("batch_size"),
    "server_health_before": tperf.get("server_health_before"),
    "server_health_after": tperf.get("server_health_after"),
    "server_ensure_ms": tperf.get("server_ensure_ms"),
    "server_request_ms": tperf.get("server_request_ms"),
    "transcribe_ms": phases.get("transcribe_ms"),
}

with results.open("a") as fh:
    fh.write(json.dumps(record) + "\n")
PY
}

model_index=0
for model in "${MODELS[@]}"; do
  slug="$(slugify_model "$model")"
  port=$((BASE_PORT + model_index))
  root_dir="$WORK_ROOT_BASE/$slug"
  mkdir -p "$root_dir"

  echo "Warming model=${model} on port=${port}"
  env \
    RIFF_ROOT="$root_dir" \
    RIFF_BEEP=0 \
    RIFF_WEB_SERVER=0 \
    RIFF_PARAKEET_MODEL="$model" \
    RIFF_PARAKEET_SERVER_URL="http://127.0.0.1:$port" \
    "$RIFF_BIN" --quiet kill-server >/dev/null 2>&1 || true

  if env \
      RIFF_ROOT="$root_dir" \
      RIFF_BEEP=0 \
      RIFF_WEB_SERVER=0 \
      RIFF_PARAKEET_MODEL="$model" \
      RIFF_PARAKEET_SERVER_URL="http://127.0.0.1:$port" \
      "$RIFF_BIN" --quiet --json status >/dev/null 2>&1; then
    :
  fi

  warm_start_json="$OUT_DIR/$slug/warmup-start.json"
  warm_stop_json="$OUT_DIR/$slug/warmup-stop.json"
  mkdir -p "$OUT_DIR/$slug"
  env \
    RIFF_ROOT="$root_dir" \
    RIFF_BEEP=0 \
    RIFF_WEB_SERVER=0 \
    RIFF_PARAKEET_MODEL="$model" \
    RIFF_PARAKEET_SERVER_URL="http://127.0.0.1:$port" \
    "$RIFF_BIN" --quiet --json start > "$warm_start_json"
  sleep 1
  env \
    RIFF_ROOT="$root_dir" \
    RIFF_BEEP=0 \
    RIFF_WEB_SERVER=0 \
    RIFF_PARAKEET_MODEL="$model" \
    RIFF_PARAKEET_SERVER_URL="http://127.0.0.1:$port" \
    "$RIFF_BIN" --quiet --json stop > "$warm_stop_json"

  for duration_sec in "${DURATIONS[@]}"; do
    for ((iteration = 1; iteration <= ITERATIONS; iteration++)); do
      run_case "$model" "$port" "$duration_sec" "$iteration" "$slug" "$root_dir"
    done
  done

  model_index=$((model_index + 1))
done

"$PYTHON_BIN" - "$RESULTS_JSONL" "$REPORT_JSON" "$SUMMARY_TXT" <<'PY'
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

results_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
summary_path = Path(sys.argv[3])
records = [json.loads(line) for line in results_path.read_text().splitlines() if line.strip()]

by_model = defaultdict(list)
for record in records:
    by_model[record["model"]].append(record)

report = {"results": records, "models": []}
summary_lines = []

for model, model_records in by_model.items():
    ok = [r for r in model_records if r.get("status") == "ok"]
    durations = {}
    for r in ok:
        durations.setdefault(r["duration_sec"], []).append(r)
    model_summary = {"model": model, "durations": {}, "overall": {}}
    summary_lines.append(model)
    for duration in sorted(durations):
        rs = durations[duration]
        start_wall = [float(r["start_wall_ms"]) for r in rs]
        stop_wall = [float(r["stop_wall_ms"]) for r in rs]
        server_request = [float(r["server_request_ms"]) for r in rs if r.get("server_request_ms") is not None]
        payload = {
            "runs": len(rs),
            "start_wall_ms_avg": round(statistics.mean(start_wall), 3),
            "stop_wall_ms_avg": round(statistics.mean(stop_wall), 3),
            "server_request_ms_avg": round(statistics.mean(server_request), 3) if server_request else None,
            "all_server_health_before_true": all(r.get("server_health_before") is True for r in rs),
            "all_server_health_after_true": all(r.get("server_health_after") is True for r in rs),
        }
        model_summary["durations"][str(duration)] = payload
        summary_lines.append(
            f"  {duration}s: start={payload['start_wall_ms_avg']}ms stop={payload['stop_wall_ms_avg']}ms server_request={payload['server_request_ms_avg']}ms"
        )
    if ok:
        model_summary["overall"] = {
            "start_wall_ms_avg": round(statistics.mean(float(r["start_wall_ms"]) for r in ok), 3),
            "stop_wall_ms_avg": round(statistics.mean(float(r["stop_wall_ms"]) for r in ok), 3),
        }
    report["models"].append(model_summary)
    summary_lines.append("")

report_path.write_text(json.dumps(report, indent=2) + "\n")
summary_path.write_text("\n".join(summary_lines).rstrip() + "\n")
PY

echo "Wrote:"
echo "  $REPORT_JSON"
echo "  $RESULTS_JSONL"
echo "  $SUMMARY_TXT"
