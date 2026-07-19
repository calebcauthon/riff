#!/usr/bin/env python3

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import parakeet_transcribe


class ServerIdentityMismatchTests(unittest.TestCase):
    def setUp(self) -> None:
        self.identity = {
            "protocol_version": 1,
            "server_instance_id": "instance-1",
            "riff_root": "/tmp/riff-test",
            "model": "model-a",
            "model_revision": "main",
            "requested_device": "auto",
            "device": "cpu",
        }

    def test_exact_identity_is_accepted(self) -> None:
        self.assertEqual(
            parakeet_transcribe.server_identity_mismatches(
                dict(self.identity), self.identity
            ),
            {},
        )

    def test_different_model_is_rejected(self) -> None:
        request = dict(self.identity, model="model-b")
        self.assertEqual(
            parakeet_transcribe.server_identity_mismatches(request, self.identity),
            {"model": {"requested": "model-b", "actual": "model-a"}},
        )

    def test_missing_identity_fields_are_rejected(self) -> None:
        mismatches = parakeet_transcribe.server_identity_mismatches({}, self.identity)
        self.assertEqual(
            set(mismatches), set(parakeet_transcribe.PARAKEET_REQUEST_IDENTITY_FIELDS)
        )


if __name__ == "__main__":
    unittest.main()
