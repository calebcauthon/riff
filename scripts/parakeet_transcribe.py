#!/usr/bin/env python3
"""
Local Parakeet transcription helper for ispy.

Usage:
  python scripts/parakeet_transcribe.py \
    --audio /tmp/ispy/sessions/<id>/audio.wav \
    --out-txt /tmp/ispy/sessions/<id>/transcript.txt \
    --model nvidia/parakeet-tdt-0.6b-v2
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from dataclasses import dataclass
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
    p = argparse.ArgumentParser(description="Transcribe an audio file with NVIDIA Parakeet (NeMo).")
    p.add_argument("--audio", required=True, help="Input audio file path")
    p.add_argument("--out-txt", required=True, help="Output transcript text file")
    p.add_argument(
        "--model",
        default=os.environ.get("ISPY_PARAKEET_MODEL", "nvidia/parakeet-tdt-0.6b-v2"),
        help="Parakeet model name (NeMo pretrained id)",
    )
    p.add_argument(
        "--device",
        default=os.environ.get("ISPY_PARAKEET_DEVICE", "auto"),
        choices=["auto", "cpu", "cuda"],
        help="Inference device",
    )
    p.add_argument("--batch-size", type=int, default=1, help="Transcription batch size")
    p.add_argument("--verbose", action="store_true", help="Verbose logging")
    p.add_argument("--quiet", action="store_true", help="Suppress non-json output")
    p.add_argument("--json", action="store_true", help="Emit JSON summary")
    return p.parse_args()


def choose_device(requested: str) -> str:
    if requested in {"cpu", "cuda"}:
        return requested
    # auto
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

    vout(f"Loading model '{model_name}' on {map_location}", verbose, quiet)
    try:
        model = ASRModel.from_pretrained(model_name=model_name, map_location=map_location)
    except Exception as e:
        raise AppError(f"Failed to load Parakeet model '{model_name}': {e}", code=21) from e

    # Best-effort move to device
    try:
        if target == "cuda":
            model = model.cuda()  # type: ignore[attr-defined]
        else:
            model = model.cpu()  # type: ignore[attr-defined]
    except Exception:
        pass

    return model, target


def normalize_transcript(raw: Any) -> str:
    # NeMo can return list[str], list[hypothesis], or other forms depending on version/settings.
    if isinstance(raw, str):
        return raw.strip()

    if isinstance(raw, list) and raw:
        first = raw[0]
        if isinstance(first, str):
            return first.strip()
        text = getattr(first, "text", None)
        if isinstance(text, str):
            return text.strip()
        # fallback
        return str(first).strip()

    return str(raw).strip()


def main() -> int:
    args = parse_args()

    try:
        audio = Path(args.audio).expanduser().resolve()
        out_txt = Path(args.out_txt).expanduser().resolve()

        if not audio.exists() or not audio.is_file():
            raise AppError(f"Audio file not found: {audio}", code=2)

        out_txt.parent.mkdir(parents=True, exist_ok=True)

        model, actual_device = load_model(args.model, args.device, args.verbose, args.quiet)

        vout(f"Transcribing: {audio}", args.verbose, args.quiet)
        try:
            result = model.transcribe([str(audio)], batch_size=args.batch_size)
        except TypeError:
            # Some versions have a slightly different signature
            result = model.transcribe([str(audio)])
        except Exception as e:
            raise AppError(f"Transcription failed: {e}", code=22) from e

        text = normalize_transcript(result)
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

    except AppError as e:
        payload = {"ok": False, "error": str(e), "code": e.code}
        if args.json:
            print(json.dumps(payload, indent=2), file=sys.stderr)
        else:
            print(f"Error: {e}", file=sys.stderr)
        return e.code


if __name__ == "__main__":
    raise SystemExit(main())
