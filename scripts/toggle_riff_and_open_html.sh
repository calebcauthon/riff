#!/usr/bin/env bash
set -euo pipefail

RIFF_BIN="${RIFF_BIN:-/Users/caleb/Code/riff/riff}"
LOG_ROOT="${RIFF_ROOT:-/tmp/riff}"
LOG_FILE="$LOG_ROOT/toggle-open-html-hotkey.log"
SESSIONS_DIR="$LOG_ROOT/sessions"

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

latest_session_id() {
  [[ -d "$SESSIONS_DIR" ]] || return 1
  ls -1 "$SESSIONS_DIR" 2>/dev/null | sort | tail -n 1
}

if is_active; then
  t0=$(now_ms)
  log "toggle: active=true -> stopping session"
  "$RIFF_BIN" --quiet stop >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: stop completed in $((t1 - t0))ms"

  if sid="$(latest_session_id)" && [[ -n "$sid" ]]; then
    html_path="$SESSIONS_DIR/$sid/note.html"
    if [[ -f "$html_path" ]]; then
      if open "$html_path" >>"$LOG_FILE" 2>&1; then
        t2=$(now_ms)
        log "toggle: opened html in $((t2 - t1))ms"
        log "toggle: stop+open total $((t2 - t0))ms"
      else
        log "toggle: open failed for $html_path"
        exit 1
      fi
    else
      log "toggle: html file missing at $html_path"
      exit 1
    fi
  else
    log "toggle: could not resolve latest session id"
    exit 1
  fi
else
  t0=$(now_ms)
  log "toggle: active=false -> starting session"
  "$RIFF_BIN" --quiet start >>"$LOG_FILE" 2>&1
  t1=$(now_ms)
  log "toggle: session started in $((t1 - t0))ms"
fi
