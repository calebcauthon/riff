# Riff events

Every `riff` invocation appends records to a global, append-only event bus at
`$RIFF_ROOT/events.jsonl` (default `/tmp/riff/events.jsonl`). `riff watch`
tails it. This is the contract that stream follows.

Per-session `sessions/<id>/events.jsonl` files are unchanged and remain the
source of truth for report rendering. Session events are written there in their
historical shape and *mirrored* onto the bus with envelope fields added.

## Envelope

Records are flat: envelope keys sit alongside the domain payload.

```json
{"v":1,"ts":"2026-07-28T18:03:11.482Z","seq":3,"inv":"1785239830146-75457",
 "pid":75457,"command":"shot","type":"screenshot_taken","session_id":"20260728-180302",
 "level":"info","shot_id":1,"dest_rel_path":"screenshots/shot-001.png","audio_sec":2.41}
```

| Field | Meaning |
| --- | --- |
| `v` | Envelope schema version. Currently `1`. |
| `ts` | RFC 3339 UTC timestamp, millisecond precision. |
| `seq` | Monotonic counter within one invocation. |
| `inv` | Invocation id, `<spawn-epoch-ms>-<pid>`. Correlates every record from one process. |
| `pid` | Emitting process id. |
| `command` | Subcommand name (`start`, `stop`, `send-images`, …), or `parakeet-server` / `transcription-watcher` for the Python helpers. |
| `type` | Event type; see the catalog below. |
| `session_id` | Present when the event belongs to a session. |
| `level` | `info`, `warn`, or `error`. |
| `truncated` | Present and `true` when a string in the payload was clipped. |

These nine envelope keys are reserved — domain payloads must not use them, and
the envelope overwrites the payload on collision.

Two payload caveats:

- **Strings are clipped at 200 characters.** This keeps a record inside a
  single append write, so concurrent riff processes do not interleave mid-line,
  and it keeps full clipboard and transcript text out of a shared file.
- **User-supplied command templates are not logged.** `--transcribe-cmd`,
  `--with-post-hook`, and hook bodies appear in `command_started` only as
  counts and booleans.

## Catalog

### Command lifecycle

Emitted for every subcommand, including ones with no domain events.

| Type | Payload |
| --- | --- |
| `command_started` | `args` — redacted argument summary |
| `command_finished` | `exit_code`, `duration_ms`. `level` is `warn` on a non-zero exit. |
| `command_failed` | `exit_code`, `duration_ms`, `error`. Always `level: error`. |

### Session lifecycle

Also written to the session's own `events.jsonl`.

| Type | Emitted by |
| --- | --- |
| `session_started` | `start` |
| `session_stopping`, `session_stopped` | `stop` |
| `session_paused`, `session_unpaused` | `pause`, `unpause`, `toggle-pause` |
| `screenshot_taken` | `shot` |
| `screenshot_moved` | `stop` adopting shots from the macOS screenshot folder |
| `clipboard_copied` | `watch-clipboard` |
| `transcript_chunk` | `chunk` and the live transcription watcher |
| `transcript_probe`, `transcription_worker_stopped` | live transcription watcher |
| `transcription_watcher_started` / `_not_started` / `_exited_early` | `start` |
| `max_duration_reached` | `watch-max-duration` |

### Pipeline and command events

Bus only.

| Type | Emitted by | Payload |
| --- | --- | --- |
| `toggle_resolved` | `toggle` | `resolved_to`: `start` or `stop` |
| `session_forked` | `fork` | `old_session_id`, `new_session_id`, `split_gap_ms`, `split_to_running_ms` |
| `transcription_finished` | `stop` | `status`, `method`, `chars`, `words`, `duration_ms` |
| `output_hooks_ran` | `stop` | `status`, `count`, `chars_before`, `chars_after`, `duration_ms` |
| `session_delivered` | `copy`, `send`, `send-images` | `mode`, `rank`, `chars`, `chunks`, `images` |
| `report_opened` | `html` | `target`, `web_server_ready` |
| `screenshot_variant_selected` | `screenshot-use` | `shot_id`, `module`, `target_path` |
| `setup_step`, `setup_finished` | `setup` | `step`, `status`, `runtime_dir`, `model`, … |
| `doctor_ran` | `doctor` | `ok`, `deep`, `checks`, `failed` |
| `config_changed` | `silence`, `loud` | `key`, `value`, `rc_file` |
| `server_killed` | `kill-server` | `server`, `status`, `pid`, `signal` |
| `parakeet_server_startup` | Parakeet server | `status`, `instance_id`, `model`, `device`, `total_ms`, `phases` |

There is no separate `setup_started` or `session_copied` — `command_started`
with `command: setup` and `session_delivered` cover those.

## Configuration

| Variable | Default | Effect |
| --- | --- | --- |
| `RIFF_EVENT_BUS` | `1` | Set to `0` to disable all bus writes. Session `events.jsonl` files are unaffected. |
| `RIFF_EVENT_BUS_MAX_BYTES` | `8388608` | Size cap. On the next write past the cap, the bus is renamed to `events.jsonl.1` and a fresh file is started. |

Bus writes never fail a command: a write error is dropped silently.

## Watching

```bash
riff watch                                  # follow from now
riff watch --since 10m                      # backfill a window, then follow
riff watch -n 50                            # last 50, then follow
riff watch --all                            # everything retained, then follow
riff watch --once                           # print the backlog and exit
riff watch --json                           # NDJSON passthrough for piping
riff watch --type screenshot_taken --type session_stopped
riff watch --command stop
riff watch --session current                # or an explicit session id
riff watch --grep parakeet
```

`--once`, `--json`, `--type`, `--command`, and `--session` compose, so
`riff watch --json --once --all | jq` is the scripting entry point.

`watch` is a viewer. It never writes to the bus and never runs anything in
response to an event.
