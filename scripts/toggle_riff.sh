#!/usr/bin/env bash
set -euo pipefail

RIFF_BIN="${RIFF_BIN:-/Users/caleb/Code/riff/riff}"
LOG_ROOT="${RIFF_ROOT:-/tmp/riff}"
LOG_FILE="$LOG_ROOT/toggle-hotkey.log"

mkdir -p "$LOG_ROOT"

ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log() {
  printf '[%s] %s\n' "$(ts)" "$*" >>"$LOG_FILE"
}

now_ms() {
  perl -MTime::HiRes=time -e 'printf("%.0f", time()*1000)'
}

is_active() {
  "$RIFF_BIN" --json --quiet status 2>/dev/null | grep -q '"active": true'
}

if is_active; then
  t0=$(now_ms)
  log "toggle: active=true -> stopping session"
  "$RIFF_BIN" --quiet stop >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: stop completed in $((t1 - t0))ms"
  log "toggle: stop total $((t1 - t0))ms"
else
  t0=$(now_ms)
  log "toggle: active=false -> starting session"
  "$RIFF_BIN" --quiet start >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: session started in $((t1 - t0))ms"
fi
