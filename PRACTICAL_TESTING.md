# Practical Start/Stop Testing

This note documents how to measure `riff start` / `riff stop` in a way that matches real usage, and why the earlier benchmark harness produced misleading results.

## What We Actually Want To Measure

The goal is simple:

1. Run the real `riff start`
2. Wait a few seconds
3. Run the real `riff stop`
4. Measure both:
   - wall-clock time
   - the timings reported by `riff` itself

For the result to be meaningful, the test must use the same runtime conditions as normal manual usage.

## Correct Practical Test

Use the real `riff` command in the normal environment, not an isolated synthetic root.

Example procedure:

1. Ensure the Parakeet server is already running and healthy.
2. Run `riff start`.
3. Wait about 5 seconds.
4. Run `riff stop`.
5. Read:
   - `startup_ms`
   - `stop_ms`
   - `transcription.method`
   - `transcription.perf`

The most important field is:

- `transcription.method`

If the run is representative of normal fast usage, it should be:

- `parakeet_server`

And these should also be true:

- `transcription.perf.server_health_before = true`
- `transcription.perf.server_health_after = true`

## Real Measured Example

A real outside-sandbox run produced:

- `riff start`: about `439ms`
- `riff stop`: about `1464ms`

That stop used:

- `transcription.method = parakeet_server`
- `server_health_before = true`
- `server_health_after = true`
- `server_ensure_ms = 7.07`
- `server_request_ms = 1019.431`

That is the real fast path.

## Why The Earlier Harness Failed

The earlier benchmark script was measuring valid code paths, but not the same path as real manual usage.

### 1. It used a fresh temporary `RIFF_ROOT`

That meant each run started from a clean environment instead of the normal warmed-up local state.

Effect:

- different server state
- different cache state
- different helper process state

### 2. “Warm server” was not actually warm

In the harness runs, the Parakeet server path often timed out or failed health checks before stop-time transcription.

Symptoms:

- `server_health_before = false`
- `server_health_after = false`
- very large `server_ensure_ms`
- fallback behavior instead of fast server inference

So the script labeled some runs as warm when they were not.

### 3. “Live transcription” changed startup behavior

When live transcription was enabled, `start` included transcription watcher/model startup time.

Effect:

- `start` became much slower than normal
- the test stopped resembling ordinary `riff start`

### 4. The script replayed audio through a recorder shim

That part was useful for repeatability, but it was still not identical to the normal interactive flow.

It is acceptable only if the rest of the runtime conditions match the real path.

### 5. Sandbox behavior differed from the normal terminal

Inside the agent environment, some runs did not behave like the normal shell session:

- AVFoundation device enumeration differed
- Parakeet server bind/health behavior differed

That is why direct outside-sandbox testing matched reality much better.

## Practical Rule

Do not trust a “practical” benchmark unless all of these are true:

- it runs the real `riff` command
- it uses the normal `RIFF_ROOT`
- it uses the same audio/input path as real usage
- `transcription.method = parakeet_server`
- `server_health_before = true`
- `server_health_after = true`

If those are not true, the test may still be useful, but it is measuring a different execution path.

## Recommended Benchmark Strategy

Use two categories:

### Real-user measurement

Measure the exact path used in normal daily usage:

- real `riff start`
- real `riff stop`
- real warmed Parakeet server

This is the number to optimize for user experience.

### Diagnostic fallback measurement

Separately measure slower paths for debugging:

- cold stop
- server-timeout fallback
- live-transcribe startup cost

These are useful for diagnosis, but should not be confused with the normal user-facing latency number.
