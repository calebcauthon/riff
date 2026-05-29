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
REAL_FFMPEG="${REAL_FFMPEG:-$(command -v ffmpeg || true)}"
REAL_FFPROBE="${REAL_FFPROBE:-$(command -v ffprobe || true)}"
PYTHON_BIN="${PYTHON_BIN:-$(command -v python3 || command -v python || true)}"
SAY_BIN="${SAY_BIN:-$(command -v say || true)}"

if [[ -z "$REAL_FFMPEG" ]]; then
  echo "ffmpeg is required." >&2
  exit 1
fi
if [[ -z "$REAL_FFPROBE" ]]; then
  echo "ffprobe is required." >&2
  exit 1
fi
if [[ -z "$PYTHON_BIN" ]]; then
  echo "python3 or python is required." >&2
  exit 1
fi

TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
OUT_DIR="${OUT_DIR:-$REPO_DIR/tmp/start-stop-experiment-$TIMESTAMP}"
ITERATIONS=1
KEEP_WORKDIR=0
DURATIONS=(5 15 30)
MODES=(cold_stop warm_server_stop live_transcribe_stop)
SAMPLE_AUDIO=""
TRANSCRIPTION_MODE="auto"
DISABLE_WEB_SERVER=1
PRACTICAL_WAIT_PAD_SEC="0.75"

usage() {
  cat <<'EOF'
Usage: scripts/run_start_stop_experiment.sh [options]

Runs practical start/stop benchmarks by replaying a real audio file through
riff's recorder path in real time. By default it executes three modes:
  cold_stop
  warm_server_stop
  live_transcribe_stop

Options:
  --sample-audio PATH         Audio file to replay during recording
  --durations 5,15,30         Comma-separated durations in seconds
  --iterations N              Repeats each duration N times
  --modes a,b,c               Subset of: cold_stop,warm_server_stop,live_transcribe_stop
  --transcription-mode MODE   auto | real | stub
  --out-dir PATH              Write artifacts/report to PATH
  --riff-bin PATH             Override riff executable path
  --keep-workdir              Keep the temporary RIFF_ROOT workspace
  --with-web-server           Do not disable RIFF_WEB_SERVER
  -h, --help                  Show this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sample-audio)
      SAMPLE_AUDIO="${2:-}"
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
    --modes)
      IFS=',' read -r -a MODES <<< "${2:-}"
      shift 2
      ;;
    --transcription-mode)
      TRANSCRIPTION_MODE="${2:-}"
      shift 2
      ;;
    --out-dir)
      OUT_DIR="${2:-}"
      shift 2
      ;;
    --riff-bin)
      RIFF_BIN="${2:-}"
      shift 2
      ;;
    --keep-workdir)
      KEEP_WORKDIR=1
      shift
      ;;
    --with-web-server)
      DISABLE_WEB_SERVER=0
      shift
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

case "$TRANSCRIPTION_MODE" in
  auto|real|stub) ;;
  *)
    echo "--transcription-mode must be auto, real, or stub." >&2
    exit 1
    ;;
esac

if ! [[ "$ITERATIONS" =~ ^[1-9][0-9]*$ ]]; then
  echo "--iterations must be a positive integer." >&2
  exit 1
fi

for mode in "${MODES[@]}"; do
  case "$mode" in
    cold_stop|warm_server_stop|live_transcribe_stop) ;;
    *)
      echo "Unknown mode: $mode" >&2
      exit 1
      ;;
  esac
done

mkdir -p "$OUT_DIR"

if [[ ! -x "$RIFF_BIN" || "$RIFF_BIN" == "$REPO_DIR/target/release/riff" ]]; then
  echo "Building current release binary..."
  (cd "$REPO_DIR" && cargo build --release >/dev/null)
fi

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/riff-start-stop-experiment.XXXXXX")"
if [[ "$KEEP_WORKDIR" -eq 0 ]]; then
  trap 'rm -rf "$WORK_DIR"' EXIT
fi

BENCH_ROOT="$WORK_DIR/riff-root"
FAKE_BIN="$WORK_DIR/fake-bin"
SCREENSHOT_SOURCE="$WORK_DIR/source-shots"
RESULTS_JSONL="$OUT_DIR/results.jsonl"
REPORT_JSON="$OUT_DIR/report.json"
SUMMARY_TXT="$OUT_DIR/summary.txt"
AUTO_AUDIO_DIR="$WORK_DIR/audio"

mkdir -p "$BENCH_ROOT" "$FAKE_BIN" "$SCREENSHOT_SOURCE" "$AUTO_AUDIO_DIR" "$WORK_DIR/mpl-cache"
: > "$RESULTS_JSONL"

write_executable() {
  local path="$1"
  local content="$2"
  printf '%s' "$content" > "$path"
  chmod +x "$path"
}

install_fake_tools() {
  write_executable "$FAKE_BIN/ffmpeg" '#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"-list_devices true"* ]]; then
  echo "AVFoundation audio devices"
  echo "[0] Built-in Microphone"
  exit 0
fi
out="${@: -1}"
if [[ -z "${RIFF_EXPERIMENT_INPUT_AUDIO:-}" ]]; then
  echo "RIFF_EXPERIMENT_INPUT_AUDIO is required for recorder shim" >&2
  exit 1
fi
exec "${RIFF_EXPERIMENT_REAL_FFMPEG:?}" \
  -hide_banner \
  -loglevel error \
  -re \
  -i "${RIFF_EXPERIMENT_INPUT_AUDIO}" \
  -ac 1 \
  -ar 16000 \
  -c:a pcm_s16le \
  "$out"
'

  write_executable "$FAKE_BIN/screencapture" '#!/usr/bin/env bash
set -euo pipefail
out="${@: -1}"
mkdir -p "$(dirname "$out")"
printf "%b" "\x89\x50\x4E\x47\x0D\x0A\x1A\x0A\x00\x00\x00\x0D\x49\x48\x44\x52\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1F\x15\xC4\x89\x00\x00\x00\x0A\x49\x44\x41\x54\x78\x9C\x63\x00\x01\x00\x00\x05\x00\x01\x0D\x0A\x2D\xB4\x00\x00\x00\x00\x49\x45\x4E\x44\xAE\x42\x60\x82" > "$out"
'

  write_executable "$FAKE_BIN/osascript" '#!/usr/bin/env bash
set -euo pipefail
printf "ExperimentApp\tcom.example.ExperimentApp\t4242\tSynthetic Window\n"
'

  write_executable "$FAKE_BIN/pbpaste" '#!/usr/bin/env bash
set -euo pipefail
printf ""
'

  write_executable "$FAKE_BIN/pbcopy" '#!/usr/bin/env bash
set -euo pipefail
cat >/dev/null
'

  write_executable "$FAKE_BIN/ps" '#!/usr/bin/env bash
set -euo pipefail
printf "1.0 0.1 1024 00:01 S /Applications/ExperimentApp.app/Contents/MacOS/ExperimentApp\n"
'

  write_executable "$FAKE_BIN/afplay" '#!/usr/bin/env bash
set -euo pipefail
exit 0
'

  write_executable "$FAKE_BIN/open" '#!/usr/bin/env bash
set -euo pipefail
exit 0
'
}

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

generate_default_sample_audio() {
  local out_wav="$1"
  local say_aiff="$AUTO_AUDIO_DIR/sample-speech.aiff"
  if [[ -n "$SAY_BIN" ]]; then
    "$SAY_BIN" -o "$say_aiff" \
      "This is a practical riff benchmark. We are replaying real speech through the recorder path to measure start and stop latency at five, fifteen, and thirty second durations."
    "$REAL_FFMPEG" -hide_banner -loglevel error -y -i "$say_aiff" -ac 1 -ar 16000 -c:a pcm_s16le "$out_wav"
    return
  fi
  "$REAL_FFMPEG" -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=440:duration=35" -ac 1 -ar 16000 -c:a pcm_s16le "$out_wav"
}

prepare_sample_audio() {
  local out_wav="$AUTO_AUDIO_DIR/base-sample.wav"
  if [[ -n "$SAMPLE_AUDIO" ]]; then
    "$REAL_FFMPEG" -hide_banner -loglevel error -y -i "$SAMPLE_AUDIO" -ac 1 -ar 16000 -c:a pcm_s16le "$out_wav"
  else
    generate_default_sample_audio "$out_wav"
  fi
  printf '%s\n' "$out_wav"
}

clip_audio_for_duration() {
  local source_audio="$1"
  local duration_sec="$2"
  local clip_path="$AUTO_AUDIO_DIR/clip-${duration_sec}s.wav"
  "$REAL_FFMPEG" \
    -hide_banner \
    -loglevel error \
    -y \
    -stream_loop -1 \
    -i "$source_audio" \
    -t "$duration_sec" \
    -ac 1 \
    -ar 16000 \
    -c:a pcm_s16le \
    "$clip_path"
  printf '%s\n' "$clip_path"
}

detect_real_transcription_support() {
  local script_path=""
  if [[ -n "${RIFF_PARAKEET_SCRIPT:-}" && -f "${RIFF_PARAKEET_SCRIPT}" ]]; then
    script_path="${RIFF_PARAKEET_SCRIPT}"
  elif [[ -f "$REPO_DIR/scripts/parakeet_transcribe.py" ]]; then
    script_path="$REPO_DIR/scripts/parakeet_transcribe.py"
  fi
  if [[ -z "$script_path" ]]; then
    return 1
  fi
  "$PYTHON_BIN" "$script_path" --help >/dev/null 2>&1
}

install_fake_tools
BASE_SAMPLE_AUDIO="$(prepare_sample_audio)"

REAL_TRANSCRIPTION_AVAILABLE=0
if detect_real_transcription_support; then
  REAL_TRANSCRIPTION_AVAILABLE=1
fi

EFFECTIVE_TRANSCRIPTION_MODE="$TRANSCRIPTION_MODE"
if [[ "$EFFECTIVE_TRANSCRIPTION_MODE" == "auto" ]]; then
  if [[ "$REAL_TRANSCRIPTION_AVAILABLE" -eq 1 ]]; then
    EFFECTIVE_TRANSCRIPTION_MODE="real"
  else
    EFFECTIVE_TRANSCRIPTION_MODE="stub"
  fi
fi

if [[ "$EFFECTIVE_TRANSCRIPTION_MODE" == "real" && "$REAL_TRANSCRIPTION_AVAILABLE" -ne 1 ]]; then
  echo "Real transcription requested, but local Parakeet setup is not runnable." >&2
  exit 1
fi

mode_live_flag() {
  case "$1" in
    cold_stop|warm_server_stop) echo 0 ;;
    live_transcribe_stop) echo 1 ;;
  esac
}

mode_server_flag() {
  case "$1" in
    cold_stop|live_transcribe_stop) echo 0 ;;
    warm_server_stop) echo 1 ;;
  esac
}

transcription_mode_for_case() {
  if [[ "$EFFECTIVE_TRANSCRIPTION_MODE" == "stub" ]]; then
    echo "stub"
  else
    echo "real"
  fi
}

run_case() {
  local mode="$1"
  local duration_sec="$2"
  local iteration="$3"
  local case_dir="$OUT_DIR/${mode}/duration-${duration_sec}s-run-${iteration}"
  local start_json="$case_dir/start.json"
  local stop_json="$case_dir/stop.json"
  local clip_audio
  local start_wall_ms start_status
  local stop_wall_ms stop_status
  local live_flag server_flag case_transcription_mode

  mkdir -p "$case_dir"
  clip_audio="$(clip_audio_for_duration "$BASE_SAMPLE_AUDIO" "$duration_sec")"
  live_flag="$(mode_live_flag "$mode")"
  server_flag="$(mode_server_flag "$mode")"
  case_transcription_mode="$(transcription_mode_for_case)"

  echo "Running mode=${mode} duration=${duration_sec}s iteration=${iteration}"

  local common_env=(
    PATH="$FAKE_BIN:$PATH"
    RIFF_ROOT="$BENCH_ROOT"
    RIFF_BEEP=0
    RIFF_CLIPBOARD_MONITOR=0
    MPLCONFIGDIR="$WORK_DIR/mpl-cache"
    RIFF_EXPERIMENT_REAL_FFMPEG="$REAL_FFMPEG"
    RIFF_EXPERIMENT_INPUT_AUDIO="$clip_audio"
    RIFF_LIVE_TRANSCRIBE="$live_flag"
    RIFF_PARAKEET_SERVER="$server_flag"
  )
  if [[ "$DISABLE_WEB_SERVER" -eq 1 ]]; then
    common_env+=(RIFF_WEB_SERVER=0)
  fi

  measure_command start_wall_ms start_status \
    env "${common_env[@]}" \
    "$RIFF_BIN" --quiet --json start --screenshot-dir "$SCREENSHOT_SOURCE" > "$start_json"

  if [[ "$start_status" -ne 0 ]]; then
    "$PYTHON_BIN" - "$RESULTS_JSONL" "$mode" "$duration_sec" "$iteration" "$start_wall_ms" "$case_transcription_mode" "$live_flag" "$server_flag" "$case_dir" <<'PY'
import json, sys
from pathlib import Path
results = Path(sys.argv[1])
record = {
    "mode": sys.argv[2],
    "duration_sec": int(sys.argv[3]),
    "iteration": int(sys.argv[4]),
    "status": "start_failed",
    "start_wall_ms": float(sys.argv[5]),
    "transcription_mode": sys.argv[6],
    "live_transcribe": bool(int(sys.argv[7])),
    "server_enabled": bool(int(sys.argv[8])),
    "artifacts": {"case_dir": sys.argv[9]},
}
with results.open("a") as fh:
    fh.write(json.dumps(record) + "\n")
PY
    echo "  start failed for mode=${mode}"
    return
  fi

  local session_info
  session_info="$("$PYTHON_BIN" - "$start_json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
print(data["session_id"])
print(data["session_dir"])
print(data["audio_path"])
PY
)"
  local session_id session_dir audio_path
  session_id="$(printf '%s\n' "$session_info" | sed -n '1p')"
  session_dir="$(printf '%s\n' "$session_info" | sed -n '2p')"
  audio_path="$(printf '%s\n' "$session_info" | sed -n '3p')"

  sleep "$("$PYTHON_BIN" - "$duration_sec" "$PRACTICAL_WAIT_PAD_SEC" <<'PY'
import sys
print(float(sys.argv[1]) + float(sys.argv[2]))
PY
)"

  local stop_env=("${common_env[@]}" "RIFF_EXPERIMENT_DURATION_SEC=$duration_sec")
  if [[ "$case_transcription_mode" == "stub" ]]; then
    stop_env+=('RIFF_TRANSCRIBE_CMD=printf "synthetic transcript %ss\n" "$RIFF_EXPERIMENT_DURATION_SEC" > {out_txt}')
  fi

  measure_command stop_wall_ms stop_status \
    env "${stop_env[@]}" \
    "$RIFF_BIN" --quiet --json stop > "$stop_json"

  "$PYTHON_BIN" - "$start_json" "$stop_json" "$RESULTS_JSONL" "$REAL_FFPROBE" "$mode" "$start_wall_ms" "$stop_wall_ms" "$duration_sec" "$iteration" "$case_transcription_mode" "$live_flag" "$server_flag" "$stop_status" "$case_dir" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

start_path = Path(sys.argv[1])
stop_path = Path(sys.argv[2])
results_path = Path(sys.argv[3])
ffprobe = sys.argv[4]
mode = sys.argv[5]
start_wall_ms = float(sys.argv[6])
stop_wall_ms = float(sys.argv[7])
requested_duration_sec = int(sys.argv[8])
iteration = int(sys.argv[9])
transcription_mode = sys.argv[10]
live_transcribe = bool(int(sys.argv[11]))
server_enabled = bool(int(sys.argv[12]))
stop_status = int(sys.argv[13])
case_dir = sys.argv[14]

start = json.load(start_path.open())
stop = {}
if stop_path.exists() and stop_path.stat().st_size > 0:
    stop = json.load(stop_path.open())

audio_path = Path(start["audio_path"])

def dominant_phase(phases):
    if not phases:
        return {"name": None, "ms": 0.0}
    name, value = max(phases.items(), key=lambda kv: float(kv[1]))
    return {"name": name, "ms": round(float(value), 3)}

audio_duration = None
out = subprocess.run(
    [
        ffprobe,
        "-v", "error",
        "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1:nokey=1",
        str(audio_path),
    ],
    capture_output=True,
    text=True,
)
if out.returncode == 0:
    try:
        audio_duration = round(float(out.stdout.strip()), 3)
    except Exception:
        audio_duration = None

transcription = stop.get("transcription", {}) or {}
transcript_path = Path(start["session_dir"]) / "transcript.txt"
transcript_text = transcript_path.read_text().strip() if transcript_path.exists() else ""

record = {
    "mode": mode,
    "duration_sec": requested_duration_sec,
    "iteration": iteration,
    "status": "ok" if stop_status == 0 else "stop_failed",
    "session_id": start.get("session_id"),
    "session_dir": start.get("session_dir"),
    "audio_duration_sec": audio_duration,
    "start_ms": start.get("startup_ms"),
    "stop_ms": stop.get("stop_ms"),
    "start_wall_ms": start_wall_ms,
    "stop_wall_ms": stop_wall_ms,
    "start_phases": start.get("phases", {}),
    "stop_phases": stop.get("phases", {}),
    "start_dominant_phase": dominant_phase(start.get("phases", {})),
    "stop_dominant_phase": dominant_phase(stop.get("phases", {})),
    "transcription_mode": transcription_mode,
    "live_transcribe": live_transcribe,
    "server_enabled": server_enabled,
    "transcription_status": transcription.get("status"),
    "transcription_method": transcription.get("method"),
    "transcription_perf": transcription.get("perf"),
    "transcript_chars": len(transcript_text),
    "transcript_preview": transcript_text[:160],
    "artifacts": {
        "case_dir": case_dir,
        "start_json": str(start_path),
        "stop_json": str(stop_path),
        "audio_path": str(audio_path),
        "transcript_path": str(transcript_path),
    },
}

with results_path.open("a") as fh:
    fh.write(json.dumps(record) + "\n")
PY

  echo "  session_id=$session_id"
  echo "  session_dir=$session_dir"
  echo "  audio_path=$audio_path"
}

for mode in "${MODES[@]}"; do
  for duration_sec in "${DURATIONS[@]}"; do
    if ! [[ "$duration_sec" =~ ^[1-9][0-9]*$ ]]; then
      echo "Invalid duration: $duration_sec" >&2
      exit 1
    fi
    for iteration in $(seq 1 "$ITERATIONS"); do
      run_case "$mode" "$duration_sec" "$iteration"
      sleep 1
    done
  done
done

"$PYTHON_BIN" - "$RESULTS_JSONL" "$REPORT_JSON" "$SUMMARY_TXT" "$WORK_DIR" "$BENCH_ROOT" "$BASE_SAMPLE_AUDIO" <<'PY'
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

results_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
summary_path = Path(sys.argv[3])
work_dir = sys.argv[4]
bench_root = sys.argv[5]
sample_audio = sys.argv[6]

records = [json.loads(line) for line in results_path.read_text().splitlines() if line.strip()]
groups = defaultdict(list)
for record in records:
    groups[(record["mode"], int(record["duration_sec"]))].append(record)

summary = []
lines = []
header = (
    "mode                   duration_s  runs  status        start_avg_ms  stop_avg_ms  "
    "start_wall_ms  stop_wall_ms  transcription     stop_dom_phase"
)
lines.append(header)
lines.append("-" * len(header))

def avg(values):
    return round(statistics.mean(values), 3) if values else None

for (mode, duration), items in sorted(groups.items()):
    ok_items = [item for item in items if item.get("status") == "ok"]
    latest = items[-1]
    row = {
        "mode": mode,
        "duration_sec": duration,
        "runs": len(items),
        "ok_runs": len(ok_items),
        "latest_status": latest.get("status"),
        "start_avg_ms": avg([float(item["start_ms"]) for item in ok_items if item.get("start_ms") is not None]),
        "stop_avg_ms": avg([float(item["stop_ms"]) for item in ok_items if item.get("stop_ms") is not None]),
        "start_wall_avg_ms": avg([float(item["start_wall_ms"]) for item in ok_items if item.get("start_wall_ms") is not None]),
        "stop_wall_avg_ms": avg([float(item["stop_wall_ms"]) for item in ok_items if item.get("stop_wall_ms") is not None]),
        "latest_transcription_method": latest.get("transcription_method"),
        "latest_stop_dominant_phase": (latest.get("stop_dominant_phase") or {}).get("name"),
    }
    summary.append(row)
    lines.append(
        f"{mode:<22} {duration:>10}  {len(items):>4}  {latest.get('status','n/a'):<12}  "
        f"{(row['start_avg_ms'] if row['start_avg_ms'] is not None else float('nan')):>12.3f}  "
        f"{(row['stop_avg_ms'] if row['stop_avg_ms'] is not None else float('nan')):>11.3f}  "
        f"{(row['start_wall_avg_ms'] if row['start_wall_avg_ms'] is not None else float('nan')):>13.3f}  "
        f"{(row['stop_wall_avg_ms'] if row['stop_wall_avg_ms'] is not None else float('nan')):>12.3f}  "
        f"{(latest.get('transcription_method') or 'n/a'):<16} "
        f"{((latest.get('stop_dominant_phase') or {}).get('name') or 'n/a')}"
    )

report = {
    "work_dir": work_dir,
    "riff_root": bench_root,
    "sample_audio": sample_audio,
    "results_jsonl": str(results_path),
    "summary": summary,
    "runs": records,
}
report_path.write_text(json.dumps(report, indent=2) + "\n")
summary_path.write_text("\n".join(lines) + "\n")
print("\n".join(lines))
print()
print(f"Detailed report: {report_path}")
print(f"Raw results:     {results_path}")
print(f"Workspace:       {work_dir}")
PY
