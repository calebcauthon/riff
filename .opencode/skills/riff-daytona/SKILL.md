---
name: riff-daytona
description: Run and verify the riff dictation CLI inside a Daytona Linux sandbox. Use when asked to build riff, run its session loop (start/shot/stop), run cargo tests, or verify riff works in this Linux sandbox, where real macOS audio capture, screenshots, clipboard, and the Parakeet stack are unavailable.
---

# Running riff in a Daytona sandbox

Riff is macOS-first: audio capture goes through ffmpeg + AVFoundation, screenshots through `screencapture`, clipboard through `pbpaste`, app metadata through `osascript`. A Daytona sandbox is Linux x86_64, so none of those exist and the NeMo/Parakeet model stack is impractical here.

You can still build riff, run `cargo test`, and exercise the entire session lifecycle end-to-end by putting stub macOS tools on `PATH` and stubbing transcription with `RIFF_TRANSCRIBE_CMD` (`--transcribe-cmd`). This is the same technique the test suite uses in `tests/cli_smoke.rs`.

Verified working as of riff 0.7.0: `--version`, `start`, `shot`, `status`, `stop --transcribe-cmd`, `list`, `show`, `doctor`, event bus writes, and `note.md`/`note.html` rendering.

## 1. Install Rust and build

The sandbox has no preinstalled toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cargo build --release   # ~2 minutes
```

## 2. Install stub macOS tools

```bash
bash .opencode/skills/riff-daytona/scripts/install_fake_macos_tools.sh /tmp/riff-fakebin
export PATH=/tmp/riff-fakebin:$PATH
```

Stubs provided: `ffmpeg` (device list + fake recording loop), `screencapture` (writes a tiny valid PNG), `osascript` (prints TestApp window metadata), plus no-op `afplay`, `open`, `pbcopy`, `ps`.

## 3. Sandbox environment for riff

```bash
export RIFF_ROOT=/tmp/riff-daytona          # keep state out of /tmp/riff defaults
export RIFF_WEB_SERVER=0                    # report server off (no browser anyway)
export RIFF_PARAKEET_SERVER=0               # no Python/NeMo warm server on Linux
export RIFF_BEEP=0                          # afplay stub would no-op regardless
export RIFF_CLIPBOARD_MONITOR=0             # no pbpaste on Linux
export RIFF_MAX_SESSION_SEC=0               # disable auto-stop watchdog while testing
```

## 4. Run an end-to-end session

```bash
BIN=target/release/riff
mkdir -p /tmp/riff-shots && rm -rf "$RIFF_ROOT"

$BIN start --screenshot-dir /tmp/riff-shots
$BIN shot
sleep 1
$BIN stop --transcribe-cmd "printf 'hello from the daytona sandbox\n' > {out_txt}"
```

`--transcribe-cmd` replaces built-in Parakeet inference; `{out_txt}` is where riff expects the transcript file. Expect output ending with `stop_ms:` timings, then confirm results:

```bash
$BIN list                                   # table row with your transcript summary
$BIN show "$(cat "$RIFF_ROOT/last_session.json" | grep -o '"[0-9-]\{15\}"' | tr -d '"')"
cat "$RIFF_ROOT/sessions/"*/transcript.txt  # hello from the daytona sandbox
ls "$RIFF_ROOT/sessions/"*/screenshots/     # shot-001.png (+ derived/ variants)
tail -5 "$RIFF_ROOT/events.jsonl"           # global bus records from every command
```

`riff doctor` runs too; it reports `ok` for scripts/tools/storage and only fails the parakeet/web server lines when you disabled them via env above.

## 5. Test suite expectations

```bash
cargo test --release    # ~25s
```

Expect **55 passed, 1 failed** on Linux. The failure is `end_to_end_start_shot_stop_produces_transcript_and_note`: it asserts `[TestApp Screenshot 1]` markers in `note.md`, but `src/main.rs` (~line 1283) gates app-metadata capture behind `cfg!(target_os = "macos")`, so the stubbed `osascript` is never invoked on Linux and notes say `App metadata unavailable: osascript_unavailable`. That is a platform gate, not a regression — treat this single failure as expected in a Daytona sandbox.
