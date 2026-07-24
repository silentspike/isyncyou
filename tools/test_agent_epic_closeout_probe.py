import argparse
import copy
import importlib.util
import json
import socket
import sys
import tempfile
import threading
import unittest
import zipfile
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse


MODULE_PATH = Path(__file__).with_name("agent-epic-closeout-probe.py")
SPEC = importlib.util.spec_from_file_location("agent_epic_closeout_probe", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def valid_release_snapshot():
    timestamp = "2026-07-20T12:00:00Z"
    return {
        "id": 42,
        "tag_name": "v1.0.0-rc.9",
        "draft": False,
        "prerelease": True,
        "target_commitish": "a" * 40,
        "created_at": timestamp,
        "published_at": timestamp,
        "updated_at": timestamp,
        "html_url": "https://github.com/silentspike/isyncyou/releases/tag/v1.0.0-rc.9",
        "assets": [
            {
                "id": index,
                "name": name,
                "size": 100 + index,
                "digest": "sha256:" + f"{index:064x}",
                "created_at": timestamp,
                "updated_at": timestamp,
            }
            for index, name in enumerate(sorted(MODULE.EXPECTED_RELEASE_ASSETS), start=1)
        ],
    }


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


class WeakCookieHandler(Handler):
    def do_GET(self):
        if self.path == "/":
            self.send_response(200)
            self.send_header(
                "Set-Cookie", "isy_session=opaque; SameSite=Lax; Path=/api/v1"
            )
            self.end_headers()
            self.wfile.write(b"shell")
            return
        super().do_GET()


class AndroidBridgeHandler(BaseHTTPRequestHandler):
    target_document = [
        {
            "type": "page",
            "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/opaque",
            "url": "https://appassets.androidplatform.net/",
        }
    ]

    def do_GET(self):
        if self.path != "/json":
            self.send_response(404)
            self.end_headers()
            return
        body = json.dumps(type(self).target_document).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


class RetrievalHandler(BaseHTTPRequestHandler):
    request_id = None
    turn_posts = 0
    cap = "agent-capability-value-opaque"

    def send_bytes(self, content_type, body, *, cookie=False):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        if cookie:
            self.send_header(
                "Set-Cookie",
                "isy_session=opaque; HttpOnly; SameSite=Strict; Path=/api/v1",
            )
        self.end_headers()
        self.wfile.write(body)

    def send_json(self, value):
        self.send_bytes(
            "application/json", json.dumps(value, separators=(",", ":")).encode()
        )

    def cap_ready(self):
        return self.headers.get("X-Capability-Token") == self.cap

    def do_GET(self):
        parsed = urlparse(self.path)
        query = parse_qs(parsed.query)
        if parsed.path == "/":
            self.send_bytes("text/html", b"shell", cookie=True)
        elif parsed.path == "/app.js":
            self.send_bytes(
                "application/javascript",
                f'const CAP = {{ agent: "{self.cap}" }};'.encode(),
            )
        elif parsed.path == "/api/v1/agent/status":
            self.send_json(
                {"enabled": True, "connected": True, "selected_provider": "codex"}
            )
        elif parsed.path == "/api/v1/accounts":
            self.send_json(
                {"accounts": [{"id": "controlled", "username": "private@example"}]}
            )
        elif parsed.path == "/api/v1/agent/session/list" and self.cap_ready():
            self.send_json(
                {
                    "sessions": [
                        {
                            "session_id": "01JSESSION00000000000000000",
                            "archived": False,
                        }
                    ],
                    "selected_session_id": "01JSESSION00000000000000000",
                    "next_cursor": None,
                }
            )
        elif parsed.path == "/api/v1/agent/stream":
            body = (
                b'data: {"event":"token","text":"PRIVATE ANSWER"}\n\n'
                b'data: {"event":"tool_result","id":"private-id",'
                b'"content":"PRIVATE TOOL RESULT","untrusted":true}\n\n'
                b'data: {"event":"done","reason":"complete"}\n\n'
                b'event: done\ndata: {}\n\n'
            )
            self.send_bytes("text/event-stream", body)
        elif parsed.path == "/api/v1/agent/session/history" and self.cap_ready():
            records = [
                {
                    "request_id": self.request_id,
                    "turn_id": "01JTURN0000000000000000000",
                    "kind": {"kind": "turn_intent", "user_text": "PRIVATE PROMPT"},
                },
                {
                    "request_id": self.request_id,
                    "turn_id": "01JTURN0000000000000000000",
                    "kind": {
                        "kind": "assistant_result",
                        "text": "PRIVATE ANSWER",
                        "sources": [
                            {
                                "service": "mail",
                                "item_id": "private-item-id",
                                "label": "PRIVATE LABEL",
                            }
                        ],
                        "usage": None,
                    },
                },
                {
                    "request_id": self.request_id,
                    "turn_id": "01JTURN0000000000000000000",
                    "kind": {
                        "kind": "turn_terminal",
                        "status": "complete",
                        "error_code": None,
                    },
                },
            ]
            self.send_json({"records": records, "next_cursor": None})
        elif parsed.path == "/api/v1/agent/request/status" and self.cap_ready():
            self.send_json(
                {"state": "committed", "code": "ok", "terminal": True, "resume_allowed": False}
            )
        elif parsed.path == "/api/v1/items":
            self.send_json(
                {
                    "items": [
                        {
                            "service": "mail",
                            "remote_id": "private-item-id",
                            "name": "PRIVATE LABEL",
                        }
                    ],
                    "count": 1,
                    "total": 1,
                    "limit": 1000,
                    "offset": 0,
                }
            )
        elif parsed.path == "/api/v1/view" and query.get("id") == ["private-item-id"]:
            self.send_bytes("text/html", b"<html>PRIVATE BODY</html>")
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        parsed = urlparse(self.path)
        if not self.cap_ready() or self.headers.get("Origin") is None:
            self.send_response(401)
            self.end_headers()
            return
        size = int(self.headers.get("Content-Length", "0"))
        document = json.loads(self.rfile.read(size))
        if parsed.path == "/api/v1/agent/turn":
            type(self).turn_posts += 1
            type(self).request_id = document["request_id"]
            self.send_json({"turn": "01JTURN0000000000000000000"})
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, *_args):
        pass


class CloseoutProbeTest(unittest.TestCase):
    def test_final_release_integrity_requires_unchanged_snapshots_and_tag_commit(self):
        release = valid_release_snapshot()
        fetches = []
        sleeps = []

        def fetcher(_repository, _tag):
            fetches.append(True)
            return copy.deepcopy(release)

        result = MODULE.inspect_release_integrity(
            "silentspike/isyncyou",
            "v1.0.0-rc.9",
            "a" * 40,
            30,
            release_fetcher=fetcher,
            tag_resolver=lambda _repository, _tag: "a" * 40,
            public_url_probe=lambda _repository, _tag: True,
            sleeper=sleeps.append,
        )
        self.assertEqual(result["state"], "pass")
        self.assertEqual(result["recheck_delay_seconds"], 30)
        self.assertEqual(len(fetches), 2)
        self.assertEqual(sleeps, [30])
        self.assertEqual(result["snapshot"]["release_id"], 42)

    def test_final_release_integrity_rejects_mutation_or_moved_tag(self):
        release = valid_release_snapshot()
        changed = copy.deepcopy(release)
        changed["updated_at"] = "2026-07-20T12:01:00Z"
        snapshots = iter((release, changed))
        with self.assertRaisesRegex(MODULE.ProbeError, "release_integrity_changed"):
            MODULE.inspect_release_integrity(
                "silentspike/isyncyou",
                "v1.0.0-rc.9",
                "a" * 40,
                30,
                release_fetcher=lambda _repository, _tag: copy.deepcopy(next(snapshots)),
                tag_resolver=lambda _repository, _tag: "a" * 40,
                public_url_probe=lambda _repository, _tag: True,
                sleeper=lambda _delay: None,
            )
        commits = iter(("a" * 40, "b" * 40))
        with self.assertRaisesRegex(MODULE.ProbeError, "release_integrity_changed"):
            MODULE.inspect_release_integrity(
                "silentspike/isyncyou",
                "v1.0.0-rc.9",
                "a" * 40,
                30,
                release_fetcher=lambda _repository, _tag: copy.deepcopy(release),
                tag_resolver=lambda _repository, _tag: next(commits),
                public_url_probe=lambda _repository, _tag: True,
                sleeper=lambda _delay: None,
            )

    def test_final_candidate_tree_must_equal_rc_commit_tree(self):
        self.assertTrue(
            MODULE.verify_candidate_tree(
                "a" * 40,
                "b" * 40,
                tree_resolver=lambda _commit: "b" * 40,
            )
        )
        with self.assertRaisesRegex(MODULE.ProbeError, "rc_candidate_tree_mismatch"):
            MODULE.verify_candidate_tree(
                "a" * 40,
                "b" * 40,
                tree_resolver=lambda _commit: "c" * 40,
            )

    def test_rejects_non_loopback_and_invalid_commit(self):
        with self.assertRaisesRegex(MODULE.ProbeError, "endpoint_not_loopback"):
            MODULE.loopback_endpoint("https://example.com:443")
        with self.assertRaisesRegex(MODULE.ProbeError, "invalid_implementation_commit"):
            MODULE.validate_commit("short", "implementation_commit")
        with self.assertRaisesRegex(MODULE.ProbeError, "unknown_implementation_commit"):
            MODULE.validate_git_object("0" * 40, "commit", "implementation_commit")

    def test_occupied_listener_fails_closed(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            with self.assertRaisesRegex(MODULE.ProbeError, "listener_already_occupied"):
                MODULE.require_free_port("127.0.0.1", listener.getsockname()[1])

    def test_android_bridge_target_is_loopback_bounded_and_redacted(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), AndroidBridgeHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            result = MODULE.inspect_android_bridge(f"127.0.0.1:{server.server_port}")
            self.assertEqual(
                result,
                {"state": "pass", "code": "verified", "page_target_ready": True},
            )
            self.assertNotIn("appassets", json.dumps(result))
            self.assertNotIn("devtools", json.dumps(result))
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

        with self.assertRaisesRegex(MODULE.ProbeError, "endpoint_not_loopback"):
            MODULE.inspect_android_bridge("example.com:9222")

    def test_android_bridge_rejects_missing_page_and_oversize_response(self):
        class MissingPageHandler(AndroidBridgeHandler):
            target_document = [{"type": "worker"}]

        server = ThreadingHTTPServer(("127.0.0.1", 0), MissingPageHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with self.assertRaisesRegex(MODULE.ProbeError, "android_bridge_page_unavailable"):
                MODULE.inspect_android_bridge(f"127.0.0.1:{server.server_port}")
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_request_status_readback_is_closed_and_validated(self):
        client = mock.Mock()
        client.json.return_value = {"state": "committed", "code": "ok", "terminal": True}
        self.assertEqual(
            MODULE.read_request_status(client, "opaque=query"),
            ("committed", "ok", True),
        )
        client.json.side_effect = MODULE.ProbeError("request_status_failed")
        self.assertEqual(
            MODULE.read_request_status(client, "opaque=query"),
            ("unavailable", "request_status_unavailable", False),
        )
        client.json.side_effect = None
        client.json.return_value = {"state": "PRIVATE STATE", "code": "ok", "terminal": True}
        with self.assertRaisesRegex(MODULE.ProbeError, "retrieval_request_status_invalid"):
            MODULE.read_request_status(client, "opaque=query")

        class OversizeHandler(AndroidBridgeHandler):
            target_document = [{"type": "page", "padding": "x" * 70000}]

        server = ThreadingHTTPServer(("127.0.0.1", 0), OversizeHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with self.assertRaisesRegex(MODULE.ProbeError, "android_bridge_target_too_large"):
                MODULE.inspect_android_bridge(f"127.0.0.1:{server.server_port}")
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_external_endpoint_report_is_redacted_and_requires_all_rows(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                observations = root / "rows.json"
                observations.write_text(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "implementation_commit": "a" * 40,
                            "rows": {
                                row: {
                                    "checks": {
                                        check: True
                                        for check in MODULE.ROW_REQUIREMENTS[row]
                                    }
                                }
                                for row in MODULE.REQUIRED_PRE_RC_ROWS
                            },
                        }
                    ),
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
                with mock.patch.object(
                    MODULE, "validate_git_object", side_effect=lambda value, *_args: value
                ):
                    report, status = MODULE.run(args)
                self.assertEqual(status, 0)
                self.assertTrue(report["required_rows_pass"])
                self.assertTrue(report["agent_status_ready"])
                self.assertTrue(report["strict_session_cookie_observed"])
                self.assertTrue(report["codex_callback_port_free_before_oauth"])
                self.assertRegex(report["observation_document_sha256"], r"^[0-9a-f]{64}$")
                rendered = json.dumps(report)
                self.assertNotIn("opaque", rendered)
                self.assertFalse(report["redaction"]["tokens_included"])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_missing_rows_are_explicitly_not_run(self):
        rows, digest = MODULE.load_observations(None, "a" * 40)
        self.assertEqual(set(rows), set(MODULE.REQUIRED_PRE_RC_ROWS))
        self.assertTrue(all(row["state"] == "not_run" for row in rows.values()))
        self.assertIsNone(digest)

    def test_observation_status_cannot_be_supplied_directly(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rows.json"
            path.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "implementation_commit": "a" * 40,
                        "rows": {"A": {"state": "pass", "code": "verified"}},
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(MODULE.ProbeError, "invalid_observations"):
                MODULE.load_observations(path, "a" * 40)

    def test_observations_are_commit_bound_and_fail_on_false_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rows.json"
            checks = {name: True for name in MODULE.ROW_REQUIREMENTS["A"]}
            checks[MODULE.ROW_REQUIREMENTS["A"][0]] = False
            path.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "implementation_commit": "a" * 40,
                        "rows": {"A": {"checks": checks}},
                    }
                ),
                encoding="utf-8",
            )
            rows, _digest = MODULE.load_observations(path, "a" * 40)
            self.assertEqual(rows["A"]["state"], "fail")
            self.assertEqual(rows["A"]["code"], "required_check_failed")
            with self.assertRaisesRegex(MODULE.ProbeError, "observation_commit_mismatch"):
                MODULE.load_observations(path, "b" * 40)

    def test_observations_reject_duplicate_json_members(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rows.json"
            path.write_text(
                '{"schema_version":1,"schema_version":1,'
                '"implementation_commit":"' + "a" * 40 + '","rows":{}}',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(MODULE.ProbeError, "invalid_observations"):
                MODULE.load_observations(path, "a" * 40)

    def test_non_strict_session_cookie_cannot_pass_closeout(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), WeakCookieHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                observations = root / "rows.json"
                observations.write_text(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "implementation_commit": "a" * 40,
                            "rows": {
                                row: {
                                    "checks": {
                                        check: True
                                        for check in MODULE.ROW_REQUIREMENTS[row]
                                    }
                                }
                                for row in MODULE.REQUIRED_PRE_RC_ROWS
                            },
                        }
                    ),
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
                    codex_oauth_preflight=False,
                    startup_timeout=2.0,
                    out=str(root / "out.json"),
                )
                with mock.patch.object(
                    MODULE, "validate_git_object", side_effect=lambda value, *_args: value
                ):
                    report, status = MODULE.run(args)
                self.assertEqual(status, 2)
                self.assertFalse(report["strict_session_cookie_observed"])
                self.assertFalse(report["required_rows_pass"])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_retrieval_turn_retries_once_and_reports_only_redacted_facts(self):
        self.assertEqual(MODULE.CONTROL_REQUEST_TIMEOUT_SECONDS, 10.0)
        RetrievalHandler.request_id = None
        RetrievalHandler.turn_posts = 0
        server = ThreadingHTTPServer(("127.0.0.1", 0), RetrievalHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                prompt = Path(tmp) / "prompt.txt"
                prompt.write_text("Find the controlled fixture {fixture}.", encoding="utf-8")
                prompt.chmod(0o600)
                base = f"http://127.0.0.1:{server.server_port}"
                client, shell, strict_cookie, status_ready, status_document = (
                    MODULE.wait_for_runtime(base, None, 2.0)
                )
                self.assertTrue(shell)
                self.assertTrue(strict_cookie)
                self.assertTrue(status_ready)
                result = MODULE.run_retrieval_turn(
                    client, status_document, prompt, None, 2.0
                )
                self.assertEqual(result["state"], "pass")
                self.assertEqual(result["provider"], "codex")
                self.assertTrue(result["retry_reused_turn"])
                self.assertTrue(result["transcript_rehydrated"])
                self.assertTrue(result["all_sources_listed_and_viewable"])
                self.assertEqual(RetrievalHandler.turn_posts, 2)
                rendered = json.dumps(result)
                for forbidden in (
                    "PRIVATE ANSWER",
                    "PRIVATE TOOL RESULT",
                    "PRIVATE PROMPT",
                    "private-item-id",
                    "private@example",
                    RetrievalHandler.cap,
                    RetrievalHandler.request_id,
                ):
                    self.assertNotIn(forbidden, rendered)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_retrieval_timeout_reports_only_closed_persisted_request_state(self):
        RetrievalHandler.request_id = None
        RetrievalHandler.turn_posts = 0
        server = ThreadingHTTPServer(("127.0.0.1", 0), RetrievalHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                prompt = Path(tmp) / "prompt.txt"
                prompt.write_text("Find {fixture}.", encoding="utf-8")
                prompt.chmod(0o600)
                base = f"http://127.0.0.1:{server.server_port}"
                client, _, _, _, status_document = MODULE.wait_for_runtime(base, None, 2.0)
                with mock.patch.object(
                    MODULE,
                    "stream_turn",
                    side_effect=MODULE.ProbeError("turn_stream_timed_out"),
                ):
                    result = MODULE.run_retrieval_turn(
                        client, status_document, prompt, None, 2.0
                    )
                self.assertEqual(result["state"], "fail")
                self.assertEqual(result["terminal_reason"], "timeout")
                self.assertEqual(result["error_code"], "turn_stream_timed_out")
                self.assertEqual(result["request_status_state"], "committed")
                self.assertEqual(result["request_status_code"], "ok")
                rendered = json.dumps(result)
                self.assertNotIn("PRIVATE", rendered)
                self.assertNotIn(RetrievalHandler.cap, rendered)
                self.assertNotIn(RetrievalHandler.request_id, rendered)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=2)

    def test_private_prompt_rejects_group_or_world_access(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "prompt.txt"
            path.write_text("fixture {fixture}", encoding="utf-8")
            path.chmod(0o644)
            with self.assertRaisesRegex(MODULE.ProbeError, "invalid_prompt_file"):
                MODULE.read_private_text(path, "invalid_prompt_file", 1024)

    def test_apk_matrix_requires_hook_markers_and_clean_distinct_default(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            hook = root / "hooks.apk"
            default = root / "default.apk"
            with zipfile.ZipFile(hook, "w") as archive:
                archive.writestr(
                    "lib/arm64-v8a/libisyncyou_mobile.so",
                    b"hook\0" + b"\0".join(MODULE.HOOK_MARKERS),
                )
            with zipfile.ZipFile(default, "w") as archive:
                archive.writestr(
                    "lib/arm64-v8a/libisyncyou_mobile.so", b"clean-default"
                )
            matrix = MODULE.inspect_apk_matrix(hook, default, None)
            self.assertEqual(matrix["state"], "pass")
            self.assertTrue(matrix["artifacts_distinct"])
            self.assertTrue(matrix["hook"]["expected_markers_present"])
            self.assertTrue(matrix["default"]["hook_markers_absent"])

    def test_runtime_root_must_be_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "unowned").write_text("keep", encoding="utf-8")
            with self.assertRaisesRegex(MODULE.ProbeError, "runtime_root_not_empty"):
                MODULE.private_runtime_root(root)


if __name__ == "__main__":
    unittest.main()
