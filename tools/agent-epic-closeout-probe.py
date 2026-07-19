#!/usr/bin/env python3
"""Run and reduce the #628 closeout runtime boundary.

The probe owns a candidate daemon when requested, keeps cookies and raw process
output in owner-only temporary files, verifies the shell and Agent status path,
and reduces externally observed OAuth/device rows to closed evidence facts. It
deliberately does not automate provider credentials or confirmation prompts.
"""

from __future__ import annotations

import argparse
import hashlib
import http.cookiejar
import json
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlparse


SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SAFE_CODE_RE = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
AGENT_CAP_RE = re.compile(r'\bagent:\s*"([^"\\]{16,512})"')
MAX_JSON_BYTES = 1024 * 1024
MAX_SSE_BYTES = 1024 * 1024
MAX_SSE_LINE_BYTES = 256 * 1024
MAX_SSE_EVENTS = 4096
MAX_CDP_TARGET_BYTES = 64 * 1024
CONTROL_REQUEST_TIMEOUT_SECONDS = 10.0
HOOK_MARKERS = (
    b"ISY_MOBILE_JOB_DEVICE_HOOK_V1",
    b"ISY_AGENT_NETWORK_DEVICE_HOOK_V1",
    b"ISY_AGENT_CREDENTIAL_STORE_SELF_TEST_V1",
)
REQUIRED_PRE_RC_ROWS = tuple("ABCDEFGHIJKLM")
ROW_REQUIREMENTS: dict[str, tuple[str, ...]] = {
    "A": (
        "oauth_ready",
        "source_resolved",
        "transcript_rehydrated",
        "local_cli_absent",
        "default_harness_absent",
    ),
    "B": (
        "oauth_ready",
        "source_resolved",
        "transcript_rehydrated",
        "store_false_preserved",
        "identity_envelope_preserved",
        "local_cli_absent",
        "default_harness_absent",
    ),
    "C": (
        "default_apk",
        "oauth_completed",
        "network_guard_spanned_turn",
        "source_resolved",
        "request_not_duplicated",
        "transcript_rehydrated",
        "hooks_absent",
    ),
    "D": (
        "default_apk",
        "oauth_completed",
        "network_guard_spanned_turn",
        "source_resolved",
        "request_not_duplicated",
        "transcript_rehydrated",
        "hooks_absent",
    ),
    "E": (
        "retrieved_content_marked_untrusted",
        "no_unconfirmed_mutation",
        "no_exfiltration",
        "pending_cancelled_or_absent",
    ),
    "F": (
        "cancel_matrix_complete",
        "one_terminal_per_case",
        "zero_mutations",
        "cancelled_pending_not_confirmable",
    ),
    "G": (
        "confirmation_required",
        "exactly_one_effect",
        "graph_or_store_verified",
        "restore_local_exactly_once",
        "fixtures_reverted",
    ),
    "H": (
        "hook_apk",
        "network_fault_recovered",
        "offline_unleased_absent",
        "effect_exactly_once",
        "hooks_present",
        "fixtures_reverted",
    ),
    "I": (
        "default_apk",
        "device_credential_confirmed",
        "foreground_job_visible",
        "notification_denial_failed_closed",
        "no_duplicate_after_restart",
        "hooks_absent",
        "fixtures_reverted",
    ),
    "J": (
        "desktop_android_continuation",
        "linear_manifest_head",
        "idempotent_retry",
        "multi_step_recovery",
        "lease_renewed",
        "stale_writer_blocked",
        "offline_commit_absent",
    ),
    "K": (
        "baseline_manifest_valid",
        "stale_generation_rejected",
        "codex_reconnect_turn",
        "controlled_state_restored",
    ),
    "L": (
        "daemon_feature_excludes_fallback",
        "mobile_feature_excludes_fallback",
        "runtime_ignores_local_cli",
        "binaries_exclude_import_strings",
        "experimental_origin_not_ready",
    ),
    "M": (
        "item_list_works",
        "item_view_works",
        "candidate_daemon_started",
        "non_agent_smoke_passed",
        "post_migration_regression_absent",
    ),
}


class ProbeError(RuntimeError):
    def __init__(self, code: str) -> None:
        super().__init__(code)
        self.code = code


def opaque_digest(value: str, domain: str) -> str:
    return hashlib.sha256(f"{domain}\0{value}".encode()).hexdigest()


def latency_bucket(elapsed_ms: int) -> str:
    if elapsed_ms < 250:
        return "under_250ms"
    if elapsed_ms < 1_000:
        return "250ms_to_1s"
    if elapsed_ms < 2_000:
        return "1s_to_2s"
    if elapsed_ms < 5_000:
        return "2s_to_5s"
    if elapsed_ms < 10_000:
        return "5s_to_10s"
    if elapsed_ms < 15_000:
        return "10s_to_15s"
    return "15s_or_more"


def read_private_text(path: Path, error_code: str, max_bytes: int) -> str:
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
        if not path.is_file() or mode & 0o077:
            raise ProbeError(error_code)
        raw = path.read_bytes()
    except OSError as error:
        raise ProbeError(error_code) from error
    if not raw or len(raw) > max_bytes:
        raise ProbeError(error_code)
    try:
        value = raw.decode("utf-8").strip()
    except UnicodeDecodeError as error:
        raise ProbeError(error_code) from error
    if not value:
        raise ProbeError(error_code)
    return value


def apk_marker_state(path: Path) -> tuple[str, dict[str, bool]]:
    if not path.is_file():
        raise ProbeError("apk_unavailable")
    digest = sha256_file(path)
    found = {marker.decode(): False for marker in HOOK_MARKERS}
    try:
        with zipfile.ZipFile(path) as archive:
            libraries = [
                name
                for name in archive.namelist()
                if name.startswith("lib/") and name.endswith("/libisyncyou_mobile.so")
            ]
            if not libraries:
                raise ProbeError("apk_mobile_library_missing")
            for name in libraries:
                info = archive.getinfo(name)
                if info.file_size > 128 * 1024 * 1024:
                    raise ProbeError("apk_mobile_library_too_large")
                payload = archive.read(name)
                for marker in HOOK_MARKERS:
                    found[marker.decode()] = found[marker.decode()] or marker in payload
    except (OSError, zipfile.BadZipFile, KeyError) as error:
        raise ProbeError("apk_invalid") from error
    return digest, found


def inspect_apk_matrix(
    hook_path: Path | None, default_path: Path | None, published_path: Path | None
) -> dict[str, object]:
    if hook_path is None and default_path is None and published_path is None:
        return {"state": "not_run", "code": "apk_paths_not_supplied"}
    if hook_path is None or default_path is None:
        raise ProbeError("hook_and_default_apk_required")
    hook_digest, hook_markers = apk_marker_state(hook_path)
    default_digest, default_markers = apk_marker_state(default_path)
    published_digest = None
    published_markers = None
    if published_path is not None:
        published_digest, published_markers = apk_marker_state(published_path)
    distinct = hook_digest != default_digest and (
        published_digest is None
        or (published_digest != hook_digest and published_digest != default_digest)
    )
    hook_complete = all(hook_markers.values())
    default_clean = not any(default_markers.values())
    published_clean = published_markers is None or not any(published_markers.values())
    passed = distinct and hook_complete and default_clean and published_clean
    return {
        "state": "pass" if passed else "fail",
        "code": "verified" if passed else "artifact_boundary_failed",
        "artifacts_distinct": distinct,
        "hook": {"sha256": hook_digest, "expected_markers_present": hook_complete},
        "default": {"sha256": default_digest, "hook_markers_absent": default_clean},
        "published": None
        if published_digest is None
        else {"sha256": published_digest, "hook_markers_absent": published_clean},
    }


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_commit(value: str, label: str) -> str:
    if not SHA_RE.fullmatch(value):
        raise ProbeError(f"invalid_{label}")
    return value


def validate_git_object(value: str, expected_type: str, label: str) -> str:
    validated = validate_commit(value, label)
    try:
        result = subprocess.run(
            ["git", "cat-file", "-t", validated],
            cwd=Path(__file__).resolve().parents[1],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise ProbeError(f"unknown_{label}") from error
    if result.stdout.strip() != expected_type:
        raise ProbeError(f"invalid_{label}_type")
    return validated


def loopback_endpoint(value: str) -> tuple[str, int, str]:
    parsed = urlparse(value if "://" in value else f"http://{value}")
    if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost", "::1"}:
        raise ProbeError("endpoint_not_loopback")
    if parsed.port is None:
        raise ProbeError("endpoint_port_required")
    return parsed.hostname, parsed.port, f"http://{parsed.hostname}:{parsed.port}"


def require_free_port(host: str, port: int) -> None:
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    with socket.socket(family, socket.SOCK_STREAM) as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            listener.bind((host, port))
        except OSError as error:
            raise ProbeError("listener_already_occupied") from error


def inspect_android_bridge(target: str | None) -> dict[str, object]:
    if target is None:
        return {"state": "not_run", "code": "android_bridge_target_not_supplied"}
    _, _, base = loopback_endpoint(target)
    request = urllib.request.Request(f"{base}/json", headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=CONTROL_REQUEST_TIMEOUT_SECONDS) as response:
            if response.status != 200 or response.headers.get_content_type() != "application/json":
                raise ProbeError("android_bridge_target_unavailable")
            raw = response.read(MAX_CDP_TARGET_BYTES + 1)
    except urllib.error.HTTPError as error:
        error.close()
        raise ProbeError("android_bridge_target_unavailable") from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise ProbeError("android_bridge_target_unavailable") from error
    if len(raw) > MAX_CDP_TARGET_BYTES:
        raise ProbeError("android_bridge_target_too_large")
    try:
        targets = json.loads(raw, object_pairs_hook=strict_object)
    except (json.JSONDecodeError, UnicodeDecodeError, ProbeError) as error:
        raise ProbeError("android_bridge_target_invalid") from error
    if not isinstance(targets, list) or len(targets) > 32:
        raise ProbeError("android_bridge_target_invalid")
    page_ready = any(
        isinstance(item, dict)
        and item.get("type") == "page"
        and isinstance(item.get("webSocketDebuggerUrl"), str)
        and str(item["webSocketDebuggerUrl"]).startswith(("ws://127.0.0.1:", "ws://localhost:"))
        for item in targets
    )
    if not page_ready:
        raise ProbeError("android_bridge_page_unavailable")
    return {"state": "pass", "code": "verified", "page_target_ready": True}


def strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ProbeError("invalid_observations")
        result[key] = value
    return result


def load_observations(
    path: Path | None, implementation_commit: str
) -> tuple[dict[str, dict[str, object]], str | None]:
    if path is None:
        return (
            {
                row: {
                    "state": "not_run",
                    "code": "evidence_not_supplied",
                    "checks": {name: False for name in ROW_REQUIREMENTS[row]},
                }
                for row in REQUIRED_PRE_RC_ROWS
            },
            None,
        )
    try:
        raw = path.read_bytes()
        value = json.loads(raw, object_pairs_hook=strict_object)
    except (OSError, json.JSONDecodeError) as error:
        raise ProbeError("invalid_observations") from error
    if not isinstance(value, dict) or set(value) != {
        "schema_version",
        "implementation_commit",
        "rows",
    }:
        raise ProbeError("invalid_observations")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ProbeError("invalid_observations")
    if value["implementation_commit"] != implementation_commit:
        raise ProbeError("observation_commit_mismatch")
    supplied_rows = value["rows"]
    if not isinstance(supplied_rows, dict) or set(supplied_rows) - set(REQUIRED_PRE_RC_ROWS):
        raise ProbeError("invalid_observations")
    result: dict[str, dict[str, object]] = {}
    for row in REQUIRED_PRE_RC_ROWS:
        item = supplied_rows.get(row)
        if item is None:
            result[row] = {
                "state": "not_run",
                "code": "evidence_not_supplied",
                "checks": {name: False for name in ROW_REQUIREMENTS[row]},
            }
            continue
        if not isinstance(item, dict) or set(item) != {"checks"}:
            raise ProbeError("invalid_observations")
        checks = item["checks"]
        required = ROW_REQUIREMENTS[row]
        if (
            not isinstance(checks, dict)
            or set(checks) != set(required)
            or any(type(checks[name]) is not bool for name in required)
        ):
            raise ProbeError("invalid_observations")
        failed = [name for name in required if not checks[name]]
        result[row] = {
            "state": "pass" if not failed else "fail",
            "code": "verified" if not failed else "required_check_failed",
            "checks": {name: checks[name] for name in required},
        }
    return result, hashlib.sha256(raw).hexdigest()


@dataclass
class ManagedDaemon:
    process: subprocess.Popen[bytes]
    log_path: Path

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)


def start_daemon(binary: Path, config: Path, bind: str, runtime: Path) -> ManagedDaemon:
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ProbeError("daemon_binary_unavailable")
    if not config.is_file():
        raise ProbeError("daemon_config_unavailable")
    log_path = runtime / "daemon.log"
    log_fd = os.open(log_path, os.O_CREAT | os.O_WRONLY | os.O_EXCL, 0o600)
    with os.fdopen(log_fd, "wb") as log:
        process = subprocess.Popen(
            [str(binary), "--config", str(config), "--tcp", "--bind", bind],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    return ManagedDaemon(process, log_path)


def session_cookie_is_strict(jar: http.cookiejar.CookieJar) -> bool:
    for cookie in jar:
        same_site = str(cookie._rest.get("SameSite", "")).lower()  # noqa: SLF001
        if (
            cookie.name == "isy_session"
            and cookie.path == "/api/v1"
            and cookie.has_nonstandard_attr("HttpOnly")
            and same_site == "strict"
            and cookie.expires is None
        ):
            return True
    return False


@dataclass
class RuntimeClient:
    base: str
    jar: http.cookiejar.CookieJar
    opener: urllib.request.OpenerDirector
    agent_cap: str | None = None

    def _request(
        self,
        method: str,
        path: str,
        *,
        cap: bool = False,
        body: bytes | None = None,
        timeout: float = 10.0,
    ) -> urllib.request.Request:
        headers = {"Accept": "application/json"}
        if method == "POST":
            headers["Content-Type"] = "application/json"
            headers["Origin"] = self.base
        if cap:
            if not self.agent_cap:
                raise ProbeError("agent_capability_unavailable")
            headers["X-Capability-Token"] = self.agent_cap
        request = urllib.request.Request(
            f"{self.base}{path}", data=body, headers=headers, method=method
        )
        request.timeout = timeout
        return request

    def json(
        self,
        method: str,
        path: str,
        *,
        cap: bool = False,
        value: object | None = None,
        timeout: float = 10.0,
    ) -> dict[str, object]:
        body = None
        if value is not None:
            body = json.dumps(value, separators=(",", ":")).encode()
        request = self._request(method, path, cap=cap, body=body, timeout=timeout)
        try:
            with self.opener.open(request, timeout=timeout) as response:
                if response.status < 200 or response.status >= 300:
                    raise ProbeError("runtime_request_rejected")
                if response.headers.get_content_type() != "application/json":
                    raise ProbeError("runtime_response_invalid")
                raw = response.read(MAX_JSON_BYTES + 1)
        except urllib.error.HTTPError as error:
            error.close()
            raise ProbeError("runtime_request_rejected") from error
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise ProbeError("runtime_request_failed") from error
        if len(raw) > MAX_JSON_BYTES:
            raise ProbeError("runtime_response_too_large")
        try:
            document = json.loads(raw, object_pairs_hook=strict_object)
        except (json.JSONDecodeError, ProbeError) as error:
            raise ProbeError("runtime_response_invalid") from error
        if not isinstance(document, dict):
            raise ProbeError("runtime_response_invalid")
        return document

    def load_agent_capability(self) -> None:
        try:
            with self.opener.open(f"{self.base}/app.js", timeout=5) as response:
                if response.status != 200:
                    raise ProbeError("agent_capability_unavailable")
                raw = response.read(2 * MAX_JSON_BYTES + 1)
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise ProbeError("agent_capability_unavailable") from error
        if len(raw) > 2 * MAX_JSON_BYTES:
            raise ProbeError("agent_capability_unavailable")
        try:
            source = raw.decode("utf-8")
        except UnicodeDecodeError as error:
            raise ProbeError("agent_capability_unavailable") from error
        matches = AGENT_CAP_RE.findall(source)
        if len(matches) != 1 or matches[0] == "__AGENT_CAP_TOKEN__":
            raise ProbeError("agent_capability_unavailable")
        self.agent_cap = matches[0]


def wait_for_runtime(
    base: str, daemon: ManagedDaemon | None, timeout: float
) -> tuple[RuntimeClient, bool, bool, bool, dict[str, object]]:
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    client = RuntimeClient(base=base, jar=jar, opener=opener)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if daemon is not None and daemon.process.poll() is not None:
            raise ProbeError("daemon_exited_before_ready")
        try:
            with opener.open(f"{base}/", timeout=2) as response:
                shell_ready = response.status == 200
            if shell_ready:
                strict_session_cookie = session_cookie_is_strict(jar)
                with opener.open(f"{base}/api/v1/agent/status", timeout=2) as response:
                    status_ready = response.status == 200
                    content_type = response.headers.get_content_type()
                    body = response.read(256 * 1024)
                if status_ready and content_type == "application/json":
                    try:
                        status_document = json.loads(body)
                    except json.JSONDecodeError as error:
                        raise ProbeError("agent_status_invalid_json") from error
                    if not isinstance(status_document, dict):
                        raise ProbeError("agent_status_invalid_json")
                    return client, True, strict_session_cookie, True, status_document
        except (urllib.error.URLError, TimeoutError, OSError):
            time.sleep(0.1)
    raise ProbeError("daemon_start_timeout")


def select_runtime_account(client: RuntimeClient, account_file: Path | None) -> str:
    document = client.json("GET", "/api/v1/accounts")
    accounts = document.get("accounts")
    if not isinstance(accounts, list):
        raise ProbeError("runtime_account_unavailable")
    available = [
        item.get("id")
        for item in accounts
        if isinstance(item, dict)
        and isinstance(item.get("id"), str)
        and 0 < len(item["id"]) <= 128
    ]
    if account_file is not None:
        selected = read_private_text(account_file, "invalid_account_file", 256)
        if selected not in available:
            raise ProbeError("runtime_account_unavailable")
        return selected
    if len(available) != 1:
        raise ProbeError("runtime_account_ambiguous")
    return available[0]


def select_or_create_session(client: RuntimeClient) -> str:
    page = client.json("GET", "/api/v1/agent/session/list?limit=100", cap=True)
    sessions = page.get("sessions")
    if not isinstance(sessions, list):
        raise ProbeError("runtime_session_unavailable")
    selected = page.get("selected_session_id")
    candidates = [
        item
        for item in sessions
        if isinstance(item, dict)
        and isinstance(item.get("session_id"), str)
        and item.get("archived") is not True
    ]
    session = next((item for item in candidates if item.get("session_id") == selected), None)
    if session is None and candidates:
        session = candidates[0]
    if session is None:
        created = client.json(
            "POST",
            "/api/v1/agent/session/create",
            cap=True,
            value={"request_id": str(uuid.uuid4()), "display_name": "Assistant"},
        )
        session = created.get("session")
    if not isinstance(session, dict):
        raise ProbeError("runtime_session_unavailable")
    session_id = session.get("session_id")
    if not isinstance(session_id, str) or not session_id or len(session_id) > 128:
        raise ProbeError("runtime_session_unavailable")
    return session_id


def stream_turn(client: RuntimeClient, turn_id: str, timeout: float) -> dict[str, object]:
    query = urllib.parse.urlencode({"turn": turn_id})
    request = client._request("GET", f"/api/v1/agent/stream?{query}", timeout=timeout)
    names: list[str] = []
    untrusted_result = False
    terminal_reason = None
    error_code = None
    deadline = time.monotonic() + timeout
    stream_started = time.monotonic()
    first_event_ms: int | None = None
    first_output_ms: int | None = None
    idle_timeout = min(timeout, CONTROL_REQUEST_TIMEOUT_SECONDS)
    total_bytes = 0
    allowed = {
        "progress",
        "token",
        "tool_call",
        "tool_result",
        "search_stage",
        "partial_result",
        "confirmation_required",
        "error",
        "done",
    }
    try:
        with client.opener.open(request, timeout=idle_timeout) as response:
            if response.status != 200 or response.headers.get_content_type() != "text/event-stream":
                raise ProbeError("turn_stream_unavailable")
            while True:
                if time.monotonic() >= deadline:
                    raise ProbeError("turn_stream_timed_out")
                line = response.readline(MAX_SSE_LINE_BYTES + 1)
                if not line:
                    break
                total_bytes += len(line)
                if len(line) > MAX_SSE_LINE_BYTES or total_bytes > MAX_SSE_BYTES:
                    raise ProbeError("turn_stream_too_large")
                if not line.startswith(b"data:"):
                    continue
                if len(names) >= MAX_SSE_EVENTS:
                    raise ProbeError("turn_stream_too_many_events")
                try:
                    event = json.loads(line[5:].strip(), object_pairs_hook=strict_object)
                except (json.JSONDecodeError, UnicodeDecodeError, ProbeError) as error:
                    raise ProbeError("turn_stream_invalid") from error
                if not isinstance(event, dict) or not isinstance(event.get("event"), str):
                    raise ProbeError("turn_stream_invalid")
                name = event["event"]
                if name not in allowed or terminal_reason is not None:
                    raise ProbeError("turn_stream_invalid")
                if first_event_ms is None:
                    first_event_ms = int((time.monotonic() - stream_started) * 1000)
                if first_output_ms is None and name != "progress":
                    first_output_ms = int((time.monotonic() - stream_started) * 1000)
                names.append(name)
                if name == "tool_result" and event.get("untrusted") is True:
                    untrusted_result = True
                if name == "error":
                    candidate = event.get("message")
                    if not isinstance(candidate, str) or not SAFE_CODE_RE.fullmatch(candidate):
                        raise ProbeError("turn_stream_invalid")
                    error_code = candidate
                if name == "done":
                    reason = event.get("reason")
                    if reason not in {"complete", "error", "cancelled", "pending_confirmation"}:
                        raise ProbeError("turn_stream_invalid")
                    terminal_reason = reason
                    # The HTTP adapter emits a separate named SSE `done` frame with an empty
                    # payload after the agent sender closes. The typed agent `done` event above
                    # is authoritative, so stop before that transport-only frame is read as a
                    # second agent event.
                    break
    except urllib.error.HTTPError as error:
        error.close()
        raise ProbeError("turn_stream_unavailable") from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise ProbeError("turn_stream_failed") from error
    if terminal_reason is None or not names or names[-1] != "done" or names.count("done") != 1:
        raise ProbeError("turn_stream_missing_terminal")
    return {
        "ordered_event_names": names,
        "terminal_reason": terminal_reason,
        "error_code": error_code,
        "untrusted_tool_result_observed": untrusted_result,
        "first_event_latency": latency_bucket(first_event_ms or 0),
        "first_output_latency": latency_bucket(first_output_ms or first_event_ms or 0),
    }


def load_session_records(client: RuntimeClient, session_id: str) -> list[dict[str, object]]:
    cursor = None
    records: list[dict[str, object]] = []
    deadline = time.monotonic() + 60
    pages = 0
    while pages < 100:
        query: dict[str, object] = {"session_id": session_id, "limit": 100}
        if cursor is not None:
            query["cursor"] = cursor
        page = client.json(
            "GET",
            "/api/v1/agent/session/history?" + urllib.parse.urlencode(query),
            cap=True,
        )
        if page.get("refreshing") is True:
            if time.monotonic() >= deadline:
                raise ProbeError("turn_history_timed_out")
            time.sleep(0.5)
            continue
        page_records = page.get("records")
        if not isinstance(page_records, list):
            raise ProbeError("turn_history_invalid")
        if not all(isinstance(record, dict) for record in page_records):
            raise ProbeError("turn_history_invalid")
        records.extend(page_records)
        pages += 1
        cursor = page.get("next_cursor")
        if cursor is None:
            break
        if not isinstance(cursor, str) or not cursor or len(cursor) > 512:
            raise ProbeError("turn_history_invalid")
    else:
        raise ProbeError("turn_history_too_many_pages")
    return records


def load_turn_records(
    client: RuntimeClient,
    session_id: str,
    request_id: str,
    turn_id: str,
) -> tuple[bool, bool, bool, list[dict[str, object]]]:
    matching = [
        record
        for record in load_session_records(client, session_id)
        if record.get("request_id") == request_id and record.get("turn_id") == turn_id
    ]
    intent = False
    assistant = False
    terminal = False
    sources: list[dict[str, object]] = []
    for record in matching:
        payload = record.get("kind")
        if not isinstance(payload, dict):
            continue
        kind = payload.get("kind")
        if kind == "turn_intent":
            intent = True
        elif kind == "assistant_result":
            assistant = True
            candidate_sources = payload.get("sources")
            if isinstance(candidate_sources, list):
                sources.extend(item for item in candidate_sources if isinstance(item, dict))
        elif kind == "turn_terminal":
            terminal = True
    return intent, assistant, terminal, sources


def read_request_status(
    client: RuntimeClient, status_query: str
) -> tuple[str, str, bool]:
    try:
        document = client.json(
            "GET", f"/api/v1/agent/request/status?{status_query}", cap=True
        )
    except ProbeError:
        return "unavailable", "request_status_unavailable", False
    state = document.get("state")
    code = document.get("code")
    terminal = document.get("terminal")
    if (
        not isinstance(state, str)
        or not SAFE_CODE_RE.fullmatch(state)
        or not isinstance(code, str)
        or not SAFE_CODE_RE.fullmatch(code)
        or type(terminal) is not bool
    ):
        raise ProbeError("retrieval_request_status_invalid")
    return state, code, terminal


def source_is_listed(client: RuntimeClient, account: str, service: str, item_id: str) -> bool:
    for offset in range(0, 10_000, 1_000):
        query = urllib.parse.urlencode(
            {"account": account, "service": service, "limit": 1000, "offset": offset}
        )
        page = client.json("GET", f"/api/v1/items?{query}")
        items = page.get("items")
        if not isinstance(items, list):
            raise ProbeError("source_list_invalid")
        if any(
            isinstance(item, dict)
            and item.get("service") == service
            and item.get("remote_id") == item_id
            for item in items
        ):
            return True
        total = page.get("total")
        if not isinstance(total, int) or total <= offset + len(items):
            return False
    raise ProbeError("source_list_too_large")


def source_view_resolves(client: RuntimeClient, account: str, service: str, item_id: str) -> bool:
    query = urllib.parse.urlencode({"account": account, "service": service, "id": item_id})
    request = client._request("GET", f"/api/v1/view?{query}", timeout=10)
    try:
        with client.opener.open(request, timeout=10) as response:
            return response.status == 200 and response.headers.get_content_type() == "text/html"
    except urllib.error.HTTPError as error:
        error.close()
        return False
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise ProbeError("source_view_failed") from error


def run_retrieval_turn(
    client: RuntimeClient,
    status_document: dict[str, object],
    prompt_file: Path,
    account_file: Path | None,
    timeout: float,
) -> dict[str, object]:
    provider = status_document.get("selected_provider")
    if provider not in {"claude", "codex"} or status_document.get("connected") is not True:
        raise ProbeError("provider_not_ready")
    client.load_agent_capability()
    account = select_runtime_account(client, account_file)
    session_id = select_or_create_session(client)
    # Model the real Assistant screen: opening it hydrates the selected session before
    # the user submits a prompt. The measured turn latency therefore exercises the hot
    # session path rather than charging an unrelated initial history load to the click.
    load_session_records(client, session_id)
    fixture_name = f"isy628-{uuid.uuid4().hex[:12]}"
    prompt_template = read_private_text(prompt_file, "invalid_prompt_file", 32 * 1024)
    if "{fixture}" not in prompt_template:
        raise ProbeError("prompt_fixture_placeholder_missing")
    prompt = prompt_template.replace("{fixture}", fixture_name)
    if not prompt or len(prompt.encode()) > 32 * 1024:
        raise ProbeError("invalid_prompt_file")
    request_id = str(uuid.uuid4())
    turn_body = {
        "request_id": request_id,
        "session_id": session_id,
        "account": account,
        "prompt": prompt,
    }
    started = time.monotonic()
    control_timeout = min(timeout, CONTROL_REQUEST_TIMEOUT_SECONDS)
    first = client.json(
        "POST", "/api/v1/agent/turn", cap=True, value=turn_body, timeout=control_timeout
    )
    elapsed_ms = int((time.monotonic() - started) * 1000)
    retry = client.json(
        "POST", "/api/v1/agent/turn", cap=True, value=turn_body, timeout=control_timeout
    )
    turn_id = first.get("turn")
    if not isinstance(turn_id, str) or not turn_id or len(turn_id) > 128:
        raise ProbeError("turn_id_invalid")
    retry_same_turn = retry.get("turn") == turn_id
    if not retry_same_turn:
        raise ProbeError("turn_retry_duplicated")
    status_query = urllib.parse.urlencode(
        {"session_id": session_id, "route": "agent_turn", "request_id": request_id}
    )
    try:
        stream = stream_turn(client, turn_id, timeout)
    except ProbeError as error:
        if error.code != "turn_stream_timed_out":
            raise
        status_state, status_code, status_terminal = read_request_status(client, status_query)
        return {
            "state": "fail",
            "provider": provider,
            "request_digest": opaque_digest(request_id, "issue-628-request"),
            "turn_digest": opaque_digest(turn_id, "issue-628-turn"),
            "fixture_digest": opaque_digest(fixture_name, "issue-628-fixture"),
            "turn_ack_latency": latency_bucket(elapsed_ms),
            "retry_reused_turn": retry_same_turn,
            "ordered_event_names": [],
            "terminal_reason": "timeout",
            "error_code": error.code,
            "untrusted_tool_result_observed": False,
            "request_status_state": status_state,
            "request_status_code": status_code,
            "request_status_terminal": status_terminal,
            "transcript_rehydrated": False,
            "source_count": 0,
            "all_sources_listed_and_viewable": False,
        }
    if stream["terminal_reason"] != "complete":
        status_state, status_code, status_terminal = read_request_status(client, status_query)
        return {
            "state": "fail",
            "provider": provider,
            "request_digest": opaque_digest(request_id, "issue-628-request"),
            "turn_digest": opaque_digest(turn_id, "issue-628-turn"),
            "fixture_digest": opaque_digest(fixture_name, "issue-628-fixture"),
            "turn_ack_latency": latency_bucket(elapsed_ms),
            "retry_reused_turn": retry_same_turn,
            "ordered_event_names": stream["ordered_event_names"],
            "first_event_latency": stream["first_event_latency"],
            "first_output_latency": stream["first_output_latency"],
            "terminal_reason": stream["terminal_reason"],
            "error_code": stream["error_code"],
            "untrusted_tool_result_observed": stream["untrusted_tool_result_observed"],
            "request_status_state": status_state,
            "request_status_code": status_code,
            "request_status_terminal": status_terminal,
            "transcript_rehydrated": False,
            "source_count": 0,
            "all_sources_listed_and_viewable": False,
        }
    try:
        intent, assistant, terminal, sources = load_turn_records(
            client, session_id, request_id, turn_id
        )
    except ProbeError as error:
        raise ProbeError("retrieval_history_failed") from error
    status_state, status_code, status_terminal = read_request_status(client, status_query)
    source_results = []
    for source in sources[:64]:
        service = source.get("service")
        item_id = source.get("item_id")
        if (
            not isinstance(service, str)
            or not service
            or len(service) > 64
            or not isinstance(item_id, str)
            or not item_id
            or len(item_id) > 512
        ):
            raise ProbeError("source_reference_invalid")
        try:
            listed = source_is_listed(client, account, service, item_id)
        except ProbeError as error:
            raise ProbeError("retrieval_source_list_failed") from error
        try:
            viewable = source_view_resolves(client, account, service, item_id)
        except ProbeError as error:
            raise ProbeError("retrieval_source_view_failed") from error
        source_results.append(listed and viewable)
    sources_resolved = bool(source_results) and all(source_results)
    transcript_rehydrated = intent and assistant and terminal
    return {
        "state": "pass"
        if stream["terminal_reason"] == "complete"
        and status_state == "committed"
        and status_terminal is True
        and retry_same_turn
        and transcript_rehydrated
        and sources_resolved
        else "fail",
        "provider": provider,
        "request_digest": opaque_digest(request_id, "issue-628-request"),
        "turn_digest": opaque_digest(turn_id, "issue-628-turn"),
        "fixture_digest": opaque_digest(fixture_name, "issue-628-fixture"),
        "turn_ack_latency": latency_bucket(elapsed_ms),
        "retry_reused_turn": retry_same_turn,
        "ordered_event_names": stream["ordered_event_names"],
        "first_event_latency": stream["first_event_latency"],
        "first_output_latency": stream["first_output_latency"],
        "terminal_reason": stream["terminal_reason"],
        "untrusted_tool_result_observed": stream["untrusted_tool_result_observed"],
        "request_status_state": status_state,
        "request_status_code": status_code,
        "request_status_terminal": status_terminal,
        "transcript_rehydrated": transcript_rehydrated,
        "source_count": len(sources),
        "all_sources_listed_and_viewable": sources_resolved,
    }


def private_runtime_root(requested: Path | None) -> tuple[Path, bool]:
    if requested is None:
        return Path(tempfile.mkdtemp(prefix="isyncyou-628-runtime-")), True
    requested.mkdir(mode=0o700, parents=True, exist_ok=True)
    requested.chmod(0o700)
    if stat.S_IMODE(requested.stat().st_mode) != 0o700:
        raise ProbeError("runtime_root_not_private")
    if any(requested.iterdir()):
        raise ProbeError("runtime_root_not_empty")
    return requested, False


def bind_retrieval_facts(
    rows: dict[str, dict[str, object]], retrieval: dict[str, object]
) -> None:
    provider_row = "A" if retrieval.get("provider") == "claude" else "B"
    authoritative = {
        provider_row: {
            "source_resolved": retrieval.get("all_sources_listed_and_viewable") is True,
            "transcript_rehydrated": retrieval.get("transcript_rehydrated") is True,
        },
        "J": {"idempotent_retry": retrieval.get("retry_reused_turn") is True},
        "M": {
            "item_list_works": retrieval.get("all_sources_listed_and_viewable") is True,
            "item_view_works": retrieval.get("all_sources_listed_and_viewable") is True,
        },
    }
    for row, updates in authoritative.items():
        checks = rows[row]["checks"]
        if not isinstance(checks, dict):
            raise ProbeError("invalid_observations")
        checks.update(updates)
        failed = [name for name in ROW_REQUIREMENTS[row] if checks.get(name) is not True]
        rows[row]["state"] = "pass" if not failed else "fail"
        rows[row]["code"] = "verified" if not failed else "required_check_failed"


def run(args: argparse.Namespace) -> tuple[dict[str, object], int]:
    implementation = validate_git_object(
        args.implementation_commit, "commit", "implementation_commit"
    )
    candidate_tree = None
    rc_commit = None
    if args.mode == "final":
        candidate_tree = validate_git_object(args.candidate_tree, "tree", "candidate_tree")
        rc_commit = validate_git_object(args.rc_commit, "commit", "rc_commit")
    host, port, base = loopback_endpoint(args.endpoint or args.bind)
    runtime, remove_runtime = private_runtime_root(Path(args.runtime_root) if args.runtime_root else None)
    daemon: ManagedDaemon | None = None
    binary_digest = None
    try:
        if args.daemon_bin:
            require_free_port(host, port)
            binary = Path(args.daemon_bin).resolve()
            binary_digest = sha256_file(binary)
            daemon = start_daemon(binary, Path(args.config).resolve(), f"{host}:{port}", runtime)
        callback_port_ready = None
        if args.codex_oauth_preflight:
            require_free_port("127.0.0.1", 1455)
            callback_port_ready = True
        client, shell_ready, session_cookie, agent_status_ready, status_document = wait_for_runtime(
            base, daemon, args.startup_timeout
        )
        rows, observation_digest = load_observations(
            Path(args.observations) if args.observations else None,
            implementation,
        )
        retrieval: dict[str, object] = {
            "state": "not_run",
            "code": "retrieval_prompt_not_supplied",
        }
        retrieval_prompt_file = getattr(args, "retrieval_prompt_file", None)
        if retrieval_prompt_file:
            retrieval = run_retrieval_turn(
                client,
                status_document,
                Path(retrieval_prompt_file),
                Path(args.account_id_file) if getattr(args, "account_id_file", None) else None,
                getattr(args, "turn_timeout", 1200.0),
            )
            retrieval["code"] = (
                "verified" if retrieval.get("state") == "pass" else "required_check_failed"
            )
            bind_retrieval_facts(rows, retrieval)
        hook_apk = getattr(args, "hook_apk", None)
        default_apk = getattr(args, "default_apk", None)
        published_apk = getattr(args, "published_apk", None)
        apk_matrix = inspect_apk_matrix(
            Path(hook_apk) if hook_apk else None,
            Path(default_apk) if default_apk else None,
            Path(published_apk) if published_apk else None,
        )
        android_bridge = inspect_android_bridge(getattr(args, "android_bridge_target", None))
        required_pass = (
            shell_ready
            and agent_status_ready
            and session_cookie
            and (not args.codex_oauth_preflight or callback_port_ready is True)
            and (not retrieval_prompt_file or retrieval["state"] == "pass")
            and (
                not any((hook_apk, default_apk, published_apk))
                or apk_matrix["state"] == "pass"
            )
            and android_bridge["state"] in {"not_run", "pass"}
            and all(item["state"] == "pass" for item in rows.values())
        )
        report: dict[str, object] = {
            "schema_version": 1,
            "mode": args.mode,
            "implementation_commit": implementation,
            "candidate_tree": candidate_tree,
            "rc_commit": rc_commit,
            "managed_daemon": daemon is not None,
            "daemon_binary_sha256": binary_digest,
            "shell_ready": shell_ready,
            "agent_status_ready": agent_status_ready,
            "strict_session_cookie_observed": session_cookie,
            "codex_callback_port_free_before_oauth": callback_port_ready,
            "observation_document_sha256": observation_digest,
            "retrieval_turn": retrieval,
            "android_bridge": android_bridge,
            "apk_matrix": apk_matrix,
            "rows": rows,
            "required_rows_pass": required_pass,
            "cleanup": {"child_stopped": True, "raw_logs_deleted": True},
            "redaction": {
                "tokens_included": False,
                "oauth_values_included": False,
                "account_identity_included": False,
                "raw_logs_included": False,
                "tool_results_included": False,
            },
        }
        return report, 0 if required_pass else 2
    finally:
        if daemon is not None:
            daemon.stop()
        for child in runtime.iterdir() if runtime.exists() else ():
            if child.is_file() or child.is_symlink():
                child.unlink(missing_ok=True)
            elif child.is_dir():
                shutil.rmtree(child)
        if remove_runtime:
            runtime.rmdir()


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("pre-rc", "final"))
    parser.add_argument("--implementation-commit", required=True)
    parser.add_argument("--candidate-tree")
    parser.add_argument("--rc-commit")
    endpoint = parser.add_mutually_exclusive_group()
    endpoint.add_argument("--endpoint")
    endpoint.add_argument("--daemon-bin")
    parser.add_argument("--config")
    parser.add_argument("--bind", default="127.0.0.1:8871")
    parser.add_argument("--runtime-root")
    parser.add_argument("--observations")
    parser.add_argument(
        "--android-bridge-target",
        help="loopback WebView CDP target forwarded through ADB, for example 127.0.0.1:9222",
    )
    parser.add_argument(
        "--retrieval-prompt-file",
        help="owner-only UTF-8 prompt template containing the literal {fixture}",
    )
    parser.add_argument(
        "--account-id-file",
        help="owner-only account alias file, required only when the runtime has multiple accounts",
    )
    parser.add_argument("--hook-apk")
    parser.add_argument("--default-apk")
    parser.add_argument("--published-apk")
    parser.add_argument(
        "--codex-oauth-preflight",
        action="store_true",
        help="fail unless the fixed loopback callback port is free before a Codex OAuth row",
    )
    parser.add_argument("--startup-timeout", type=float, default=20.0)
    parser.add_argument("--turn-timeout", type=float, default=1200.0)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    if args.daemon_bin and not args.config:
        parser.error("--config is required with --daemon-bin")
    if not args.daemon_bin and not args.endpoint:
        parser.error("either --endpoint or --daemon-bin is required")
    if args.mode == "final" and (not args.candidate_tree or not args.rc_commit):
        parser.error("final mode requires --candidate-tree and --rc-commit")
    if args.account_id_file and not args.retrieval_prompt_file:
        parser.error("--account-id-file requires --retrieval-prompt-file")
    if bool(args.hook_apk) != bool(args.default_apk):
        parser.error("--hook-apk and --default-apk must be supplied together")
    if args.published_apk and args.mode != "final":
        parser.error("--published-apk is valid only in final mode")
    if args.turn_timeout <= 0 or args.turn_timeout > 1200:
        parser.error("--turn-timeout must be in (0, 1200]")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        report, status = run(args)
    except ProbeError as error:
        report = {
            "schema_version": 1,
            "mode": args.mode,
            "status": "fail",
            "code": error.code,
            "redaction": {"raw_logs_included": False, "tokens_included": False},
        }
        status = 1
    output = Path(args.out)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
