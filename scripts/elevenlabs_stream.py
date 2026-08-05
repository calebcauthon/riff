#!/usr/bin/env python3
"""Stream riff's live recording to ElevenLabs Scribe v2 Realtime.

The recorder already writes 16 kHz mono pcm_s16le, which is exactly the format
the realtime API wants, so this sidecar tails the growing WAV rather than
requiring its own audio sink. Committed transcripts are written into riff's
provider-neutral transcript contract as they arrive:

  * ``transcript_chunk`` events appended to the session JSONL
  * merged text in ``<session_dir>/transcript.txt``

Signals:
  SIGUSR1  commit the current segment (``riff chunk`` / ``riff pause``)
  SIGUSR2  final commit, flush, exit
  SIGTERM  exit without waiting for a final commit

Exit codes:
  0  clean finish
  1  stream error (also recorded as an ``elevenlabs_stream_error`` event)
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import json
import os
import signal
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

try:
    import websockets
except ImportError:  # pragma: no cover - dependency guidance
    sys.stderr.write(
        "elevenlabs_stream.py requires the 'websockets' package.\n"
        "Install it with: pip install websockets\n"
    )
    sys.exit(1)

DEFAULT_WS_URL = "wss://api.elevenlabs.io/v1/speech-to-text/realtime"
# 32000 bytes == 1 second of 16 kHz mono 16-bit PCM, matching the chunk size in
# the ElevenLabs streaming guide.
BYTES_PER_SECOND = 32000
READ_POLL_SEC = 0.05
# How long a forced commit waits for the server to answer before giving up.
COMMIT_WAIT_SEC = 5.0
# Enough to cover the RIFF header plus ffmpeg's LIST/ISFT metadata chunk.
HEADER_SCAN_BYTES = 4096


def find_wav_data_offset(path: Path) -> int | None:
    """Byte offset of the first PCM sample in a RIFF/WAVE file.

    Not a fixed 44: ffmpeg writes a LIST/ISFT metadata chunk unless asked not
    to, so the header must actually be walked. Returns None while the file is
    too short to contain a complete header yet.
    """
    try:
        with path.open("rb") as fh:
            head = fh.read(HEADER_SCAN_BYTES)
    except FileNotFoundError:
        return None

    if len(head) < 12 or head[0:4] != b"RIFF" or head[8:12] != b"WAVE":
        return None

    pos = 12
    while pos + 8 <= len(head):
        chunk_id = head[pos : pos + 4]
        chunk_size = int.from_bytes(head[pos + 4 : pos + 8], "little")
        if chunk_id == b"data":
            return pos + 8
        # Chunks are word-aligned.
        pos += 8 + chunk_size + (chunk_size % 2)
    return None


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace(
        "+00:00", "Z"
    )


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Stream audio to ElevenLabs realtime STT")
    p.add_argument("--audio", required=True, help="Growing WAV file written by the recorder")
    p.add_argument("--session-dir", required=True, help="Session directory")
    p.add_argument("--events-path", required=True, help="Session events JSONL path")
    p.add_argument("--model-id", default="scribe_v2_realtime")
    p.add_argument("--sample-rate", type=int, default=16000)
    p.add_argument("--language-code", default=None)
    p.add_argument("--ws-url", default=DEFAULT_WS_URL)
    p.add_argument(
        "--commit-strategy",
        default="vad",
        choices=("vad", "manual"),
        help=(
            "vad: the server finalizes segments on natural pauses, so most text "
            "is already committed when stop runs. manual: nothing is finalized "
            "until riff asks, which pushes all the work into stop."
        ),
    )
    return p.parse_args()


class TranscriptSink:
    """Writes into riff's shared transcript contract."""

    def __init__(self, session_dir: Path, events_path: Path):
        self.session_dir = session_dir
        self.events_path = events_path
        self.transcript_path = session_dir / "transcript.txt"

    def _next_chunk_id(self) -> int:
        max_id = 0
        try:
            with self.events_path.open("r", encoding="utf-8") as fh:
                for line in fh:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if event.get("type") == "transcript_chunk":
                        try:
                            max_id = max(max_id, int(event.get("id", 0)))
                        except (TypeError, ValueError):
                            pass
        except FileNotFoundError:
            pass
        return max_id + 1

    def append_event(self, payload: dict) -> None:
        payload.setdefault("ts", now_iso())
        with self.events_path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(payload) + "\n")
            fh.flush()

    def commit_text(self, text: str, start_sec: float, end_sec: float, reason: str) -> None:
        trimmed = (text or "").strip()
        chunk_id = self._next_chunk_id()

        if trimmed:
            existing = ""
            if self.transcript_path.exists():
                existing = self.transcript_path.read_text(encoding="utf-8").strip()
            merged = f"{existing}\n\n{trimmed}" if existing else trimmed
            self.transcript_path.write_text(merged + "\n", encoding="utf-8")

        self.append_event(
            {
                "type": "transcript_chunk",
                "id": chunk_id,
                "mode": "stream",
                "status": "ok" if trimmed else "skipped",
                "reason": reason if trimmed else "empty_transcript",
                "start_sec": round(start_sec, 3),
                "end_sec": round(end_sec, 3),
                "chars": len(trimmed),
                "words": len(trimmed.split()),
                "transcription": {"status": "ok", "method": "elevenlabs_stream"},
            }
        )

    def error(self, reason: str, detail: str = "") -> None:
        self.append_event(
            {
                "type": "elevenlabs_stream_error",
                "reason": reason,
                "detail": detail,
            }
        )


class AudioTail:
    """Yields fixed-size PCM frames from a WAV file that is still being written."""

    def __init__(self, path: Path):
        self.path = path
        self.offset: int | None = None
        self.bytes_sent = 0

    async def wait_for_header(self, deadline_sec: float = 10.0) -> bool:
        """Block until the WAV header is complete enough to locate the PCM."""
        started = time.monotonic()
        while time.monotonic() - started < deadline_sec:
            data_offset = find_wav_data_offset(self.path)
            if data_offset is not None:
                self.offset = data_offset
                return True
            await asyncio.sleep(READ_POLL_SEC)
        return False

    def read_available(self, max_bytes: int = BYTES_PER_SECOND) -> bytes:
        if self.offset is None:
            data_offset = find_wav_data_offset(self.path)
            if data_offset is None:
                return b""
            self.offset = data_offset
        try:
            size = self.path.stat().st_size
        except FileNotFoundError:
            return b""
        if size <= self.offset:
            return b""
        want = min(max_bytes, size - self.offset)
        # Keep frames sample-aligned; a split 16-bit sample would inject noise.
        want -= want % 2
        if want <= 0:
            return b""
        with self.path.open("rb") as fh:
            fh.seek(self.offset)
            data = fh.read(want)
        self.offset += len(data)
        self.bytes_sent += len(data)
        return data

    def elapsed_sec(self, sample_rate: int) -> float:
        return self.bytes_sent / float(sample_rate * 2)


def build_url(args: argparse.Namespace) -> str:
    params = [
        f"model_id={args.model_id}",
        f"audio_format=pcm_{args.sample_rate}",
        # VAD by default: it finalizes segments during natural pauses, so stop
        # only has to flush a short tail instead of the whole recording. riff's
        # explicit boundaries (chunk/pause/stop) still force a commit on top.
        f"commit_strategy={args.commit_strategy}",
    ]
    if args.language_code:
        params.append(f"language_code={args.language_code}")
    return f"{args.ws_url}?{'&'.join(params)}"


async def run(args: argparse.Namespace) -> int:
    session_dir = Path(args.session_dir)
    sink = TranscriptSink(session_dir, Path(args.events_path))
    audio = AudioTail(Path(args.audio))

    api_key = os.environ.get("ELEVENLABS_API_KEY", "").strip()
    if not api_key:
        sink.error("missing_api_key", "ELEVENLABS_API_KEY was not set in the environment.")
        return 1

    commit_requested = asyncio.Event()
    finalize_requested = asyncio.Event()
    terminate_requested = asyncio.Event()

    loop = asyncio.get_running_loop()
    loop.add_signal_handler(signal.SIGUSR1, commit_requested.set)
    loop.add_signal_handler(signal.SIGUSR2, finalize_requested.set)
    loop.add_signal_handler(signal.SIGTERM, terminate_requested.set)
    loop.add_signal_handler(signal.SIGINT, terminate_requested.set)

    if not await audio.wait_for_header():
        sink.error(
            "audio_header_timeout",
            f"No readable WAV header appeared at {audio.path} within 10s.",
        )
        return 1

    url = build_url(args)
    # Where the current uncommitted segment began, so chunk events carry real
    # audio offsets rather than guesses.
    segment_start_sec = 0.0
    # Committed segments are written straight through as they arrive, so the
    # transcript grows live and stop only has to flush the tail. This counter
    # is how a forced commit knows its result has landed.
    committed_count = 0

    try:
        async with websockets.connect(
            url,
            additional_headers={"xi-api-key": api_key},
            max_size=None,
            ping_interval=20,
            ping_timeout=20,
        ) as ws:
            sink.append_event({"type": "elevenlabs_stream_connected", "model_id": args.model_id})

            async def receive() -> None:
                nonlocal segment_start_sec, committed_count
                async for raw in ws:
                    try:
                        msg = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    kind = msg.get("message_type", "")

                    if kind in ("committed_transcript", "final_transcript"):
                        text = (msg.get("text") or "").strip()
                        committed_count += 1
                        if text:
                            end_sec = audio.elapsed_sec(args.sample_rate)
                            sink.commit_text(
                                text, segment_start_sec, end_sec, "stream_commit"
                            )
                            segment_start_sec = end_sec
                    elif kind == "partial_transcript":
                        # Interim text is intentionally not written to the
                        # transcript: it can change, and riff's contract is
                        # append-only.
                        continue
                    elif kind.endswith("error") or kind in (
                        "quota_exceeded",
                        "rate_limited",
                        "unaccepted_terms",
                        "resource_exhausted",
                        "queue_overflow",
                        "session_time_limit_exceeded",
                        "chunk_size_exceeded",
                        "insufficient_audio_activity",
                        "transcriber_error",
                        "commit_throttled",
                    ):
                        sink.error(kind, json.dumps(msg))
                        raise RuntimeError(f"ElevenLabs stream error: {kind}")

            receiver = asyncio.create_task(receive())

            async def send_chunk(data: bytes, commit: bool) -> None:
                payload = {
                    "message_type": "input_audio_chunk",
                    "audio_base_64": base64.b64encode(data).decode("ascii"),
                    "commit": commit,
                }
                await ws.send(json.dumps(payload))

            async def flush_commit(reason: str) -> None:
                """Force the server to finalize whatever it is still holding.

                The committed text itself is written by `receive`; this only
                pushes the remaining audio, asks for a commit, and waits for it
                to land so `riff stop` does not return before the tail arrives.
                """
                t0 = time.monotonic()
                drained = 0
                while True:
                    data = audio.read_available()
                    if not data:
                        break
                    drained += len(data)
                    await send_chunk(data, False)
                await send_chunk(b"", True)
                t_sent = time.monotonic()

                before = committed_count
                deadline = time.monotonic() + COMMIT_WAIT_SEC
                while time.monotonic() < deadline and committed_count == before:
                    if receiver.done():
                        break
                    await asyncio.sleep(0.005)
                t_done = time.monotonic()

                sink.append_event(
                    {
                        "type": "elevenlabs_commit_timing",
                        "reason": reason,
                        "drain_send_ms": round((t_sent - t0) * 1000, 1),
                        "await_commit_ms": round((t_done - t_sent) * 1000, 1),
                        "drained_bytes": drained,
                    }
                )

                if committed_count == before:
                    # Nothing came back: record the boundary so the gap is
                    # visible in the session rather than silently missing.
                    sink.append_event(
                        {
                            "type": "elevenlabs_commit_timeout",
                            "reason": reason,
                            "waited_sec": COMMIT_WAIT_SEC,
                        }
                    )

            while True:
                if receiver.done():
                    # Propagate a receive-side failure instead of streaming on.
                    receiver.result()
                    break
                if terminate_requested.is_set():
                    break
                if finalize_requested.is_set():
                    await flush_commit("stop_flush")
                    break
                if commit_requested.is_set():
                    commit_requested.clear()
                    await flush_commit("manual_chunk")
                    continue

                data = audio.read_available()
                if data:
                    await send_chunk(data, False)
                else:
                    await asyncio.sleep(READ_POLL_SEC)

            if not receiver.done():
                receiver.cancel()
            sink.append_event({"type": "elevenlabs_stream_finished"})
            return 0

    except Exception as exc:  # noqa: BLE001 - any failure must be visible to stop
        sink.error(type(exc).__name__, str(exc))
        return 1


def main() -> int:
    args = parse_args()
    try:
        return asyncio.run(run(args))
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())
