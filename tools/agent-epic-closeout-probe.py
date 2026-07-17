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
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import urlparse


SHA_RE = re.compile(r"^[0-9a-f]{40}$")
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


def wait_for_runtime(base: str, daemon: ManagedDaemon | None, timeout: float) -> tuple[bool, bool, bool]:
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
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
                    return True, strict_session_cookie, True
        except (urllib.error.URLError, TimeoutError, OSError):
            time.sleep(0.1)
    raise ProbeError("daemon_start_timeout")


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


def run(args: argparse.Namespace) -> tuple[dict[str, object], int]:
    implementation = validate_commit(args.implementation_commit, "implementation_commit")
    candidate_tree = None
    rc_commit = None
    if args.mode == "final":
        candidate_tree = validate_commit(args.candidate_tree, "candidate_tree")
        rc_commit = validate_commit(args.rc_commit, "rc_commit")
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
        shell_ready, session_cookie, agent_status_ready = wait_for_runtime(
            base, daemon, args.startup_timeout
        )
        rows, observation_digest = load_observations(
            Path(args.observations) if args.observations else None,
            implementation,
        )
        required_pass = (
            shell_ready
            and agent_status_ready
            and session_cookie
            and (not args.codex_oauth_preflight or callback_port_ready is True)
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
        "--codex-oauth-preflight",
        action="store_true",
        help="fail unless the fixed loopback callback port is free before a Codex OAuth row",
    )
    parser.add_argument("--startup-timeout", type=float, default=20.0)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    if args.daemon_bin and not args.config:
        parser.error("--config is required with --daemon-bin")
    if not args.daemon_bin and not args.endpoint:
        parser.error("either --endpoint or --daemon-bin is required")
    if args.mode == "final" and (not args.candidate_tree or not args.rc_commit):
        parser.error("final mode requires --candidate-tree and --rc-commit")
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
