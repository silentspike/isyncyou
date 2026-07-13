#!/usr/bin/env python3
"""Produce a redacted #640 device/APK state report.

No serial, IP, logs, callback target, account data, or raw dumpsys output is
returned. The probe records only closed booleans, scope, an APK hash, and marker
presence. Raw subprocess output remains in memory and is discarded.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import zipfile
from pathlib import Path
from typing import Protocol


PACKAGE = "com.silentspike.isyncyou.debug"
HOOK_MARKER = b"ISY_AGENT_NETWORK_DEVICE_HOOK_V1"


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


def collect(runner: Runner, apk: Path, scope: str) -> dict[str, object]:
    connected = runner.run("get-state").stdout.strip() == "device"
    installed = False
    running = False
    guard_running = False
    if connected:
        installed = runner.run("shell", "pm", "path", PACKAGE).returncode == 0
        running = bool(runner.run("shell", "pidof", PACKAGE).stdout.strip())
        service = runner.run(
            "shell", "dumpsys", "activity", "services", PACKAGE,
        ).stdout
        guard_running = "NetworkCriticalGuardService" in service
    digest = hashlib.sha256(apk.read_bytes()).hexdigest()
    marker = apk_marker(apk)
    expected = scope == "hook"
    return {
        "schema_version": 1,
        "scope": scope,
        "device_connected": connected,
        "package_installed": installed,
        "app_process_running": running,
        "network_guard_service_running": guard_running,
        "apk_sha256": digest,
        "hook_marker_present": marker,
        "marker_matches_scope": marker == expected,
        "redaction": {
            "serial_included": False,
            "ip_included": False,
            "raw_platform_output_included": False,
            "callback_or_account_data_included": False,
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--adb", default="adb")
    parser.add_argument("--apk", required=True)
    parser.add_argument("--scope", choices=("default", "hook"), required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    apk = Path(args.apk).resolve()
    if not apk.is_file():
        parser.error("APK does not exist")
    report = collect(AdbRunner(args.adb), apk, args.scope)
    Path(args.output).write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if report["device_connected"] and report["marker_matches_scope"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
