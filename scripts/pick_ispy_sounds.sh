#!/usr/bin/env bash
set -euo pipefail

# TUI picker for ISPY_BEEP_START / ISPY_BEEP_STOP
# Controls:
#   ↑/↓ or j/k  move selection
#   p or space  preview selected sound
#   1           set START sound to selected item
#   2           set STOP sound to selected item
#   Esc or s    save + exit
#   q           quit without saving

SYSTEM_DIR="/System/Library/Sounds"
USER_DIR="$HOME/Library/Sounds"
ZSHRC="$HOME/.zshrc"
MARKER_START="# >>> ispy-beeps >>>"
MARKER_END="# <<< ispy-beeps <<<"

if ! command -v afplay >/dev/null 2>&1; then
  echo "Error: afplay not found (macOS required)." >&2
  exit 1
fi

declare -a SOUND_NAMES=()
declare -a SOUND_PATHS=()

selected=0
start_idx=-1
stop_idx=-1
start_count=1
stop_count=1
gap_sec="0.08"
status_msg="Use ↑/↓, p/space, 1(start cycle), 2(stop cycle), +/- gap, Esc/s(save), q(quit)"
play_pid=""

cleanup() {
  stty sane 2>/dev/null || true
  if [[ -n "${play_pid:-}" ]]; then
    kill "$play_pid" 2>/dev/null || true
  fi
  printf '\033[?25h' # show cursor
}
trap cleanup EXIT INT TERM

add_sound_file() {
  local path="$1"
  [[ -f "$path" ]] || return 0
  local base name i
  base="$(basename "$path")"
  name="${base%.*}"

  for i in "${!SOUND_NAMES[@]}"; do
    if [[ "${SOUND_NAMES[$i]}" == "$name" ]]; then
      return 0
    fi
  done

  SOUND_NAMES+=("$name")
  SOUND_PATHS+=("$path")
}

load_sounds() {
  local f
  if [[ -d "$SYSTEM_DIR" ]]; then
    while IFS= read -r -d '' f; do
      add_sound_file "$f"
    done < <(find "$SYSTEM_DIR" -maxdepth 1 -type f \( -name '*.aiff' -o -name '*.wav' -o -name '*.caf' \) -print0 | sort -z)
  fi

  if [[ -d "$USER_DIR" ]]; then
    while IFS= read -r -d '' f; do
      add_sound_file "$f"
    done < <(find "$USER_DIR" -maxdepth 1 -type f \( -name '*.aiff' -o -name '*.wav' -o -name '*.caf' -o -name '*.m4a' -o -name '*.mp3' \) -print0 | sort -z)
  fi
}

name_to_index() {
  local want="$1" i
  [[ -n "$want" ]] || { echo -1; return 0; }
  for i in "${!SOUND_NAMES[@]}"; do
    if [[ "${SOUND_NAMES[$i]}" == "$want" ]]; then
      echo "$i"
      return 0
    fi
  done
  echo -1
}

clamp_count() {
  local n="$1"
  if ! [[ "$n" =~ ^[0-9]+$ ]]; then
    echo 1
    return
  fi
  if (( n < 1 )); then
    echo 1
  elif (( n > 3 )); then
    echo 3
  else
    echo "$n"
  fi
}

clamp_gap() {
  local g="$1"
  awk -v x="$g" 'BEGIN {
    if (x !~ /^[0-9]+(\.[0-9]+)?$/) x=0.08;
    if (x < 0) x = 0;
    if (x > 1) x = 1;
    printf "%.2f", x;
  }'
}

adjust_gap() {
  local delta="$1"
  gap_sec="$(awk -v x="$gap_sec" -v d="$delta" 'BEGIN {
    x += d;
    if (x < 0) x = 0;
    if (x > 1) x = 1;
    printf "%.2f", x;
  }')"
}

repeat_char() {
  local ch="$1" count="$2" out="" i
  for ((i=0; i<count; i++)); do
    out+="$ch"
  done
  printf '%s' "$out"
}

load_existing_config() {
  local existing_start existing_stop existing_start_count existing_stop_count existing_gap
  existing_start="${ISPY_BEEP_START:-}"
  existing_stop="${ISPY_BEEP_STOP:-}"
  existing_start_count="${ISPY_BEEP_START_COUNT:-}"
  existing_stop_count="${ISPY_BEEP_STOP_COUNT:-}"
  existing_gap="${ISPY_BEEP_GAP_SEC:-}"

  if [[ ( -z "$existing_start" || -z "$existing_stop" || -z "$existing_start_count" || -z "$existing_stop_count" || -z "$existing_gap" ) && -f "$ZSHRC" ]]; then
    local block
    block="$(awk -v s="$MARKER_START" -v e="$MARKER_END" '
      $0==s {in=1; next}
      $0==e {in=0; next}
      in==1 {print}
    ' "$ZSHRC" 2>/dev/null || true)"

    if [[ -z "$existing_start" ]]; then
      existing_start="$(printf '%s\n' "$block" | awk -F'"' '/^export ISPY_BEEP_START=/{print $2; exit}')"
    fi
    if [[ -z "$existing_stop" ]]; then
      existing_stop="$(printf '%s\n' "$block" | awk -F'"' '/^export ISPY_BEEP_STOP=/{print $2; exit}')"
    fi
    if [[ -z "$existing_start_count" ]]; then
      existing_start_count="$(printf '%s\n' "$block" | awk -F'=' '/^export ISPY_BEEP_START_COUNT=/{print $2; exit}')"
    fi
    if [[ -z "$existing_stop_count" ]]; then
      existing_stop_count="$(printf '%s\n' "$block" | awk -F'=' '/^export ISPY_BEEP_STOP_COUNT=/{print $2; exit}')"
    fi
    if [[ -z "$existing_gap" ]]; then
      existing_gap="$(printf '%s\n' "$block" | awk -F'=' '/^export ISPY_BEEP_GAP_SEC=/{print $2; exit}')"
    fi
  fi

  existing_gap="${existing_gap%\"}"
  existing_gap="${existing_gap#\"}"

  start_idx="$(name_to_index "$existing_start")"
  stop_idx="$(name_to_index "$existing_stop")"

  if (( start_idx < 0 )); then
    start_idx="$(name_to_index Ping)"
  fi
  if (( stop_idx < 0 )); then
    stop_idx="$(name_to_index Glass)"
  fi

  start_count="$(clamp_count "${existing_start_count:-1}")"
  stop_count="$(clamp_count "${existing_stop_count:-1}")"
  gap_sec="$(clamp_gap "${existing_gap:-0.08}")"

  if (( selected < 0 || selected >= ${#SOUND_NAMES[@]} )); then
    selected=0
  fi
}

render() {
  printf '\033[2J\033[H'
  echo "ispy sound picker"
  echo
  echo "Controls: ↑/↓ or j/k | p/space play | 1 start(cycle 1..3) | 2 stop(cycle 1..3) | +/- gap | Esc/s save+exit | q quit"
  echo

  local sname tname
  sname="${SOUND_NAMES[$start_idx]:-unset}"
  tname="${SOUND_NAMES[$stop_idx]:-unset}"
  printf 'START: %s x%s   STOP: %s x%s   GAP: %ss\n' "$sname" "$start_count" "$tname" "$stop_count" "$gap_sec"
  echo

  local i marker flags line s_marks t_marks
  for i in "${!SOUND_NAMES[@]}"; do
    marker="  "
    (( i == selected )) && marker="> "

    s_marks=""
    t_marks=""
    (( i == start_idx )) && s_marks="$(repeat_char S "$start_count")"
    (( i == stop_idx )) && t_marks="$(repeat_char T "$stop_count")"

    if [[ -n "$s_marks" && -n "$t_marks" ]]; then
      flags="$s_marks|$t_marks"
    elif [[ -n "$s_marks" ]]; then
      flags="$s_marks"
    elif [[ -n "$t_marks" ]]; then
      flags="$t_marks"
    else
      flags=" "
    fi

    line="${SOUND_NAMES[$i]}"
    printf '%s[%-7s] %s\n' "$marker" "$flags" "$line"
  done

  echo
  echo "$status_msg"
}

play_selected() {
  local path name preview_count source
  path="${SOUND_PATHS[$selected]}"
  name="${SOUND_NAMES[$selected]}"

  preview_count=1
  source="default"

  if (( selected == start_idx && selected == stop_idx )); then
    preview_count=$((start_count + stop_count))
    source="start+stop"
  elif (( selected == start_idx )); then
    preview_count=$start_count
    source="start"
  elif (( selected == stop_idx )); then
    preview_count=$stop_count
    source="stop"
  fi

  if [[ -n "${play_pid:-}" ]]; then
    kill "$play_pid" 2>/dev/null || true
    wait "$play_pid" 2>/dev/null || true
  fi

  (
    set +e
    local i child
    local -a children=()

    cleanup_children() {
      local cp
      for cp in "${children[@]}"; do
        kill "$cp" 2>/dev/null || true
      done
    }
    trap cleanup_children INT TERM

    for ((i=1; i<=preview_count; i++)); do
      afplay "$path" >/dev/null 2>&1 &
      child="$!"
      children+=("$child")

      if (( i < preview_count )); then
        sleep "$gap_sec"
      fi
    done

    for child in "${children[@]}"; do
      wait "$child" 2>/dev/null || true
    done
  ) &
  play_pid="$!"

  if [[ "$source" == "default" ]]; then
    status_msg="Previewing: $name x$preview_count (interval ${gap_sec}s)"
  else
    status_msg="Previewing: $name x$preview_count ($source, interval ${gap_sec}s)"
  fi
}

write_zshrc_block() {
  local start_sound="$1"
  local stop_sound="$2"
  local start_n="$3"
  local stop_n="$4"
  local gap="$5"
  local tmp
  tmp="$(mktemp)"

  if [[ -f "$ZSHRC" ]]; then
    awk -v s="$MARKER_START" -v e="$MARKER_END" '
      $0==s {skip=1; next}
      $0==e {skip=0; next}
      skip==0 {print}
    ' "$ZSHRC" > "$tmp"
  fi

  cat >>"$tmp" <<EOF
$MARKER_START
export ISPY_BEEP=1
export ISPY_BEEP_START="$start_sound"
export ISPY_BEEP_STOP="$stop_sound"
export ISPY_BEEP_START_COUNT=$start_n
export ISPY_BEEP_STOP_COUNT=$stop_n
export ISPY_BEEP_GAP_SEC="$gap"
$MARKER_END
EOF

  mv "$tmp" "$ZSHRC"
}

save_and_exit() {
  local start_sound stop_sound
  start_sound="${SOUND_NAMES[$start_idx]:-Ping}"
  stop_sound="${SOUND_NAMES[$stop_idx]:-Glass}"

  write_zshrc_block "$start_sound" "$stop_sound" "$start_count" "$stop_count" "$gap_sec"

  printf '\033[2J\033[H'
  echo "Saved ispy beep config:"
  echo "  START: $start_sound x$start_count"
  echo "  STOP : $stop_sound x$stop_count"
  echo "  GAP  : ${gap_sec}s"
  echo
  echo "Run: source ~/.zshrc"
  exit 0
}

main() {
  if [[ ! -t 0 || ! -t 1 ]]; then
    echo "This picker must be run in an interactive terminal (TTY)." >&2
    exit 1
  fi

  load_sounds
  if (( ${#SOUND_NAMES[@]} == 0 )); then
    echo "No sounds found in $SYSTEM_DIR or $USER_DIR" >&2
    exit 1
  fi

  load_existing_config

  # raw input mode
  # time=1 => read waits up to 0.1s for the next byte (helps ESC parsing).
  stty -echo -icanon time 1 min 0
  printf '\033[?25l' # hide cursor

  while true; do
    render

    local key rest
    IFS= read -rsn1 key || true
    if [[ -z "$key" ]]; then
      sleep 0.03
      continue
    fi

    if [[ "$key" == $'\x1b' ]]; then
      # ESC alone => save/exit. Arrow keys usually send ESC [ A/B (or ESC O A/B).
      local r1="" r2=""
      IFS= read -rsn1 r1 || true

      if [[ -z "$r1" ]]; then
        save_and_exit
      fi

      if [[ "$r1" == "[" || "$r1" == "O" ]]; then
        IFS= read -rsn1 r2 || true
        case "$r2" in
          A) # up
            selected=$(( (selected - 1 + ${#SOUND_NAMES[@]}) % ${#SOUND_NAMES[@]} ))
            status_msg="Selected: ${SOUND_NAMES[$selected]}"
            ;;
          B) # down
            selected=$(( (selected + 1) % ${#SOUND_NAMES[@]} ))
            status_msg="Selected: ${SOUND_NAMES[$selected]}"
            ;;
          "")
            # partial sequence; treat as plain escape for reliability
            save_and_exit
            ;;
          *)
            status_msg="Unhandled ESC sequence: ESC${r1}${r2}"
            ;;
        esac
      else
        # Unknown ESC sequence; treat as Escape key so user can always exit.
        save_and_exit
      fi
      continue
    fi

    case "$key" in
      k)
        selected=$(( (selected - 1 + ${#SOUND_NAMES[@]}) % ${#SOUND_NAMES[@]} ))
        status_msg="Selected: ${SOUND_NAMES[$selected]}"
        ;;
      j)
        selected=$(( (selected + 1) % ${#SOUND_NAMES[@]} ))
        status_msg="Selected: ${SOUND_NAMES[$selected]}"
        ;;
      " ")
        play_selected
        ;;
      p|P)
        play_selected
        ;;
      1)
        if (( start_idx == selected )); then
          start_count=$(( (start_count % 3) + 1 ))
        else
          start_idx=$selected
          start_count=1
        fi
        status_msg="START set: ${SOUND_NAMES[$start_idx]} x$start_count (press 1 again to cycle)"
        ;;
      2)
        if (( stop_idx == selected )); then
          stop_count=$(( (stop_count % 3) + 1 ))
        else
          stop_idx=$selected
          stop_count=1
        fi
        status_msg="STOP set: ${SOUND_NAMES[$stop_idx]} x$stop_count (press 2 again to cycle)"
        ;;
      +|=)
        adjust_gap 0.02
        status_msg="Gap increased to ${gap_sec}s"
        ;;
      -|_)
        adjust_gap -0.02
        status_msg="Gap decreased to ${gap_sec}s"
        ;;
      s|S)
        save_and_exit
        ;;
      q|Q)
        printf '\033[2J\033[H'
        echo "Canceled (no changes saved)."
        exit 0
        ;;
      *)
        status_msg="Unhandled key (code $(printf '%d' "'${key}" 2>/dev/null || echo '?'))"
        ;;
    esac
  done
}

main "$@"
