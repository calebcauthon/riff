#!/usr/bin/env python3
"""
Local Parakeet transcription helper for riff.

One-shot mode:
  python scripts/parakeet_transcribe.py \
    --audio /tmp/riff/sessions/<id>/audio.wav \
    --out-txt /tmp/riff/sessions/<id>/transcript.txt \
    --model nvidia/parakeet-tdt-0.6b-v2

Server mode (keeps model loaded for faster subsequent transcriptions):
  python scripts/parakeet_transcribe.py --serve --port 8765
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


@dataclass
class AppError(Exception):
    message: str
    code: int = 1

    def __str__(self) -> str:
        return self.message


def out(msg: str, quiet: bool) -> None:
    if not quiet:
        print(msg)


def vout(msg: str, verbose: bool, quiet: bool) -> None:
    if verbose and not quiet:
        print(f"[verbose] {msg}")


def emit_json(payload: dict[str, Any], enabled: bool) -> None:
    if enabled:
        print(json.dumps(payload, indent=2))


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Transcribe audio with NVIDIA Parakeet (NeMo).")

    p.add_argument("--audio", help="Input audio file path (one-shot mode)")
    p.add_argument("--out-txt", help="Output transcript text file (one-shot mode)")

    p.add_argument(
        "--model",
        default=os.environ.get("RIFF_PARAKEET_MODEL", "nvidia/parakeet-tdt-0.6b-v2"),
        help="Parakeet model name (NeMo pretrained id)",
    )
    p.add_argument(
        "--device",
        default=os.environ.get("RIFF_PARAKEET_DEVICE", "auto"),
        choices=["auto", "cpu", "cuda"],
        help="Inference device",
    )
    p.add_argument("--batch-size", type=int, default=1, help="Transcription batch size")

    p.add_argument("--serve", action="store_true", help="Run persistent HTTP server")
    p.add_argument("--host", default="127.0.0.1", help="Server host (serve mode)")
    p.add_argument("--port", type=int, default=8765, help="Server port (serve mode)")

    p.add_argument("--verbose", action="store_true", help="Verbose logging")
    p.add_argument("--quiet", action="store_true", help="Suppress non-json output")
    p.add_argument("--json", action="store_true", help="Emit JSON summary")
    return p.parse_args()


def choose_device(requested: str) -> str:
    if requested in {"cpu", "cuda"}:
        return requested
    try:
        import torch  # type: ignore

        return "cuda" if torch.cuda.is_available() else "cpu"
    except Exception:
        return "cpu"


def ensure_supported_python() -> None:
    # NeMo/Parakeet stack is currently much more reliable on Python 3.10-3.12.
    if sys.version_info >= (3, 13):
        raise AppError(
            "Unsupported Python version for Parakeet/NeMo in this setup. "
            f"Detected {sys.version.split()[0]}. Use Python 3.10-3.12 (recommended: 3.12).",
            code=23,
        )


def load_model(model_name: str, device: str, verbose: bool, quiet: bool):
    ensure_supported_python()
    try:
        import torch  # type: ignore
        from nemo.collections.asr.models import ASRModel  # type: ignore
    except Exception as e:
        raise AppError(
            "Failed to import Parakeet dependencies. "
            "This is usually a Python/version dependency mismatch. "
            "Install with: pip install nemo_toolkit[asr] torch soundfile\n"
            f"Import error: {type(e).__name__}: {e}",
            code=20,
        ) from e

    target = choose_device(device)
    map_location = "cuda" if target == "cuda" else "cpu"

    t0 = time.time()
    vout(f"Loading model '{model_name}' on {map_location}", verbose, quiet)
    try:
        model = ASRModel.from_pretrained(model_name=model_name, map_location=map_location)
    except Exception as e:
        raise AppError(f"Failed to load Parakeet model '{model_name}': {e}", code=21) from e

    try:
        if target == "cuda":
            model = model.cuda()  # type: ignore[attr-defined]
        else:
            model = model.cpu()  # type: ignore[attr-defined]
    except Exception:
        pass

    vout(f"Model loaded in {time.time() - t0:.2f}s", verbose, quiet)
    return model, target


def normalize_transcript(raw: Any) -> str:
    if isinstance(raw, str):
        return raw.strip()

    if isinstance(raw, list) and raw:
        first = raw[0]
        if isinstance(first, str):
            return first.strip()
        text = getattr(first, "text", None)
        if isinstance(text, str):
            return text.strip()
        return str(first).strip()

    return str(raw).strip()


def transcribe_path(model: Any, audio: Path, batch_size: int, verbose: bool, quiet: bool) -> str:
    if not audio.exists() or not audio.is_file():
        raise AppError(f"Audio file not found: {audio}", code=2)

    t0 = time.time()
    vout(f"Transcribing: {audio}", verbose, quiet)
    try:
        result = model.transcribe([str(audio)], batch_size=batch_size)
    except TypeError:
        result = model.transcribe([str(audio)])
    except Exception as e:
        raise AppError(f"Transcription failed: {e}", code=22) from e

    text = normalize_transcript(result)
    vout(f"Transcription finished in {time.time() - t0:.2f}s", verbose, quiet)
    return text


def run_one_shot(args: argparse.Namespace) -> int:
    if not args.audio or not args.out_txt:
        raise AppError("--audio and --out-txt are required in one-shot mode", code=2)

    audio = Path(args.audio).expanduser().resolve()
    out_txt = Path(args.out_txt).expanduser().resolve()
    out_txt.parent.mkdir(parents=True, exist_ok=True)

    model, actual_device = load_model(args.model, args.device, args.verbose, args.quiet)
    text = transcribe_path(model, audio, args.batch_size, args.verbose, args.quiet)
    out_txt.write_text(text + "\n", encoding="utf-8")

    payload = {
        "ok": True,
        "audio": str(audio),
        "out_txt": str(out_txt),
        "model": args.model,
        "device": actual_device,
        "chars": len(text),
    }

    out(f"wrote transcript: {out_txt}", args.quiet)
    emit_json(payload, args.json)
    if not args.json and args.verbose and not args.quiet:
        print(text)
    return 0


def run_server(args: argparse.Namespace) -> int:
    model, actual_device = load_model(args.model, args.device, args.verbose, args.quiet)
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def _send(self, code: int, payload: dict[str, Any]) -> None:
            body = json.dumps(payload).encode("utf-8")
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, fmt: str, *args_: Any) -> None:  # silence default access logs
            if args.verbose and not args.quiet:
                super().log_message(fmt, *args_)

        def do_GET(self) -> None:  # noqa: N802
            if self.path.rstrip("/") == "/health":
                self._send(
                    200,
                    {
                        "ok": True,
                        "service": "parakeet",
                        "model": args.model,
                        "device": actual_device,
                    },
                )
                return
            self._send(404, {"ok": False, "error": "not found"})

        def do_POST(self) -> None:  # noqa: N802
            if self.path.rstrip("/") != "/transcribe":
                self._send(404, {"ok": False, "error": "not found"})
                return

            try:
                raw_len = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(raw_len) if raw_len > 0 else b"{}"
                payload = json.loads(body.decode("utf-8"))

                audio = Path(str(payload.get("audio", ""))).expanduser().resolve()
                out_txt_raw = payload.get("out_txt")
                out_txt = (
                    Path(str(out_txt_raw)).expanduser().resolve() if isinstance(out_txt_raw, str) and out_txt_raw else None
                )
                batch_size = int(payload.get("batch_size", args.batch_size))

                if not str(audio):
                    raise AppError("audio is required", code=2)

                t0 = time.time()
                with lock:
                    text = transcribe_path(model, audio, batch_size, args.verbose, args.quiet)

                if out_txt is not None:
                    out_txt.parent.mkdir(parents=True, exist_ok=True)
                    out_txt.write_text(text + "\n", encoding="utf-8")

                self._send(
                    200,
                    {
                        "ok": True,
                        "text": text,
                        "chars": len(text),
                        "audio": str(audio),
                        "out_txt": str(out_txt) if out_txt else None,
                        "elapsed_sec": round(time.time() - t0, 3),
                    },
                )
            except AppError as e:
                self._send(400, {"ok": False, "error": str(e), "code": e.code})
            except Exception as e:  # noqa: BLE001
                self._send(500, {"ok": False, "error": f"internal error: {e}"})

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    out(
        f"Parakeet server ready on http://{args.host}:{args.port} (model={args.model}, device={actual_device})",
        args.quiet,
    )
    try:
        server.serve_forever(poll_interval=0.2)
    except KeyboardInterrupt:
        out("Shutting down Parakeet server", args.quiet)
    finally:
        server.server_close()
    return 0


def main() -> int:
    args = parse_args()
    try:
        if args.serve:
            return run_server(args)
        return run_one_shot(args)
    except AppError as e:
        payload = {"ok": False, "error": str(e), "code": e.code}
        if args.json:
            print(json.dumps(payload, indent=2), file=sys.stderr)
        else:
            print(f"Error: {e}", file=sys.stderr)
        return e.code


if __name__ == "__main__":
    raise SystemExit(main())
