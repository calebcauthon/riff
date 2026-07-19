# Start/Stop Performance Notes

This note captures what changed during the start/stop latency work, how the practical benchmark should be run, and what the current boundary is.

## Goal

Make `riff start` and `riff stop` feel as close to instant as possible under normal real usage.

The working benchmark became:

1. Use the real `riff` binary.
2. Use the normal `/tmp/riff` environment.
3. Use a dedicated warmed Parakeet server.
4. Measure both caller wall-clock time and `riff`'s own JSON timings.

## What We Changed

### Instrumentation

- Added fine-grained `start` and `stop` phase timing in:
  - `src/session_commands.rs`
- Added richer transcription perf metadata in:
  - `src/transcription.rs`
- Added short `startup_phase_ms` and `stop_phase_ms` summaries to command output.

### Startup optimizations

- Detached beep helper stdio so command timing no longer waits on sound playback.
- Trusted cached AVFoundation audio device immediately on the fast path.
- Reduced recorder startup confirmation wait from `300ms` to `120ms`.
- Tightened recorder start polling from `50ms` to `20ms`.
- Cached the resolved watcher Python binary so live-transcription start does not repeatedly probe Torch/NeMo dependencies.
- Added non-blocking Parakeet server warmup on `start`.

### Shutdown optimizations

- Removed an unconditional post-stop recorder sleep.
- Changed clipboard watcher shutdown to send `SIGTERM` and return immediately.
- Added a WAV-header fast path for duration lookup so common WAV sessions avoid `ffprobe`.

## Benchmark Notes

The earlier harnesses were misleading because they often changed the runtime path:

- fresh temporary `RIFF_ROOT`
- cold or unhealthy server state
- live watcher startup cost mixed into `start`
- synthetic recorder shims
- unrelated process already bound to the Parakeet server port

The practical benchmark must confirm:

- `transcription.method = parakeet_server`
- `transcription.perf.server_health_before = true`
- `transcription.perf.server_health_after = true`

If those are false, the run is measuring a different path.

## Current Results

Warm dedicated server path:

- `5s`: start `165.255ms`, stop `642.276ms`
- `15s`: start `140.282ms`, stop `835.662ms`
- `30s`: start `137.570ms`, stop `1068.788ms`

Important detail:

- `start` is now under the `250ms` target.
- warm `stop` is still dominated by Parakeet inference time on CPU.

## Current Boundary

With the current synchronous semantics, `stop` still means:

1. end recording
2. ensure transcription is complete
3. render/write final artifacts

That means sub-`250ms` `stop` is not realistic on the current CPU-backed Parakeet path.

To make `stop` truly instant from the user's perspective, the next step is a semantic change:

- return from `stop` once recording ends
- finish transcription and note generation in the background
- expose a way to check completion state afterward

Without that change, further improvements are likely to be incremental rather than transformational.
