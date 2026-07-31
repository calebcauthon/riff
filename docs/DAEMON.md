# The riff daemon (riffd)

`riffd` is a long-running control socket for events in and out of riff. It is
the systematic integration point for external tools: anything that can speak
HTTP over a Unix socket can publish events onto riff's bus and subscribe to
everything riff does.

The JSONL bus file (`$RIFF_ROOT/events.jsonl`, see `docs/EVENTS.md`) remains
the durable log and single source of truth. The daemon feeds subscribers by
tailing that file, so events appended by any process — one-shot CLI commands,
the Python helpers, the daemon itself — all reach subscribers. Riff processes
never need the daemon to publish; `POST /events` exists for tools that are not
riff and should not hand-format the envelope.

## Lifecycle

```bash
riff daemon start     # spawn detached, wait until ready
riff daemon status    # identity + uptime + subscriber count; exit 1 when down
riff daemon stop      # SIGTERM, removes socket + pid file
riff daemon run       # foreground (debugging); Ctrl-C to stop
riff kill-server      # kills riffd along with the web + parakeet helpers
```

Files, all under `$RIFF_ROOT`: `riffd.sock` (mode 0600), `riffd.pid`,
`riffd.log`. The daemon emits `daemon_started` / `daemon_stopped` bus events,
and `daemon start` refuses nothing: starting twice reports `already_running`.

At startup the daemon warms the Parakeet transcription server in the
background (`daemon_preload` event) so the first `riff stop` never pays the
model-load cost. `RIFF_DAEMON_PRELOAD=0` disables the warm-up;
`RIFF_PARAKEET_SERVER=0` disables the server entirely, preload included.

## HTTP API

Minimal HTTP/1.1 over the Unix socket, mirroring the Parakeet server's
conventions. Debug with `curl --unix-socket`.

### `GET /identity`

Ownership handshake: `service` (`"riffd"`), `protocol_version`, `pid`,
`riff_root` (canonicalized), `version`, `build_id`, `started_at`, `socket`.
Clients must verify `riff_root` matches their own before trusting the socket.

### `GET /health`

Identity plus `ok`, `uptime_sec`, and `subscribers` (current stream count).

### `POST /events`

Append an external event. Body is a JSON object:

| Field | Required | Meaning |
| --- | --- | --- |
| `type` | yes | Event type: 1-64 chars, letters/digits/`_`/`-`/`.`, starts with a letter. |
| `source` | no | Sender name, same charset; envelope `command` becomes `external:<source>` (default `external:unknown`). |
| `payload` | no | JSON object merged into the record. Strings are clipped at 200 chars; a `ts` here overrides the envelope timestamp. |
| `session_id` | no | Attach the event to a session. |
| `level` | no | `info` (default), `warn`, or `error`. |

The daemon assigns the rest of the envelope (`v`, `seq`, `inv`, `pid`) —
external senders cannot forge riff-native records because `command` always
names them. Returns `200 {"ok":true,...}` or `400` with a reason.

```bash
curl -s --unix-socket /tmp/riff/riffd.sock http://riffd/events \
  -d '{"type":"build_finished","source":"ci","payload":{"job":"tests","ok":true}}'
```

### `GET /subscribe`

Long-lived NDJSON stream of bus events, pushed as they land. Query filters
compose (AND), comma-separate multiple values: `type`, `command`, `session`,
`grep`.

```bash
curl -sN --unix-socket /tmp/riff/riffd.sock \
  "http://riffd/subscribe?type=session_started,session_stopped"
```

The stream starts at "now" — use `riff watch --since/--tail/--all` (or read
`events.jsonl` directly) for history. `riff watch` itself prefers this stream
when the daemon is up and falls back to file tailing when it is not.

## Ownership

Like every riff helper, the daemon belongs to one `RIFF_ROOT`: the socket
lives under it, `/identity` names it, and `daemon run --root` puts it on the
command line so a wedged daemon is still identifiable from the process table.
