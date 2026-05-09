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
import re
import shutil
import subprocess
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


def iso_now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime())


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
    p.add_argument(
        "--batch-size",
        type=int,
        default=int(os.environ.get("RIFF_PARAKEET_BATCH_SIZE", "4")),
        help="Transcription batch size",
    )

    p.add_argument("--serve", action="store_true", help="Run persistent HTTP server")
    p.add_argument("--watch-audio", action="store_true", help="Run incremental silence-aware chunking")
    p.add_argument("--host", default="127.0.0.1", help="Server host (serve mode)")
    p.add_argument("--port", type=int, default=8765, help="Server port (serve mode)")
    p.add_argument("--events-path", help="events.jsonl path (watch mode)")
    p.add_argument("--session-id", help="session id (watch mode)")
    p.add_argument("--started-at-epoch", type=float, help="session start unix epoch sec (watch mode)")
    p.add_argument(
        "--min-chunk-sec",
        type=float,
        default=float(os.environ.get("RIFF_CHUNK_MIN_SEC", "12")),
        help="minimum chunk duration before considering silence",
    )
    p.add_argument(
        "--max-chunk-sec",
        type=float,
        default=float(os.environ.get("RIFF_CHUNK_MAX_SEC", "0")),
        help="forced cut if no suitable silence found; <=0 disables max cut",
    )
    p.add_argument(
        "--silence-sec",
        type=float,
        default=float(os.environ.get("RIFF_CHUNK_SILENCE_SEC", "1.2")),
        help="minimum silence length for a boundary",
    )
    p.add_argument(
        "--silence-db",
        type=float,
        default=float(os.environ.get("RIFF_CHUNK_SILENCE_DB", "-33")),
        help="silence threshold in dB for ffmpeg silencedetect",
    )
    p.add_argument(
        "--poll-ms",
        type=int,
        default=int(os.environ.get("RIFF_CHUNK_POLL_MS", "800")),
        help="poll interval in milliseconds for watch mode",
    )

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


def ffprobe_duration_sec(audio: Path) -> float:
    out = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            str(audio),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if out.returncode != 0:
        return 0.0
    try:
        return max(0.0, float(out.stdout.strip()))
    except ValueError:
        return 0.0


def append_event(events_path: Path, payload: dict[str, Any]) -> None:
    events_path.parent.mkdir(parents=True, exist_ok=True)
    with events_path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(payload, ensure_ascii=False))
        f.write("\n")


def stopping_requested(events_path: Path, session_id: str) -> bool:
    if not events_path.exists():
        return False
    try:
        lines = events_path.read_text(encoding="utf-8").splitlines()
    except Exception:
        return False
    for line in reversed(lines):
        if not line.strip():
            continue
        try:
            payload = json.loads(line)
        except Exception:
            continue
        if payload.get("type") != "session_stopping":
            continue
        if payload.get("session_id") == session_id:
            return True
    return False


SILENCE_START_RE = re.compile(r"silence_start:\s*([0-9]+(?:\.[0-9]+)?)")
SILENCE_END_RE = re.compile(r"silence_end:\s*([0-9]+(?:\.[0-9]+)?)")
MEAN_VOLUME_RE = re.compile(r"mean_volume:\s*(-?inf|[-+]?[0-9]+(?:\.[0-9]+)?)\s*dB")
MAX_VOLUME_RE = re.compile(r"max_volume:\s*(-?inf|[-+]?[0-9]+(?:\.[0-9]+)?)\s*dB")


def measure_tail_db(audio: Path, start_sec: float, window_sec: float) -> float | None:
    if window_sec <= 0:
        return None
    # Keep this short so the live meter reflects near-current speech activity.
    tail_sec = min(0.12, window_sec)
    tail_start = max(start_sec, start_sec + window_sec - tail_sec)
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "info",
        "-ss",
        f"{tail_start:.3f}",
        "-t",
        f"{tail_sec:.3f}",
        "-i",
        str(audio),
        "-af",
        "volumedetect",
        "-f",
        "null",
        "-",
    ]
    run = subprocess.run(cmd, capture_output=True, text=True, check=False)
    logs = f"{run.stdout}\n{run.stderr}"
    # Prefer short-window peak level; this tracks silencedetect behavior better than mean.
    m = MAX_VOLUME_RE.search(logs) or MEAN_VOLUME_RE.search(logs)
    if not m:
        return None
    raw = m.group(1)
    if raw == "-inf":
        return -120.0
    try:
        return float(raw)
    except ValueError:
        return None


def detect_cut_time(
    audio: Path,
    start_sec: float,
    available_sec: float,
    min_chunk_sec: float,
    max_chunk_sec: float | None,
    silence_sec: float,
    silence_db: float,
) -> tuple[float | None, str, float, float | None]:
    if available_sec <= 0:
        return None, "wait_min", 0.0, None

    window_sec = min(available_sec, max_chunk_sec) if max_chunk_sec is not None else available_sec
    current_db = measure_tail_db(audio, start_sec, window_sec)
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "info",
        "-ss",
        f"{start_sec:.3f}",
        "-t",
        f"{window_sec:.3f}",
        "-i",
        str(audio),
        "-af",
        f"silencedetect=noise={silence_db:.3f}dB:d={silence_sec:.3f}",
        "-f",
        "null",
        "-",
    ]
    run = subprocess.run(cmd, capture_output=True, text=True, check=False)
    logs = f"{run.stdout}\n{run.stderr}"
    starts = [float(m.group(1)) for m in SILENCE_START_RE.finditer(logs)]
    ends = [float(m.group(1)) for m in SILENCE_END_RE.finditer(logs)]

    trailing_silence = 0.0
    for silence_start in starts:
        silence_end = next((e for e in ends if e >= silence_start), None)
        if silence_end is None:
            trailing_silence = max(trailing_silence, max(0.0, window_sec - silence_start))
        elif silence_end >= window_sec - 0.03:
            trailing_silence = max(trailing_silence, max(0.0, window_sec - silence_start))

    for silence_start in starts:
        silence_end = next((e for e in ends if e >= silence_start), None)
        if silence_end is None:
            continue
        midpoint = (silence_start + silence_end) / 2.0
        if midpoint >= min_chunk_sec:
            return start_sec + midpoint, "silence", trailing_silence, current_db

    if max_chunk_sec is not None and available_sec >= max_chunk_sec:
        return start_sec + window_sec, "max", trailing_silence, current_db
    if available_sec < min_chunk_sec:
        return None, "wait_min", trailing_silence, current_db
    return None, "wait_silence", trailing_silence, current_db


def extract_segment_to_file(
    source_audio: Path,
    start_sec: float,
    end_sec: float,
    target_audio: Path,
) -> bool:
    if end_sec <= start_sec:
        return False
    target_audio.parent.mkdir(parents=True, exist_ok=True)
    duration = end_sec - start_sec
    run = subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
            f"{start_sec:.3f}",
            "-t",
            f"{duration:.3f}",
            "-i",
            str(source_audio),
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            str(target_audio),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    return run.returncode == 0 and target_audio.exists()


def join_chunk_text(existing_text: str, chunk_text: str) -> str:
    left = existing_text.strip()
    right = chunk_text.strip()
    if not right:
        return left
    if not left:
        return right
    return f"{left} {right}"


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


def run_watch_audio(args: argparse.Namespace) -> int:
    if not args.audio or not args.out_txt or not args.events_path or not args.session_id:
        raise AppError(
            "--watch-audio requires --audio, --out-txt, --events-path, and --session-id",
            code=2,
        )

    if not shutil.which("ffmpeg"):
        raise AppError("ffmpeg is required for --watch-audio", code=5)
    if not shutil.which("ffprobe"):
        raise AppError("ffprobe is required for --watch-audio", code=5)

    source_audio = Path(args.audio).expanduser().resolve()
    out_txt = Path(args.out_txt).expanduser().resolve()
    events_path = Path(args.events_path).expanduser().resolve()
    scratch_audio = source_audio.parent / ".chunk-transcribe.wav"

    min_chunk_sec = max(0.5, float(args.min_chunk_sec))
    max_chunk_sec = (
        max(min_chunk_sec, float(args.max_chunk_sec))
        if float(args.max_chunk_sec) > 0
        else None
    )
    silence_sec = max(0.1, float(args.silence_sec))
    silence_db = float(args.silence_db)
    poll_sec = max(0.2, float(args.poll_ms) / 1000.0)

    model, actual_device = load_model(args.model, args.device, args.verbose, args.quiet)

    chunk_id = 0
    next_start_sec = 0.0
    transcript_text = out_txt.read_text(encoding="utf-8").strip() if out_txt.exists() else ""
    last_probe_key: tuple[str, int] | None = None

    while True:
        duration = ffprobe_duration_sec(source_audio)
        available = max(0.0, duration - next_start_sec)
        cut_time, reason, trailing_silence_sec, current_db = detect_cut_time(
            source_audio,
            next_start_sec,
            available,
            min_chunk_sec,
            max_chunk_sec,
            silence_sec,
            silence_db,
        )

        should_stop = stopping_requested(events_path, args.session_id)
        if should_stop and cut_time is None and available > 0.2:
            cut_time = duration
            reason = "stop_flush"

        if cut_time is not None and cut_time > next_start_sec + 0.05:
            ok_segment = extract_segment_to_file(
                source_audio, next_start_sec, cut_time, scratch_audio
            )
            chunk_id += 1

            if not ok_segment:
                append_event(
                    events_path,
                    {
                        "ts": iso_now(),
                        "type": "transcript_chunk",
                        "id": chunk_id,
                        "mode": "live",
                        "status": "error",
                        "reason": "segment_extract_failed",
                        "start_sec": round(next_start_sec, 3),
                        "end_sec": round(cut_time, 3),
                    },
                )
                next_start_sec = cut_time
                continue

            try:
                chunk_text = transcribe_path(
                    model, scratch_audio, args.batch_size, args.verbose, args.quiet
                ).strip()
                status = "ok" if chunk_text else "skipped"
                transcript_text = join_chunk_text(transcript_text, chunk_text)
                out_txt.parent.mkdir(parents=True, exist_ok=True)
                out_txt.write_text(transcript_text + ("\n" if transcript_text else ""), encoding="utf-8")
                append_event(
                    events_path,
                    {
                        "ts": iso_now(),
                        "type": "transcript_chunk",
                        "id": chunk_id,
                        "mode": "live",
                        "status": status,
                        "reason": reason,
                        "model": args.model,
                        "device": actual_device,
                        "start_sec": round(next_start_sec, 3),
                        "end_sec": round(cut_time, 3),
                        "chars": len(chunk_text),
                        "words": len(chunk_text.split()),
                    },
                )
            except Exception as e:  # noqa: BLE001
                append_event(
                    events_path,
                    {
                        "ts": iso_now(),
                        "type": "transcript_chunk",
                        "id": chunk_id,
                        "mode": "live",
                        "status": "error",
                        "reason": f"transcribe_failed:{type(e).__name__}",
                        "start_sec": round(next_start_sec, 3),
                        "end_sec": round(cut_time, 3),
                    },
                )

            next_start_sec = cut_time
            continue

        probe_silence_ms = int(max(0.0, trailing_silence_sec) * 1000.0)
        probe_key = (reason, probe_silence_ms // 100)
        if probe_key != last_probe_key:
            append_event(
                events_path,
                {
                    "ts": iso_now(),
                    "type": "transcript_probe",
                    "mode": "live",
                    "reason": reason,
                    "next_start_sec": round(next_start_sec, 3),
                    "available_sec": round(available, 3),
                    "trailing_silence_ms": probe_silence_ms,
                    "silence_target_ms": int(silence_sec * 1000.0),
                    "silence_db": round(silence_db, 3),
                    "current_db": (round(current_db, 3) if current_db is not None else None),
                    "min_chunk_sec": round(min_chunk_sec, 3),
                    "max_chunk_sec": (round(max_chunk_sec, 3) if max_chunk_sec is not None else None),
                },
            )
            last_probe_key = probe_key

        if should_stop:
            append_event(
                events_path,
                {
                    "ts": iso_now(),
                    "type": "transcription_worker_stopped",
                    "session_id": args.session_id,
                    "reason": "session_stopping_seen",
                    "chunks": chunk_id,
                    "processed_sec": round(next_start_sec, 3),
                },
            )
            try:
                scratch_audio.unlink(missing_ok=True)
            except Exception:
                pass
            return 0

        time.sleep(poll_sec)


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
        if args.watch_audio:
            return run_watch_audio(args)
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
