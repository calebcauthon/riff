# ispy (Rust + local Parakeet)

Minimal local dictation CLI for macOS.

Workflow:

1. `dictate start`
2. Take screenshots with `dictate shot` (recommended) or normal `Cmd+Shift+4`
3. `dictate stop`

On `stop`, ispy:
- stops audio recording
- finds screenshots created during the session in your normal screenshot folder
- copies them to session tmp storage (`/tmp/ispy/sessions/<session-id>/screenshots`)
- deletes the originals from your normal screenshot folder
- runs local transcription (Parakeet via Python script / warm local server)
- writes `note.md` with `[Screenshot N]` markers + footnotes
- writes `note.html` with metadata, transcript, and image preview gallery

---

## Files

```text
/tmp/ispy/sessions/<session-id>/
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
/tmp/ispy/perf.jsonl                # structured start/stop phase timings
/tmp/ispy/parakeet-server.log       # warm server logs
/tmp/ispy/toggle-hotkey.log         # hotkey toggle/copy/paste lifecycle
```

---

## Install

```bash
cd ~/Code/ispy
chmod +x dictate
```

`dictate` is a wrapper script that builds/runs the Rust binary.
If `ISPY_PYTHON_BIN` is not set and `~/Code/ispy/.venv/bin/python` exists, the wrapper auto-uses that venv.

Performance note: `dictate start` now warms a local Parakeet server in the background (when enabled), so later `dictate stop` calls are faster.

Optional PATH link:

```bash
mkdir -p ~/bin
ln -sf ~/Code/ispy/dictate ~/bin/dictate
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

## One-time Parakeet setup (Python)

Create a local venv and install dependencies (**use Python 3.12 preferred; 3.10-3.12 supported**):

```bash
cd ~/Code/ispy
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
export ISPY_PYTHON_BIN="$HOME/Code/ispy/.venv/bin/python"
export ISPY_PARAKEET_SCRIPT="$HOME/Code/ispy/scripts/parakeet_transcribe.py"
export ISPY_PARAKEET_MODEL="nvidia/parakeet-tdt-0.6b-v2"
# optional perf + warm server controls
export ISPY_PARAKEET_SERVER=1
export ISPY_PARAKEET_SERVER_URL="http://127.0.0.1:8765"
```

---

## Commands

### Start

```bash
dictate start
```

Flags:
- `--screenshot-dir <path>` override screenshot source dir
- `--audio-device <selector>` ffmpeg avfoundation selector (default `auto`, prefers built-in Mac mic and avoids iPhone/Continuity)

You can also set a fixed selector:

```bash
export ISPY_AUDIO_DEVICE=":1"
```

### Shot (capture directly into active session)

```bash
dictate shot
```

Uses macOS `screencapture -i` and writes directly to the active session's `screenshots/` folder.
This avoids delayed Desktop screenshot writes and floating thumbnail timing issues.

### Stop

```bash
dictate stop
```

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

### Status

```bash
dictate status
```

### Last session

```bash
dictate last
dictate last --open
```

### List recent sessions

```bash
dictate list 10
```

Shows a terminal table with:
- readable timestamp (`mon 4-10 4:32pm` style)
- transcript summary (`first 3 words..last 3 words [n words]`)
- image count
- dictation length

### Copy session transcript to stdout

```bash
dictate copy        # most recent (same as copy 1)
dictate copy 3      # 3rd most recent
```

Outputs only the transcript section to stdout (pipe to pbcopy, files, etc.):

```bash
dictate copy | pbcopy
```

### Show full session markdown to stdout

```bash
dictate show        # most recent (same as show 1)
dictate show 2      # 2nd most recent
```

Outputs raw `note.md` markdown for the selected session.

### Open session HTML report

```bash
dictate html        # most recent (same as html 1)
dictate html 2      # 2nd most recent
```

Behavior:
- prints HTML path to stdout
- prints `Opening <path>`
- runs `open <path>`
- HTML page includes:
  - `Copy markdown` button
  - `Copy transcript` button
  - `Copy image` button on each screenshot card (falls back to copying image path if image clipboard API is unavailable)

---

## Global flags

- `--verbose`
- `--quiet`
- `--json`
- `--dry-run`

Examples:

```bash
dictate --dry-run start
dictate --dry-run stop
dictate --json status
```

---

## Hotkeys

Current skhd setup on this machine:
- `alt + /` (keycode `alt - 0x2C`) → toggle start/stop via `scripts/toggle_dictate_and_paste.sh`
- `cmd + alt + d` → fallback toggle
- `cmd + s` → hard fallback toggle
- `cmd + alt + 9` → `dictate shot`

Toggle behavior:
- if inactive: starts dictation
- if active: stops dictation, copies transcript to clipboard, and pastes into focused app

Use Raycast, Alfred, Hammerspoon, Keyboard Maestro, etc. if you prefer a different launcher.

---

## Troubleshooting Parakeet import errors

If you see dependency/import errors even after pip install, check your Python version:

```bash
$ISPY_PYTHON_BIN -V
```

If it's `3.13+` (especially 3.14), recreate the venv with Python 3.12:

```bash
cd ~/Code/ispy
rm -rf .venv
python3.12 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
pip install nemo_toolkit[asr] torch soundfile
```
