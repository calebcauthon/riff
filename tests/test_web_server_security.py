#!/usr/bin/env python3
"""Security regression tests for scripts/riff_web_server.py.

Covers the same-origin / loopback-only gate: Host allowlisting (DNS-rebinding
defense), Origin allowlisting, Sec-Fetch-Site cross-site rejection, absence of
any Access-Control-Allow-* headers, presence of hardening response headers,
path-confined /save-image, and refusal to bind non-loopback hosts.

Runs under plain unittest (stdlib only, no new dependencies):

    python3 tests/test_web_server_security.py

Also discoverable/runnable under pytest if it's installed:

    pytest tests/test_web_server_security.py -v
"""

from __future__ import annotations

import base64
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SERVER_SCRIPT = REPO_ROOT / "scripts" / "riff_web_server.py"

# A valid 1x1 PNG, base64-encoded, for /save-image payloads.
TINY_PNG_B64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY"
    "42YAAAAASUVORK5CYII="
)


def _free_port() -> int:
    """Pick a free ephemeral port by binding and releasing it."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_for_health(port: int, timeout: float = 15.0) -> None:
    """Poll /health with a headerless (CLI-like) request until ready."""
    deadline = time.time() + timeout
    last_err = None
    while time.time() < deadline:
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1.0)
            try:
                conn.request("GET", "/health")
                resp = conn.getresponse()
                body = resp.read()
                if resp.status == 200:
                    payload = json.loads(body.decode("utf-8"))
                    if payload.get("ok") is True:
                        return
            finally:
                conn.close()
        except (ConnectionRefusedError, OSError, socket.timeout) as e:
            last_err = e
        time.sleep(0.1)
    raise RuntimeError(f"server did not become healthy in {timeout}s (last error: {last_err})")


def _request(
    port: int,
    method: str,
    path: str,
    headers: dict | None = None,
    body: bytes | None = None,
    host: str | None = None,
) -> tuple[int, dict, bytes]:
    """Issue a raw HTTP request with explicit control over the Host header.

    http.client lets us override Host by passing it as the connection host
    only for the connection itself; to send an arbitrary/invalid Host header
    value we must set it explicitly via the headers dict (http.client will
    otherwise auto-generate one from the connection target).
    """
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5.0)
    try:
        hdrs = dict(headers or {})
        # putrequest/putheader lets us fully control the Host header,
        # including sending an intentionally wrong one (DNS-rebinding sim).
        conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        if host is not None:
            conn.putheader("Host", host)
        for k, v in hdrs.items():
            conn.putheader(k, v)
        if body is not None:
            conn.putheader("Content-Length", str(len(body)))
        conn.endheaders(message_body=body if body is not None else None)
        resp = conn.getresponse()
        resp_headers = {k: v for k, v in resp.getheaders()}
        resp_body = resp.read()
        return resp.status, resp_headers, resp_body
    finally:
        conn.close()


class WebServerSecurityTestBase(unittest.TestCase):
    """Starts one real server subprocess for the whole test class."""

    proc: subprocess.Popen
    port: int
    root: Path
    tmpdir: tempfile.TemporaryDirectory
    static_rel = "hello.txt"
    static_content = b"hello from static file\n"

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmpdir = tempfile.TemporaryDirectory(prefix="riff-web-sec-")
        cls.root = Path(cls.tmpdir.name)
        (cls.root / cls.static_rel).write_bytes(cls.static_content)

        cls.port = _free_port()
        env = dict(os.environ)
        cls.proc = subprocess.Popen(
            [
                sys.executable,
                str(SERVER_SCRIPT),
                "--root",
                str(cls.root),
                "--host",
                "127.0.0.1",
                "--port",
                str(cls.port),
                "--idle-timeout-sec",
                "3600",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
        )
        try:
            _wait_for_health(cls.port)
        except Exception:
            cls._terminate()
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        cls._terminate()
        cls.tmpdir.cleanup()

    @classmethod
    def _terminate(cls) -> None:
        proc = getattr(cls, "proc", None)
        if proc is None:
            return
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)

    def host_hdr(self) -> str:
        return f"127.0.0.1:{self.port}"

    def origin_hdr(self) -> str:
        return f"http://127.0.0.1:{self.port}"


class SameOriginRequestsSucceed(WebServerSecurityTestBase):
    def test_static_get_with_plain_host_succeeds(self):
        status, headers, body = _request(
            self.port, "GET", f"/{self.static_rel}", host=self.host_hdr()
        )
        self.assertEqual(status, 200)
        self.assertEqual(body, self.static_content)

    def test_static_get_with_same_origin_origin_header_succeeds(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            headers={"Origin": self.origin_hdr()},
            host=self.host_hdr(),
        )
        self.assertEqual(status, 200)
        self.assertEqual(body, self.static_content)

    def test_touch_post_headerless_succeeds(self):
        status, headers, body = _request(
            self.port, "POST", "/touch", body=b"", host=self.host_hdr()
        )
        self.assertEqual(status, 200)
        payload = json.loads(body.decode("utf-8"))
        self.assertTrue(payload.get("ok"))

    def test_touch_post_with_same_origin_origin_succeeds(self):
        status, headers, body = _request(
            self.port,
            "POST",
            "/touch",
            headers={"Origin": self.origin_hdr()},
            body=b"",
            host=self.host_hdr(),
        )
        self.assertEqual(status, 200)
        payload = json.loads(body.decode("utf-8"))
        self.assertTrue(payload.get("ok"))


class CrossOriginRequestsRejected(WebServerSecurityTestBase):
    def test_get_with_evil_origin_rejected(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            headers={"Origin": "http://evil.example"},
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def test_post_with_evil_origin_rejected(self):
        status, headers, body = _request(
            self.port,
            "POST",
            "/touch",
            headers={"Origin": "http://evil.example"},
            body=b"",
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def test_get_with_null_origin_rejected(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            headers={"Origin": "null"},
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def test_post_with_null_origin_rejected(self):
        status, headers, body = _request(
            self.port,
            "POST",
            "/touch",
            headers={"Origin": "null"},
            body=b"",
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def test_get_with_cross_site_sec_fetch_site_rejected(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            headers={"Sec-Fetch-Site": "cross-site"},
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def test_post_with_cross_site_sec_fetch_site_rejected(self):
        status, headers, body = _request(
            self.port,
            "POST",
            "/touch",
            headers={"Sec-Fetch-Site": "cross-site"},
            body=b"",
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def test_options_preflight_cross_site_origin_rejected_no_cors_headers(self):
        status, headers, body = _request(
            self.port,
            "OPTIONS",
            "/touch",
            headers={
                "Origin": "http://evil.example",
                "Access-Control-Request-Method": "POST",
            },
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self._assert_no_cors_headers(headers)

    def _assert_no_cors_headers(self, headers: dict) -> None:
        for k in headers:
            self.assertFalse(
                k.lower().startswith("access-control-allow"),
                f"unexpected CORS header present: {k}",
            )


class InvalidHostRejected(WebServerSecurityTestBase):
    def test_evil_host_rejected(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            host=f"evil.example:{self.port}",
        )
        self.assertEqual(status, 403)

    def test_wrong_port_host_rejected(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            host=f"127.0.0.1:{self.port + 1}",
        )
        self.assertEqual(status, 403)

    def test_missing_host_rejected(self):
        # HTTP/1.1 requires Host; http.client will still let us omit it via
        # skip_host + no explicit header, simulating a garbage/absent Host.
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            host=None,
        )
        self.assertEqual(status, 403)

    def test_garbage_host_rejected(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            host="not-a-real-host",
        )
        self.assertEqual(status, 403)


class ResponseHeaderHardening(WebServerSecurityTestBase):
    def test_success_response_has_hardening_headers_and_no_cors(self):
        status, headers, body = _request(
            self.port, "GET", f"/{self.static_rel}", host=self.host_hdr()
        )
        self.assertEqual(status, 200)
        lower = {k.lower(): v for k, v in headers.items()}
        self.assertEqual(lower.get("cross-origin-resource-policy"), "same-origin")
        self.assertEqual(lower.get("x-frame-options"), "DENY")
        self.assertEqual(lower.get("x-content-type-options"), "nosniff")
        self.assertEqual(lower.get("referrer-policy"), "no-referrer")
        self.assertIn("frame-ancestors 'none'", lower.get("content-security-policy", ""))
        for k in headers:
            self.assertFalse(k.lower().startswith("access-control-allow"))

    def test_error_response_has_hardening_headers_and_no_cors(self):
        status, headers, body = _request(
            self.port,
            "GET",
            f"/{self.static_rel}",
            headers={"Origin": "http://evil.example"},
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        lower = {k.lower(): v for k, v in headers.items()}
        self.assertEqual(lower.get("cross-origin-resource-policy"), "same-origin")
        self.assertEqual(lower.get("x-frame-options"), "DENY")
        self.assertEqual(lower.get("x-content-type-options"), "nosniff")
        self.assertEqual(lower.get("referrer-policy"), "no-referrer")
        for k in headers:
            self.assertFalse(k.lower().startswith("access-control-allow"))


class CrossOriginSaveImageDoesNotWrite(WebServerSecurityTestBase):
    def test_cross_origin_save_image_does_not_create_file(self):
        target_rel = "sessions/evil-session/screenshots/derived/pwned.png"
        target_path = self.root / target_rel
        self.assertFalse(target_path.exists())

        body = json.dumps(
            {
                "url": f"/{target_rel}",
                "dataUrl": f"data:image/png;base64,{TINY_PNG_B64}",
            }
        ).encode("utf-8")

        status, headers, resp_body = _request(
            self.port,
            "POST",
            "/save-image",
            headers={
                "Origin": "http://evil.example",
                "Content-Type": "application/json",
            },
            body=body,
            host=self.host_hdr(),
        )
        self.assertEqual(status, 403)
        self.assertFalse(
            target_path.exists(), "cross-origin /save-image must not write to disk"
        )


class BindRefusal(unittest.TestCase):
    def test_non_loopback_host_bind_refused(self):
        port = _free_port()
        proc = subprocess.Popen(
            [
                sys.executable,
                str(SERVER_SCRIPT),
                "--root",
                tempfile.mkdtemp(prefix="riff-web-sec-bind-"),
                "--host",
                "0.0.0.0",
                "--port",
                str(port),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            out, err = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate()
            self.fail("server did not exit promptly when given --host 0.0.0.0")

        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("non-loopback", err.lower())


if __name__ == "__main__":
    unittest.main()
