#!/usr/bin/env bash
set -euo pipefail

RIFF_BIN="${RIFF_BIN:-/Users/caleb/Code/riff/riff}"
LOG_ROOT="${RIFF_ROOT:-/tmp/riff}"
LOG_FILE="$LOG_ROOT/toggle-send-paste-hotkey.log"

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

paste_text() {
  local text="$1"
  # Use osascript to type the text into the active application
  osascript -e "tell application \"System Events\" to keystroke \"$text\"" 2>/dev/null || {
    # Fallback: copy to clipboard
    echo -n "$text" | pbcopy
    log "paste: copied to clipboard (osascript failed)"
  }
}

if is_active; then
  t0=$(now_ms)
  log "toggle: active=true -> stopping session"
  "$RIFF_BIN" --quiet stop >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: stop completed in $((t1 - t0))ms"

  log "toggle: sending..."
  if output=$("$RIFF_BIN" --quiet send 2>>"$LOG_FILE"); then
    t2=$(now_ms)
    log "toggle: send completed in $((t2 - t1))ms"
    log "toggle: output=$output"
    
    # Paste/type the result
    if [[ -n "$output" ]]; then
      paste_text "$output"
      log "toggle: pasted output"
    fi
    
    log "toggle: stop+send+paste total $((t2 - t0))ms"
  else
    log "toggle: send failed"
    exit 1
  fi
else
  t0=$(now_ms)
  log "toggle: active=false -> starting session"
  "$RIFF_BIN" --quiet start >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: session started in $((t1 - t0))ms"
fi
