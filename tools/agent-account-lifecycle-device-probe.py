#!/usr/bin/env python3
"""Reduce #645 APK/device lifecycle evidence to a closed, secret-free report."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import subprocess
import zipfile
from pathlib import Path
from typing import Protocol


PACKAGE = "com.silentspike.isyncyou.debug"
HOOK_MARKER = b"ISY_AGENT_ACCOUNT_LIFECYCLE_DEVICE_HOOK_V1"
MAX_OBSERVATION_BYTES = 64 * 1024
PROVIDERS = {"claude", "codex"}
OPERATIONS = {"connect", "disconnect", "reconnect", "switch", "candidate_cleanup"}
RESULTS = {"pass", "fail", "blocked", "not_run_policy"}
STATES = {
    "connected", "reconnect_required", "revoke_unknown", "cleanup_pending",
    "candidate_cleanup", "exchange_outcome_unknown", "awaiting_oauth_login",
    "disconnected", "busy", "unavailable",
}
HOOKS = {
    "hold_after_revoke_before_cleanup", "crash_after_revoke_confirmed",
    "force_revoke_timeout", "force_candidate_validation_failure",
}
OBSERVATION_FIELDS = {
    "provider", "operation", "result", "initial_state", "final_state",
    "server_revoke_2xx", "old_generation_cleaned", "new_generation_ready",
    "post_turn_completed", "same_account_rejected",
    "oauth_guard_ended_before_revoke_guard", "credential_revoke_guard_observed",
    "candidate_retained_when_outcome_unknown", "hook_checkpoint",
}
BOOLEAN_FIELDS = OBSERVATION_FIELDS - {
    "provider", "operation", "result", "initial_state", "final_state", "hook_checkpoint",
}


class Runner(Protocol):
    def run(self, *args: str, timeout: int = 20) -> subprocess.CompletedProcess[str]: ...


class AdbRunner:
    def __init__(self, adb: str) -> None:
        self.adb = adb

    def run(self, *args: str, timeout: int = 20) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.adb, *args], text=True, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, timeout=timeout, check=False,
        )


def apk_marker(apk: Path) -> bool:
    with zipfile.ZipFile(apk) as archive:
        native = archive.read("lib/arm64-v8a/libisyncyou_mobile.so")
    return HOOK_MARKER in native


def _load_observation(path: Path) -> dict[str, object]:
    if path.parent != Path("/tmp") and Path("/tmp") not in path.parents:
        raise ValueError("observation must be below /tmp")
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ValueError("observation must be a regular non-symlink file")
    if metadata.st_uid != os.getuid() or metadata.st_size > MAX_OBSERVATION_BYTES:
        raise ValueError("observation ownership or size is invalid")
    value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_unique_object)
    if not isinstance(value, dict) or set(value) - OBSERVATION_FIELDS:
        raise ValueError("observation contains unsupported fields")
    if value.get("provider") not in PROVIDERS:
        raise ValueError("observation provider is invalid")
    if value.get("operation") not in OPERATIONS or value.get("result") not in RESULTS:
        raise ValueError("observation operation or result is invalid")
    if value.get("initial_state") not in STATES or value.get("final_state") not in STATES:
        raise ValueError("observation state is invalid")
    hook = value.get("hook_checkpoint")
    if hook is not None and hook not in HOOKS:
        raise ValueError("observation hook is invalid")
    for field in BOOLEAN_FIELDS:
        if field in value and value[field] is not None and not isinstance(value[field], bool):
            raise ValueError(f"observation {field} must be boolean or null")
    return value


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError("observation contains duplicate fields")
        value[key] = item
    return value


def collect(
    runner: Runner,
    apk: Path,
    scope: str,
    observation: Path | None = None,
) -> dict[str, object]:
    connected = runner.run("get-state").stdout.strip() == "device"
    installed = False
    running = False
    revoke_guard_running = False
    if connected:
        installed = runner.run("shell", "pm", "path", PACKAGE).returncode == 0
        running = bool(runner.run("shell", "pidof", PACKAGE).stdout.strip())
        services = runner.run("shell", "dumpsys", "activity", "services", PACKAGE).stdout
        revoke_guard_running = (
            "NetworkCriticalGuardService" in services and "credential_revoke" in services
        )
    marker = apk_marker(apk)
    report: dict[str, object] = {
        "schema_version": 1,
        "scope": scope,
        "device_connected": connected,
        "package_installed": installed,
        "app_process_running": running,
        "credential_revoke_guard_running": revoke_guard_running,
        "apk_sha256": hashlib.sha256(apk.read_bytes()).hexdigest(),
        "hook_marker_present": marker,
        "marker_matches_scope": marker == (scope == "hook"),
        "observation": _load_observation(observation) if observation else None,
        "redaction": {
            "serial_included": False,
            "account_identity_included": False,
            "token_or_callback_included": False,
            "raw_platform_output_included": False,
            "provider_response_included": False,
        },
    }
    return report


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--adb", default="adb")
    parser.add_argument("--apk", required=True)
    parser.add_argument("--scope", choices=("default", "hook"), required=True)
    parser.add_argument("--observation")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    apk = Path(args.apk).resolve()
    if not apk.is_file():
        parser.error("APK does not exist")
    observation = Path(args.observation).absolute() if args.observation else None
    report = collect(AdbRunner(args.adb), apk, args.scope, observation)
    output = Path(args.output)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if report["device_connected"] and report["marker_matches_scope"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
