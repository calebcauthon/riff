# AGENTS.md

## Project overview

Riff is a local-first macOS dictation CLI. A session records microphone audio, associates screenshots and copied clipboard text with points on the audio timeline, transcribes the audio locally, and produces Markdown and HTML notes. Session content stays under `RIFF_ROOT` (default: `/tmp/riff`) unless the user explicitly copies, pastes, opens, or moves it elsewhere.

The main application is a Rust binary. It delegates audio capture and macOS integration to system tools and delegates speech recognition to a small Python/NeMo helper. It is not a hosted service and does not contain a built-in LLM, chat model, summarizer, embedding model, or image-generation model.

The normal user loop is:

```text
riff start  ->  speak / take screenshots / copy text  ->  riff stop
     |                                                    |
     +-- record and start watchers                        +-- transcribe and render notes
```

## Model stack

### Production speech-recognition model

The default and setup-pinned checkpoint is:

```text
nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc
revision: main
```

This is an English NVIDIA NeMo automatic speech-recognition checkpoint from the FastConformer hybrid streaming family. `scripts/parakeet_transcribe.py` loads it through `nemo.collections.asr.models.ASRModel.from_pretrained(...)` and calls the model's `transcribe(...)` method. The model is used only for speech-to-text.

Model resolution order is:

1. `riff stop --parakeet-model <id>` for that invocation
2. `RIFF_PARAKEET_MODEL`
3. the default checkpoint above

`riff setup` installs the pinned Python stack from `scripts/parakeet-requirements.txt` and pre-downloads the pinned checkpoint. The key pinned packages are NeMo Toolkit 2.4.0, PyTorch 2.7.1, and SoundFile 0.13.1. Setup requires Python 3.12; the transcription helper accepts Python 3.10-3.12. Device selection is CUDA when explicitly requested or available, otherwise CPU. On a normal Mac this means CPU inference; the code does not currently select Apple's MPS backend.

The preferred inference path is the persistent local server on the Riff-owned Unix socket at `$RIFF_ROOT/parakeet-server.sock`. Keeping the model loaded avoids paying model startup cost at every `riff stop`. An explicit `RIFF_PARAKEET_SERVER_URL` retains loopback TCP compatibility for benchmarks and custom setups. Health and transcription responses carry the server instance, owning root, model revision, actual device, PID, and runtime versions; Riff requires the requested identity to match before accepting a transcript. If the server is disabled, mismatched, unhealthy, or fails a request, Riff launches the same Python helper as a one-shot process. Both paths use the same configured model and remain local.

### Experimental checkpoints

`scripts/run_nemo_model_bakeoff.sh` can benchmark these NeMo IDs:

- `nvidia/parakeet-tdt_ctc-110m`
- `nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms_pc` (the production default)
- `nvidia/stt_en_fastconformer_hybrid_medium_streaming_80ms`
- `nvidia/stt_en_fastconformer_ctc_large`

Their presence in the benchmark script does not mean the application loads all of them. Runtime uses exactly one resolved checkpoint.

### Extensible/custom model path

`RIFF_TRANSCRIBE_CMD` or `--transcribe-cmd` replaces built-in Parakeet transcription with an arbitrary local command. `RIFF_POST_TRANSCRIBE_CMD`, `RIFF_HOOKS`, and repeated `--with-post-hook` values may then rewrite the transcript. These hooks could invoke another model, but Riff does not prescribe, bundle, or know about one.

## Architecture

```text
                         active_session.json
                                  |
riff CLI (Rust) -------- session state machine ----------------------+
  |                               |                                  |
  | start                         | capture                          | stop
  v                               v                                  v
ffmpeg/AVFoundation       screenshots + clipboard            finalize processes
16 kHz mono PCM WAV       events in events.jsonl                    |
                                                                     v
                    +---------------- transcription -----------------+
                    | custom command, or live chunks, or             |
                    | warm Parakeet HTTP server -> one-shot fallback |
                    +-----------------------+-------------------------+
                                            v
                               post-command and hook chain
                                            |
                                            v
                         annotation markers + report rendering
                              /                         \
                         note.md                     note.html
                                                        |
                                              local report server
                                              127.0.0.1:8766
```

### Rust control plane

- `src/main.rs` is the process entry point. It loads configuration, owns shared OS/process helpers, dispatches Clap commands, records performance events, and implements several utility commands.
- `src/cli.rs` defines the public command and flag surface.
- `src/session_commands.rs` implements the session lifecycle: start, direct screenshot capture, manual chunking, pause/unpause, and stop/finalization.
- `src/transcription.rs` resolves Python/model configuration, manages the Parakeet and report servers, calls the transcription endpoint, falls back to one-shot Python, and runs transcript commands/hooks.
- `src/reporting.rs` reconstructs screenshot/clipboard metadata from events, injects timeline markers, and renders Markdown, per-session HTML, and the sessions index.
- `src/history.rs` implements browsing and delivery commands such as `list`, `show`, `copy`, `send`, and `send-images`.
- `src/setup.rs` provisions the private Python runtime and implements installation diagnostics.
- `src/screenshots.rs` detects the macOS screenshot directory and adopts screenshots created during a session.
- `src/shot_modules/` renders the original, framed, enhanced, and polaroid screenshot variants. These are deterministic Rust image transforms, not AI models.
- `src/models.rs` contains Rust data structures such as `SessionState`, `ShotMeta`, and `ClipboardMeta`; despite the filename, these are domain structs, not ML models.
- `src/paths.rs` centralizes the on-disk layout below `RIFF_ROOT`.

### Python data plane

- `scripts/parakeet_transcribe.py` has three modes:
  - one-shot transcription of an audio file;
  - a persistent `/health` + `/transcribe` HTTP server with one loaded model;
  - an optional live watcher that detects silence, extracts chunks with ffmpeg, transcribes them, and appends chunk events.

In server mode, Rust passes a correlated startup instance ID, spawn timestamp, trigger, and `perf.jsonl` path. Python records one durable `parakeet_server_startup` event after binding or on import/model/bind failure, and exposes the successful startup timing object through `/health`. This records the full cold start without making `riff start` wait for readiness.
- `scripts/riff_web_server.py` serves reports and supports local report actions such as selecting a screenshot variant or saving an annotation. It is a file/report server, not an inference service, and shuts down after an idle timeout.

The Parakeet server uses a mode-`0600` Unix socket below `RIFF_ROOT` by default; the report server binds to loopback. PID and log files live below `RIFF_ROOT`, and `riff kill-server` stops both helpers and removes the Parakeet socket.

## Session lifecycle and data flow

### 1. Start

`riff start` refuses to overlap a live session, creates `sessions/<timestamp>/`, detects the macOS screenshot folder and audio input, and starts ffmpeg with AVFoundation. Audio is written as 16 kHz, mono, 16-bit PCM WAV.

It then writes `active_session.json`, starts a clipboard watcher by default, optionally starts live transcription when `RIFF_LIVE_TRANSCRIBE=1`, and begins warming the Parakeet server in the background. Start returns a structured warmup outcome immediately; a newly spawned Python server later appends its correlated ready/error event. The clipboard watcher is another invocation of the Riff binary that polls `pbpaste` and appends changed, non-empty text to `events.jsonl` with an audio-relative timestamp.

Start also spawns a max-duration watchdog (`riff watch-max-duration`, another invocation of the binary) unless `RIFF_MAX_SESSION_SEC=0`. It exits as soon as its recorder pid dies, and otherwise waits until the session has run for the cap, then appends a `max_duration_reached` event and spawns a normal detached `riff stop` — so a forgotten session still transcribes and runs its hooks. It only ever fires when the active state still names both its session id and its recorder pid, and `stop`/`fork`/stale-session cleanup SIGTERM it through `max_duration_watcher_pid`.

### 2. Capture during the session

`riff shot` runs interactive macOS `screencapture` directly into the session. It also attempts to record frontmost-application/window metadata through `osascript` and process statistics through `ps`.

Screenshots taken with normal macOS shortcuts are handled later: stop scans the configured screenshot folder for supported images whose modification times fall inside the session window, copies them into the session, records an event, and deletes the source copy.

`riff chunk` can transcribe audio since the last cursor while recording continues. Pause/unpause controls transcription capture and updates state/events. The optional live watcher divides growing audio at silence boundaries and incrementally maintains `transcript.txt`.

### 3. Stop and transcription selection

`riff stop` stops the clipboard watcher and ffmpeg recorder, asks a live watcher to flush and exit, adopts matching screenshots, and selects one transcription route:

1. A configured custom transcription command wins.
2. If live/manual chunks exist, Riff loads and flushes the accumulated chunk transcript.
3. Otherwise Riff uses the built-in Parakeet path: healthy warm server first, then one-shot Python fallback.

The result may pass through a single post-transcription command and then an ordered output-hook chain. Hook metadata includes session timing, audio, screenshots, clipboard captures, and transcription metadata. Hooks mutate a temporary transcript file; the canonical result is persisted as `transcript.txt`.

### 4. Reporting

Riff inserts `[Screenshot N]` and `[Clipboard N]` markers according to each item's audio timestamp, then writes `note.md` and `note.html`. HTML generation also creates deterministic screenshot variants under `screenshots/derived/` and rebuilds the sessions index. Finally it records `last_session.json`, clears `active_session.json`, logs phase timings, and starts the local report server if enabled.

## On-disk state

Default layout:

```text
/tmp/riff/
  active_session.json             # present only while recording
  last_session.json
  perf.jsonl                       # start/stop and Parakeet cold-start timing events
  parakeet-server.{pid,log}
  parakeet-server.sock
  web-server.{pid,log}
  sessions/
    <YYYYMMDD-HHMMSS>/
      audio.wav
      events.jsonl                # append-only session timeline
      ffmpeg.log
      transcription-watcher.log  # when live transcription is attempted
      transcript.txt
      transcript.original.txt     # only when hooks changed the text
      note.md
      note.html
      screenshots/
        shot-001.png
        derived/
```

`RIFF_ROOT` relocates this entire tree. Treat `active_session.json` as the coordination record and `events.jsonl` as the historical source of truth for captures and lifecycle events.

## Configuration

Riff reads `RIFF_*` values from three places. Precedence is process environment, then `~/.riff.json`, then `~/.riffrc`, then compiled defaults. Command flags override the resolved environment for the option they represent. `RIFF_RC_FILE` and `RIFF_CONFIG_JSON_FILE` can relocate the two config files.

Important switches:

- `RIFF_ROOT`: session and helper state root.
- `RIFF_PYTHON_BIN`, `RIFF_PARAKEET_SCRIPT`, `RIFF_PARAKEET_MODEL`, `RIFF_PARAKEET_MODEL_REVISION`: transcription runtime and exact checkpoint revision.
- `RIFF_PARAKEET_SERVER` / `RIFF_PARAKEET_SERVER_URL`: warm inference helper; enabled by default on `$RIFF_ROOT/parakeet-server.sock`, with an explicit URL selecting TCP compatibility mode.
- `RIFF_WEB_SERVER` / `RIFF_WEB_SERVER_URL`: report helper; enabled by default at port 8766.
- `RIFF_CLIPBOARD_MONITOR`: clipboard watcher; enabled by default.
- `RIFF_LIVE_TRANSCRIBE`: silence-aware incremental transcription; disabled by default.
- `RIFF_MAX_SESSION_SEC`: auto-stop watchdog cap in seconds; defaults to 90, clamped to 5-86400, `0` disables it.
- `RIFF_TRANSCRIBE_CMD`, `RIFF_POST_TRANSCRIBE_CMD`, `RIFF_HOOKS`: custom pipeline stages.

## Development and verification

The repository version is stored in `VERSION`; `build.rs` combines it with the Git commit and dirty state for the build ID. The `riff` shell wrapper builds `target/release/riff` on first use and selects a bundled runtime or local `.venv` when available.

Use these checks for normal Rust changes:

```bash
cargo fmt --check
cargo test
cargo build --release
```

`tests/cli_smoke.rs` uses temporary roots and fake macOS tools for command-level tests. Keep tests isolated by setting `RIFF_ROOT`, disabling background servers and beeps, and avoiding a user's real session tree. Real latency testing is different: follow `PRACTICAL_TESTING.md` and verify that the measured run actually used `transcription.method = parakeet_server` with healthy pre/post checks.

When changing architecture, update this file together with the relevant README sections. In particular, keep the pinned model ID consistent across `src/setup.rs`, `src/transcription.rs`, `scripts/parakeet_transcribe.py`, and documentation.
