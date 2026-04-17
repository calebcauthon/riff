#!/usr/bin/env bash
set -euo pipefail

RIFF_BIN="${RIFF_BIN:-${DICTATE_BIN:-/Users/caleb/Code/riff/riff}}"
LOG_ROOT="${ISPY_ROOT:-/tmp/ispy}"
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

paste_clipboard() {
  # Small delay improves reliability for some focused apps.
  sleep 0.08
  /usr/bin/osascript -e 'tell application "System Events" to keystroke "v" using command down'
}

if is_active; then
  t0=$(now_ms)
  log "toggle: active=true -> stopping session"
  "$RIFF_BIN" --quiet stop >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: stop completed in $((t1 - t0))ms"

  # copy command emits transcript text to stdout
  if "$RIFF_BIN" copy | pbcopy; then
    t2=$(now_ms)
    log "toggle: transcript copied in $((t2 - t1))ms"
    if paste_clipboard >>"$LOG_FILE" 2>&1; then
      t3=$(now_ms)
      log "toggle: transcript pasted into focused app in $((t3 - t2))ms"
      log "toggle: stop+copy+paste total $((t3 - t0))ms"
    else
      log "toggle: paste failed (clipboard still contains transcript)"
    fi
  else
    log "toggle: copy failed; skipping paste"
    exit 1
  fi
else
  t0=$(now_ms)
  log "toggle: active=false -> starting session"
  "$RIFF_BIN" --quiet start >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: session started in $((t1 - t0))ms"
fi
