#!/usr/bin/env python3
"""
Tiny local web server for ispy session pages.

Serves /tmp/ispy (or configured root) over localhost and exits after idle timeout.

Endpoints:
- GET  /health        -> JSON health payload
- POST /touch         -> reset idle timer
- GET  /sessions/...  -> static files (e.g., note.html)
"""

from __future__ import annotations

import argparse
import json
import os
import threading
import time
from functools import partial
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="ispy local web server")
    p.add_argument("--root", default=os.environ.get("ISPY_ROOT", "/tmp/ispy"), help="Directory to serve")
    p.add_argument("--host", default="127.0.0.1", help="Bind host")
    p.add_argument("--port", type=int, default=8766, help="Bind port")
    p.add_argument("--idle-timeout-sec", type=int, default=1800, help="Exit after this many idle seconds")
    return p.parse_args()


def main() -> int:
    args = parse_args()
    root = Path(args.root).expanduser().resolve()
    root.mkdir(parents=True, exist_ok=True)

    lock = threading.Lock()
    last_activity = {"ts": time.time()}

    def touch() -> None:
        with lock:
            last_activity["ts"] = time.time()

    def idle_seconds() -> float:
        with lock:
            return max(0.0, time.time() - last_activity["ts"])

    class Handler(SimpleHTTPRequestHandler):
        def _json(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, fmt: str, *args):  # noqa: ANN001
            # Keep logs minimal in background mode.
            return

        def do_GET(self):  # noqa: N802
            if self.path.rstrip("/") == "/health":
                self._json(
                    HTTPStatus.OK,
                    {
                        "ok": True,
                        "service": "ispy-web",
                        "root": str(root),
                        "idle_timeout_sec": args.idle_timeout_sec,
                        "idle_sec": round(idle_seconds(), 3),
                    },
                )
                return

            touch()
            super().do_GET()

        def do_POST(self):  # noqa: N802
            if self.path.rstrip("/") == "/touch":
                touch()
                self._json(
                    HTTPStatus.OK,
                    {
                        "ok": True,
                        "touched": True,
                        "idle_sec": round(idle_seconds(), 3),
                    },
                )
                return

            self._json(HTTPStatus.NOT_FOUND, {"ok": False, "error": "not found"})

    server = ThreadingHTTPServer((args.host, args.port), partial(Handler, directory=str(root)))
    print(
        json.dumps(
            {
                "ok": True,
                "service": "ispy-web",
                "url": f"http://{args.host}:{args.port}",
                "root": str(root),
                "idle_timeout_sec": args.idle_timeout_sec,
            }
        ),
        flush=True,
    )

    stop_event = threading.Event()

    def idle_watcher() -> None:
        while not stop_event.is_set():
            time.sleep(1.0)
            if idle_seconds() > args.idle_timeout_sec:
                print(
                    json.dumps(
                        {
                            "ok": True,
                            "service": "ispy-web",
                            "event": "idle-timeout-shutdown",
                            "idle_sec": round(idle_seconds(), 3),
                        }
                    ),
                    flush=True,
                )
                server.shutdown()
                return

    watcher = threading.Thread(target=idle_watcher, daemon=True)
    watcher.start()

    try:
        server.serve_forever(poll_interval=0.2)
    except KeyboardInterrupt:
        pass
    finally:
        stop_event.set()
        server.server_close()

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
