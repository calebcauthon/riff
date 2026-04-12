#!/usr/bin/env bash
set -euo pipefail

DICTATE_BIN="${DICTATE_BIN:-/Users/caleb/Code/ispy/dictate}"
LOG_ROOT="${ISPY_ROOT:-/tmp/ispy}"
LOG_FILE="$LOG_ROOT/toggle-hotkey.log"

mkdir -p "$LOG_ROOT"

ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

log() {
  printf '[%s] %s\n' "$(ts)" "$*" >>"$LOG_FILE"
}

is_active() {
  "$DICTATE_BIN" --json --quiet status 2>/dev/null | grep -q '"active": true'
}

paste_clipboard() {
  # Small delay improves reliability for some focused apps.
  sleep 0.08
  /usr/bin/osascript -e 'tell application "System Events" to keystroke "v" using command down'
}

if is_active; then
  log "toggle: active=true -> stopping session"
  "$DICTATE_BIN" --quiet stop >>"$LOG_FILE" 2>&1

  # copy command emits transcript text to stdout
  if "$DICTATE_BIN" copy | pbcopy; then
    log "toggle: transcript copied to clipboard"
    if paste_clipboard >>"$LOG_FILE" 2>&1; then
      log "toggle: transcript pasted into focused app"
    else
      log "toggle: paste failed (clipboard still contains transcript)"
    fi
  else
    log "toggle: copy failed; skipping paste"
    exit 1
  fi
else
  log "toggle: active=false -> starting session"
  "$DICTATE_BIN" --quiet start >>"$LOG_FILE" 2>&1
  log "toggle: session started"
fi
