import argparse
import importlib.util
import json
import socket
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("agent-epic-closeout-probe.py")
SPEC = importlib.util.spec_from_file_location("agent_epic_closeout_probe", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/api/v1/agent/status":
            cookie = self.headers.get("Cookie", "")
            if "isy_session=opaque" not in cookie:
                self.send_response(401)
                self.end_headers()
                return
            body = b'{"enabled":true}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("Set-Cookie", "isy_session=opaque; HttpOnly; SameSite=Strict; Path=/api/v1")
        self.end_headers()
        self.wfile.write(b"shell")

    def log_message(self, *_args):
        pass


class CloseoutProbeTest(unittest.TestCase):
    def test_rejects_non_loopback_and_invalid_commit(self):
        with self.assertRaisesRegex(MODULE.ProbeError, "endpoint_not_loopback"):
            MODULE.loopback_endpoint("https://example.com:443")
        with self.assertRaisesRegex(MODULE.ProbeError, "invalid_implementation_commit"):
            MODULE.validate_commit("short", "implementation_commit")

    def test_occupied_listener_fails_closed(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            with self.assertRaisesRegex(MODULE.ProbeError, "listener_already_occupied"):
                MODULE.require_free_port("127.0.0.1", listener.getsockname()[1])

    def test_external_endpoint_report_is_redacted_and_requires_all_rows(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                observations = root / "rows.json"
                observations.write_text(
                    json.dumps({row: {"state": "pass", "code": "verified", "cleanup_complete": True}
                                for row in MODULE.REQUIRED_PRE_RC_ROWS}),
                    encoding="utf-8",
                )
                args = argparse.Namespace(
                    mode="pre-rc",
                    implementation_commit="a" * 40,
                    candidate_tree=None,
                    rc_commit=None,
                    endpoint=f"127.0.0.1:{server.server_port}",
                    daemon_bin=None,
                    config=None,
                    bind="127.0.0.1:8871",
                    runtime_root=str(root / "runtime"),
                    observations=str(observations),
                    codex_oauth_preflight=True,
                    startup_timeout=2.0,
                    out=str(root / "out.json"),
                )
                report, status = MODULE.run(args)
                self.assertEqual(status, 0)
                self.assertTrue(report["required_rows_pass"])
                self.assertTrue(report["agent_status_ready"])
                self.assertTrue(report["strict_session_cookie_observed"])
                self.assertTrue(report["codex_callback_port_free_before_oauth"])
                rendered = json.dumps(report)
                self.assertNotIn("opaque", rendered)
                self.assertFalse(report["redaction"]["tokens_included"])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_missing_rows_are_explicitly_not_run(self):
        rows = MODULE.load_observations(None)
        self.assertEqual(set(rows), set(MODULE.REQUIRED_PRE_RC_ROWS))
        self.assertTrue(all(row["state"] == "not_run" for row in rows.values()))

    def test_runtime_root_must_be_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "unowned").write_text("keep", encoding="utf-8")
            with self.assertRaisesRegex(MODULE.ProbeError, "runtime_root_not_empty"):
                MODULE.private_runtime_root(root)


if __name__ == "__main__":
    unittest.main()
