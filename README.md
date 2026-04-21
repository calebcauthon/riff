# riff (Rust + local Parakeet)

Minimal local dictation CLI for macOS.

Workflow:

1. `riff start` (or `riff toggle` when idle)
2. Take screenshots with `riff shot` (recommended) or normal `Cmd+Shift+4`
3. `riff stop` (or `riff toggle` when active)

On `stop`, riff:
- stops audio recording
- finds screenshots created during the session in your normal screenshot folder
- copies them to session tmp storage (`/tmp/riff/sessions/<session-id>/screenshots`)
- deletes the originals from your normal screenshot folder
- captures copied clipboard text during the session
- runs local transcription (Parakeet via Python script / warm local server)
- writes `note.md` with `[Screenshot N]` / `[Clipboard N]` markers + footnotes
- writes `note.html` with metadata, transcript, and image preview gallery
- auto-starts local web server (idle-timeout) for richer HTML behavior

---

## Files

```text
/tmp/riff/sessions/<session-id>/
  audio.wav
  events.jsonl
  ffmpeg.log
  transcript.txt        (if transcription succeeded)
  note.md
  note.html
  screenshots/
    shot-001.png
    ...
```

Performance/observability logs:

```text
/tmp/riff/perf.jsonl                # structured start/stop phase timings
/tmp/riff/parakeet-server.log       # warm Parakeet server logs
/tmp/riff/web-server.log            # local HTML web server logs
/tmp/riff/toggle-hotkey.log         # hotkey toggle/stop/send lifecycle
```

---

## Install

```bash
cd ~/Code/riff
chmod +x riff
```

`riff` is a wrapper script that builds/runs the Rust binary.
If `RIFF_PYTHON_BIN` is not set, it auto-prefers:
1. `~/Code/riff/runtime/python/bin/python` (bundled runtime)
2. `~/Code/riff/.venv/bin/python` (dev venv)
3. `python3` from PATH

Performance note:
- `riff start` warms a local Parakeet server in the background (when enabled), so later `riff stop` calls are faster.
- `riff stop` auto-starts a local HTML web server with idle-timeout for richer session pages.

Optional PATH link:

```bash
mkdir -p ~/bin
ln -sf ~/Code/riff/riff ~/bin/riff
```

---

## Requirements

- macOS
- Rust/Cargo
- `ffmpeg` (+ optional `ffprobe`) in PATH
- Python env for Parakeet transcription

Install ffmpeg:

```bash
brew install ffmpeg
```

---

## One-time Parakeet setup (Python, dev venv)

Create a local venv and install dependencies (**use Python 3.12 preferred; 3.10-3.12 supported**):

```bash
cd ~/Code/riff
python3.12 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
pip install nemo_toolkit[asr] torch soundfile
```

If you only have Python 3.14 installed, install 3.12 first:

```bash
brew install python@3.12
```

Set env vars (add to `~/.zshrc` if desired):

```bash
export RIFF_PYTHON_BIN="$HOME/Code/riff/.venv/bin/python"
export RIFF_PARAKEET_SCRIPT="$HOME/Code/riff/scripts/parakeet_transcribe.py"
export RIFF_PARAKEET_MODEL="nvidia/parakeet-tdt-0.6b-v2"
# optional perf + warm server controls
export RIFF_PARAKEET_SERVER=1
export RIFF_PARAKEET_SERVER_URL="http://127.0.0.1:8765"

# optional local HTML server controls
export RIFF_WEB_SERVER=1
export RIFF_WEB_SERVER_URL="http://127.0.0.1:8766"
export RIFF_WEB_SERVER_IDLE_TIMEOUT_SEC=1800

# optional clipboard monitor controls
export RIFF_CLIPBOARD_MONITOR=1
```

---

## Bundled private Python runtime (recommended for distribution)

Use this when you want to package `riff` for another machine without relying on that machine's system Python.

Create the bundled runtime (full Python distribution copy, not a venv):

```bash
cd ~/Code/riff
brew install uv
uv python install 3.12
./scripts/build_bundled_python_runtime.sh
```

Build the Rust binary:

```bash
cd ~/Code/riff
cargo build --release
```

Create a full distribution artifact tarball (runtime + binary + scripts):

```bash
cd ~/Code/riff
./scripts/create_distribution_artifact.sh
```

This writes `dist/riff-<platform>-<sha>-<timestamp>.tar.gz` plus a `.sha256` file.

Package these paths together:

```text
riff
target/release/riff
scripts/
runtime/python/
```

When run from that packaged root, `riff` will auto-use `runtime/python/bin/python` first.
This runtime is no-system-dependency for Python itself, but it is still platform/architecture-specific
(build on same OS/CPU family you intend to run).

Optional script flags:

```bash
# force a specific uv-managed source interpreter
./scripts/build_bundled_python_runtime.sh --source-python "$HOME/.local/bin/python3.12"

# copy runtime only (skip package install)
./scripts/build_bundled_python_runtime.sh --skip-install

# allow non-relocatable runtime sources (not recommended for distribution)
./scripts/build_bundled_python_runtime.sh --allow-nonrelocatable --source-python /opt/homebrew/bin/python3.12
```

---

## Commands

### Start

```bash
riff start
```

Flags:
- `--screenshot-dir <path>` override screenshot source dir
- `--audio-device <selector>` ffmpeg avfoundation selector (default `auto`, prefers built-in Mac mic and avoids iPhone/Continuity)

You can also set a fixed selector:

```bash
export RIFF_AUDIO_DEVICE=":1"
```

### Shot (capture directly into active session)

```bash
riff shot
```

Uses macOS `screencapture -i` and writes directly to the active session's `screenshots/` folder.
This avoids delayed Desktop screenshot writes and floating thumbnail timing issues.

### Stop

```bash
riff stop
```

Stops recording and processes the session (transcription, note/html generation, screenshots, etc.).
It does **not** send output to the focused app.

Flags:
- `--python-bin <path>` override python interpreter
- `--parakeet-script <path>` override script path
- `--parakeet-model <name>` override model name
- `--transcribe-cmd '<template>'` custom transcription command (advanced)

`--transcribe-cmd` placeholders:
- `{audio}`
- `{out_base}`
- `{out_txt}`
- `{session_dir}`

### Toggle (start if idle, stop if active)

```bash
riff toggle
```

Useful when you want one command instead of separate `start`/`stop`.

Flags:
- Start-path flags (used when idle): `--screenshot-dir`, `--audio-device`
- Stop-path flags (used when active): `--python-bin`, `--parakeet-script`, `--parakeet-model`, `--transcribe-cmd`

### Sounds (interactive picker)

```bash
riff sounds
```

Browse system/user sounds, preview each, and set start/stop beep choices.

Picker controls:
- `↑/↓` (or `j/k`) move selection
- `p` or `space` preview selected sound (uses configured repeat count for selected START/STOP sound)
- `1` set START sound (press `1` again on same sound to cycle repeats `x1 -> x2 -> x3`)
- `2` set STOP sound (press `2` again on same sound to cycle repeats `x1 -> x2 -> x3`)
- `+` / `-` increase/decrease delay between repeated beeps
- `Esc` (or `s`) save + exit
- `q` quit without saving

### Status

```bash
riff status
```

### Perf (startup/shutdown timing summary)

```bash
riff perf        # recent 40 records
riff perf 100    # recent 100 records
```

Summarizes `start`/`stop` timings from `/tmp/riff/perf.jsonl` (count, avg, p50, p95) and shows recent entries with dominant phase.

### List recent sessions

```bash
riff list 10
```

Shows a terminal table with:
- readable timestamp (`mon 4-10 4:32pm` style)
- transcript summary (`first 3 words..last 3 words [n words]`)
- image count
- dictation length

### Copy session transcript to stdout

```bash
riff copy        # most recent (same as copy 1)
riff copy 3      # 3rd most recent
```

Outputs only the transcript section to stdout (pipe to pbcopy, files, etc.):

```bash
riff copy | pbcopy
```

### Send transcript to focused app

```bash
riff send        # most recent (same as send 1)
riff send 2      # 2nd most recent
```

Copies the transcript to clipboard and immediately pastes it into the currently focused app.

### Show full session markdown to stdout

```bash
riff show 20260413-013011
```

`show` now takes a session id (not a numeric index). Use `riff list` to find ids.
Outputs raw `note.md` markdown for that session.

### Open session HTML report

```bash
riff html        # most recent (same as html 1)
riff html 2      # 2nd most recent
```

Behavior:
- regenerates HTML file
- ensures local web server is running
- resets web server idle timer (`/touch`)
- prints HTML filesystem path to stdout
- prints `Opening <target>`
- opens served URL (falls back to file path if server unavailable)
- HTML page includes:
  - `Copy markdown` button
  - `Copy transcript` button
  - `Copy image` button on each screenshot card (falls back to copying image path if image clipboard API is unavailable)
  - `Browse all sessions` link to `/sessions/index.html` (one transcript row per session with screenshot thumbnails)

---

## Global flags

- `--verbose`
- `--quiet`
- `--json`
- `--dry-run`

Examples:

```bash
riff --dry-run start
riff --dry-run stop
riff --json status
```

---

## Latency instrumentation

Every start/stop writes structured timings to:

```bash
cat /tmp/riff/perf.jsonl
```

Tail live while testing hotkeys:

```bash
tail -f /tmp/riff/perf.jsonl
```

Focus on these fields:
- start: `phases.spawn_recorder_ms`
- stop: `phases.transcribe_ms` (usually the biggest)
- stop: `phases.web_server_ms` (local HTML server startup/health)
- stop: `phases.generate_index_ms` (sessions index rebuild cost)
- stop: `phases.write_note_html_ms` (note/html file write cost)
- stop: `transcription_perf.execution_path` (`parakeet`, `custom_command`, etc.)
- stop: `transcription_perf.server_ensure_ms` (time spent waiting for Parakeet server readiness)
- stop: `transcription_perf.python_transcribe_ms` (one-shot fallback cost when server isn’t used)
- stop: `transcription_perf.server_health_before` / `server_health_after` (server availability before/after ensure)

If `transcription_perf.server_pid_alive` is `false`, inspect:

```bash
tail -n 120 /tmp/riff/parakeet-server.log
```

Index generation tuning:

```bash
export RIFF_SESSIONS_INDEX_LIMIT=500   # default 500, range 1..5000
```

---

## Audio cues (start/stop beeps)

By default, successful start/stop plays two different macOS sounds:
- start: `Ping`
- stop: `Glass`

Customize or disable:

```bash
export RIFF_BEEP=1                 # default on (set 0 to disable)
export RIFF_BEEP_START="Ping"     # name in /System/Library/Sounds or absolute path
export RIFF_BEEP_STOP="Glass"     # name in /System/Library/Sounds or absolute path
export RIFF_BEEP_START_COUNT=1     # 1..3 repeats
export RIFF_BEEP_STOP_COUNT=1      # 1..3 repeats
export RIFF_BEEP_GAP_SEC=0.08      # launch interval between repeats (0.00..1.00); lower values overlap beeps
```

Interactive picker (preview + choose start/stop sounds):

```bash
riff sounds
```

---

## Hotkeys

Current skhd setup on this machine:
- `alt + /` (keycode `alt - 0x2C`) → toggle start/stop via `riff toggle`
- `cmd + alt + d` → fallback toggle
- `cmd + s` → hard fallback toggle
- `cmd + alt + 9` → `riff shot`

Suggested skhd keybind setup (native commands + inline env):

```text
# riff toggle: start if idle, stop if active
alt - 0x2C : env RIFF_ROOT=/tmp/riff RIFF_PYTHON_BIN=$HOME/Code/riff/.venv/bin/python RIFF_PARAKEET_SCRIPT=$HOME/Code/riff/scripts/parakeet_transcribe.py RIFF_PARAKEET_MODEL=nvidia/parakeet-tdt-0.6b-v2 $HOME/Code/riff/target/release/riff --quiet toggle >> /tmp/riff/toggle-hotkey.log 2>&1

# riff toggle + send: stop, then send transcript to focused app
alt - 0x27 : env RIFF_ROOT=/tmp/riff RIFF_PYTHON_BIN=$HOME/Code/riff/.venv/bin/python RIFF_PARAKEET_SCRIPT=$HOME/Code/riff/scripts/parakeet_transcribe.py RIFF_PARAKEET_MODEL=nvidia/parakeet-tdt-0.6b-v2 $HOME/Code/riff/target/release/riff --quiet toggle && env RIFF_ROOT=/tmp/riff RIFF_PYTHON_BIN=$HOME/Code/riff/.venv/bin/python RIFF_PARAKEET_SCRIPT=$HOME/Code/riff/scripts/parakeet_transcribe.py RIFF_PARAKEET_MODEL=nvidia/parakeet-tdt-0.6b-v2 $HOME/Code/riff/target/release/riff --quiet send >> /tmp/riff/toggle-hotkey.log 2>&1

# riff toggle + open html: stop, then open latest note.html
alt - 0x29 : env RIFF_ROOT=/tmp/riff RIFF_PYTHON_BIN=$HOME/Code/riff/.venv/bin/python RIFF_PARAKEET_SCRIPT=$HOME/Code/riff/scripts/parakeet_transcribe.py RIFF_PARAKEET_MODEL=nvidia/parakeet-tdt-0.6b-v2 $HOME/Code/riff/target/release/riff --quiet toggle && env RIFF_ROOT=/tmp/riff RIFF_PYTHON_BIN=$HOME/Code/riff/.venv/bin/python RIFF_PARAKEET_SCRIPT=$HOME/Code/riff/scripts/parakeet_transcribe.py RIFF_PARAKEET_MODEL=nvidia/parakeet-tdt-0.6b-v2 $HOME/Code/riff/target/release/riff --quiet html >> /tmp/riff/toggle-hotkey.log 2>&1
```

Toggle behavior:
- `0x2C`: if inactive start; if active stop
- `0x27`: if inactive start; if active stop then send
- `0x29`: if inactive start; if active stop then open HTML

Use Raycast, Alfred, Hammerspoon, Keyboard Maestro, etc. if you prefer a different launcher.

---

## Troubleshooting Parakeet import errors

If you see dependency/import errors even after pip install, check your Python version:

```bash
$RIFF_PYTHON_BIN -V
```

If it's `3.13+` (especially 3.14), recreate your environment with Python 3.12.

For dev venv:

```bash
cd ~/Code/riff
rm -rf .venv
python3.12 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
pip install nemo_toolkit[asr] torch soundfile
```

For bundled runtime:

```bash
cd ~/Code/riff
uv python install 3.12
./scripts/build_bundled_python_runtime.sh --python-version 3.12
```

Check bundled runtime directly:

```bash
~/Code/riff/runtime/python/bin/python -V
```
