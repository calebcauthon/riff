#!/usr/bin/env python3

import argparse
import importlib.util
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "parakeet_transcribe.py"
SPEC = importlib.util.spec_from_file_location("parakeet_transcribe", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
parakeet = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = parakeet
SPEC.loader.exec_module(parakeet)


def server_args(perf_log: Path) -> argparse.Namespace:
    return argparse.Namespace(
        model="test/model",
        device="cpu",
        verbose=False,
        quiet=True,
        host="127.0.0.1",
        port=8765,
        startup_instance_id="instance-123",
        startup_spawn_epoch_ms=(time.time() * 1000.0) - 5.0,
        startup_trigger_session_id="20260718-120913",
        startup_trigger_action="start",
        startup_perf_log=str(perf_log),
    )


def read_events(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


class FakeServer:
    def __init__(self, _address, handler):
        self.handler = handler
        self.health_payload = None

    def serve_forever(self, poll_interval=0.2):
        request = self.handler.__new__(self.handler)
        request.path = "/health"
        request._send = lambda _code, payload: setattr(self, "health_payload", payload)
        self.handler.do_GET(request)

    def server_close(self):
        pass


class StartupTelemetryTests(unittest.TestCase):
    def test_ready_event_is_correlated_and_exposed_by_health(self):
        with tempfile.TemporaryDirectory() as tmp:
            perf_log = Path(tmp) / "perf.jsonl"
            args = server_args(perf_log)
            fake_server = FakeServer

            def fake_load_model(_model, _device, _verbose, _quiet, startup_phases):
                startup_phases.update(
                    dependency_import_ms=2.0,
                    model_load_ms=3.0,
                    model_placement_ms=1.0,
                )
                return object(), "cpu"

            created = []

            def make_server(address, handler):
                server = fake_server(address, handler)
                created.append(server)
                return server

            with mock.patch.object(parakeet, "load_model", side_effect=fake_load_model), mock.patch.object(
                parakeet, "ThreadingHTTPServer", side_effect=make_server
            ):
                self.assertEqual(parakeet.run_server(args), 0)

            events = read_events(perf_log)
            self.assertEqual(len(events), 1)
            event = events[0]
            self.assertEqual(event["action"], "parakeet_server_startup")
            self.assertEqual(event["status"], "ready")
            self.assertEqual(event["instance_id"], "instance-123")
            self.assertEqual(event["trigger_session_id"], "20260718-120913")
            self.assertEqual(event["trigger_action"], "start")
            self.assertEqual(event["model"], "test/model")
            self.assertEqual(event["device"], "cpu")
            self.assertGreater(event["pid"], 0)
            self.assertTrue(all(value >= 0 for value in event["phases"].values()))
            self.assertLess(abs(event["total_ms"] - sum(event["phases"].values())), 50.0)
            self.assertEqual(created[0].health_payload["startup"], event)

    def test_import_and_model_failures_emit_structured_error(self):
        cases = [
            ("dependency_import", 20),
            ("model_load", 21),
        ]
        for failed_phase, code in cases:
            with self.subTest(failed_phase=failed_phase), tempfile.TemporaryDirectory() as tmp:
                perf_log = Path(tmp) / "perf.jsonl"
                args = server_args(perf_log)
                error = parakeet.AppError("boom", code=code, failed_phase=failed_phase)
                with mock.patch.object(parakeet, "load_model", side_effect=error):
                    with self.assertRaises(parakeet.AppError):
                        parakeet.run_server(args)

                events = read_events(perf_log)
                self.assertEqual(len(events), 1)
                self.assertEqual(events[0]["status"], "error")
                self.assertEqual(events[0]["failed_phase"], failed_phase)
                self.assertEqual(events[0]["error"], "boom")

    def test_bind_failure_emits_structured_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            perf_log = Path(tmp) / "perf.jsonl"
            args = server_args(perf_log)
            with mock.patch.object(parakeet, "load_model", return_value=(object(), "cpu")), mock.patch.object(
                parakeet, "ThreadingHTTPServer", side_effect=OSError("address in use")
            ):
                with self.assertRaises(parakeet.AppError):
                    parakeet.run_server(args)

            events = read_events(perf_log)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["status"], "error")
            self.assertEqual(events[0]["failed_phase"], "server_bind")
            self.assertIn("address in use", events[0]["error"])


if __name__ == "__main__":
    unittest.main()
