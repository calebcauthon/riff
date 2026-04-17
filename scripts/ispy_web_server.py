#!/usr/bin/env python3
"""
Tiny local web server for ispy session pages.

Serves /tmp/ispy (or configured root) over localhost and exits after idle timeout.

Endpoints:
- GET  /health        -> JSON health payload
- POST /touch         -> reset idle timer
- POST /use-screenshot -> promote derived module image into transcript screenshot path
- POST /save-image    -> write annotated PNG into served sessions tree
- GET  /sessions/...  -> static files (e.g., note.html)
"""

from __future__ import annotations

import argparse
import base64
import binascii
import json
import os
import subprocess
import threading
import time
from functools import partial
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse


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
        def _write_cors_headers(self) -> None:
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "Content-Type")

        def _json(self, status: int, payload: dict) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _read_json_body(self) -> dict | None:
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                return None
            if length <= 0 or length > 25_000_000:
                return None
            raw = self.rfile.read(length)
            try:
                parsed = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                return None
            if not isinstance(parsed, dict):
                return None
            return parsed

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

        def end_headers(self) -> None:  # noqa: N802
            self._write_cors_headers()
            super().end_headers()

        def do_OPTIONS(self):  # noqa: N802
            self.send_response(HTTPStatus.NO_CONTENT)
            self.end_headers()

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

            if self.path.rstrip("/") == "/use-screenshot":
                try:
                    length = int(self.headers.get("Content-Length", "0") or "0")
                except ValueError:
                    length = 0
                raw = self.rfile.read(length) if length > 0 else b"{}"
                try:
                    payload = json.loads(raw.decode("utf-8") or "{}")
                except json.JSONDecodeError:
                    self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "invalid json"})
                    return

                session_id = str(payload.get("session_id") or "").strip()
                module = str(payload.get("module") or "").strip()
                shot_id = payload.get("shot_id")
                if not session_id or not module or not isinstance(shot_id, int):
                    self._json(
                        HTTPStatus.BAD_REQUEST,
                        {"ok": False, "error": "session_id, shot_id(int), and module are required"},
                    )
                    return

                riff_bin = (
                    os.environ.get("ISPY_RIFF_BIN")
                    or os.environ.get("ISPY_DICTATE_BIN")
                    or "riff"
                )
                cmd = [
                    riff_bin,
                    "--json",
                    "--quiet",
                    "screenshot-use",
                    "--session-id",
                    session_id,
                    "--shot-id",
                    str(shot_id),
                    "--module",
                    module,
                ]
                try:
                    run = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
                except Exception as e:  # noqa: BLE001
                    self._json(
                        HTTPStatus.BAD_REQUEST,
                        {
                            "ok": False,
                            "error": "riff screenshot-use execution failed",
                            "detail": str(e),
                            "cmd": cmd,
                        },
                    )
                    return
                if run.returncode != 0:
                    self._json(
                        HTTPStatus.BAD_REQUEST,
                        {
                            "ok": False,
                            "error": "riff screenshot-use failed",
                            "code": run.returncode,
                            "stderr": run.stderr.strip(),
                            "stdout": run.stdout.strip(),
                        },
                    )
                    return

                touch()
                self._json(
                    HTTPStatus.OK,
                    {
                        "ok": True,
                        "session_id": session_id,
                        "shot_id": shot_id,
                        "module": module,
                    },
                )
                return

            if self.path.rstrip("/") == "/save-image":
                payload = self._read_json_body()
                if payload is None:
                    self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "invalid json body"})
                    return

                raw_url = str(payload.get("url", ""))
                raw_abs_path = str(payload.get("absPath", ""))
                data_url = str(payload.get("dataUrl", ""))
                if not data_url:
                    self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "dataUrl required"})
                    return

                target: Path | None = None
                if raw_abs_path:
                    try:
                        abs_path = Path(raw_abs_path).expanduser().resolve()
                        abs_path.relative_to(root)
                        target = abs_path
                    except Exception:
                        self._json(HTTPStatus.FORBIDDEN, {"ok": False, "error": "absPath escapes server root"})
                        return

                if target is None:
                    if not raw_url:
                        self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "url or absPath required"})
                        return
                    parsed = urlparse(raw_url)
                    rel_path = parsed.path.lstrip("/")
                    if not rel_path:
                        self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "invalid url path"})
                        return
                    target = (root / rel_path).resolve()

                prefix = "data:image/png;base64,"
                if not data_url.startswith(prefix):
                    self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "dataUrl must be image/png base64"})
                    return

                encoded = data_url[len(prefix) :]
                try:
                    png_bytes = base64.b64decode(encoded, validate=True)
                except (binascii.Error, ValueError):
                    self._json(HTTPStatus.BAD_REQUEST, {"ok": False, "error": "invalid base64 payload"})
                    return

                try:
                    target.relative_to(root)
                except ValueError:
                    self._json(HTTPStatus.FORBIDDEN, {"ok": False, "error": "path escapes server root"})
                    return

                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(png_bytes)
                touch()
                self._json(
                    HTTPStatus.OK,
                    {
                        "ok": True,
                        "saved": True,
                        "path": str(target),
                        "bytes": len(png_bytes),
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
