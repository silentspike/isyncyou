#!/usr/bin/env python3
"""Verify the production boundary for issue #640.

The scan intentionally reads tracked production sources instead of test output. It
checks that the diagnostic hook is default-off, release workflows cannot enable it,
the removed callback-routing workarounds stay absent, and the WebUI keeps the closed
Retry/Settings contract. Optional APK arguments add binary corroboration.
"""

from __future__ import annotations

import argparse
import json
import re
import zipfile
from dataclasses import dataclass, asdict
from pathlib import Path


HOOK_FEATURE = "agent-network-device-test-hooks"
HOOK_MARKER = b"ISY_AGENT_NETWORK_DEVICE_HOOK_V1"


@dataclass
class Check:
    name: str
    passed: bool
    detail: str


def read(root: Path, relative: str) -> str:
    return (root / relative).read_text(encoding="utf-8")


def apk_has_marker(path: Path) -> bool:
    with zipfile.ZipFile(path) as apk:
        native = apk.read("lib/arm64-v8a/libisyncyou_mobile.so")
    return HOOK_MARKER in native


def scan(root: Path, default_apk: Path | None = None, hook_apk: Path | None = None) -> list[Check]:
    mobile_toml = read(root, "crates/mobile/Cargo.toml")
    app_host_toml = read(root, "crates/app-host/Cargo.toml")
    gradle = read(root, "android/app/build.gradle.kts")
    app_js = read(root, "gui/webui/src/app.js")
    app_host = read(root, "crates/app-host/src/lib.rs")
    workflows = "\n".join(
        p.read_text(encoding="utf-8") for p in sorted((root / ".github/workflows").glob("*.yml"))
    )

    mobile_default = re.search(r"default\s*=\s*\[([^]]*)\]", mobile_toml, re.S)
    host_default = re.search(r"default\s*=\s*\[([^]]*)\]", app_host_toml, re.S)
    # app-host has small cfg(test) helpers near the top, so only the final test
    # module boundary is safe to remove from this production-source scan.
    final_test_module = app_host.rfind("#[cfg(test)]\nmod tests")
    app_host_product = app_host[:final_test_module] if final_test_module >= 0 else app_host
    callback_writes = [
        line.strip()
        for line in app_host_product.splitlines()
        if "CODEX_CALLBACK_DIAGNOSTICS_FILE" in line
        and "const CODEX_CALLBACK_DIAGNOSTICS_FILE" not in line
        and "remove_file" not in line
    ]
    routing_residue = re.findall(
        r"(?im)^.*(?:local_address\s*\(|dns-over-https|\bdoh\b|fixed[_ -]?ip).*$",
        app_host_product,
    )

    checks = [
        Check(
            "mobile_default_excludes_hook",
            bool(mobile_default) and HOOK_FEATURE not in mobile_default.group(1),
            "mobile default feature list",
        ),
        Check(
            "app_host_default_excludes_hook",
            bool(host_default) and HOOK_FEATURE not in host_default.group(1),
            "app-host default feature list",
        ),
        Check(
            "android_allowlist_bounds_hook",
            HOOK_FEATURE in gradle and "allowedCargoTestFeatures" in gradle,
            "Gradle accepts the hook only through its test-feature allowlist",
        ),
        Check(
            "release_workflows_exclude_hook",
            HOOK_FEATURE not in workflows and "--all-features" not in workflows,
            "release workflows do not activate the hook or all features",
        ),
        Check(
            "callback_debug_file_is_cleanup_only",
            not callback_writes,
            "legacy filename has no production use except remove_file",
        ),
        Check(
            "callback_transport_has_no_fixed_routing",
            not routing_residue,
            "no fixed-IP, DoH, or local_address routing residue",
        ),
        Check(
            "assistant_has_closed_settings_action",
            'nativeCall("openNetworkSettings", { hint }' in app_js
            and 'data-agent-connectivity-settings' in app_js
            and 'const allowed = ["internet_panel", "background_data", "app_details", "battery_settings"]' in app_js,
            "WebUI forwards only a closed settings hint",
        ),
        Check(
            "assistant_has_retry_diagnostic",
            'data-agent-connectivity-retry' in app_js and "CONNECTIVITY_COPY" in app_js,
            "WebUI renders code-specific Retry diagnostics",
        ),
    ]
    if default_apk is not None:
        checks.append(Check(
            "default_apk_excludes_hook_marker",
            default_apk.is_file() and not apk_has_marker(default_apk),
            "default APK native library marker is absent",
        ))
    if hook_apk is not None:
        checks.append(Check(
            "hook_apk_contains_hook_marker",
            hook_apk.is_file() and apk_has_marker(hook_apk),
            "hook APK native library marker is present",
        ))
    return checks


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=".")
    parser.add_argument("--default-apk")
    parser.add_argument("--hook-apk")
    parser.add_argument("--output")
    args = parser.parse_args()
    root = Path(args.root).resolve()
    checks = scan(
        root,
        Path(args.default_apk).resolve() if args.default_apk else None,
        Path(args.hook_apk).resolve() if args.hook_apk else None,
    )
    report = {"schema_version": 1, "ok": all(c.passed for c in checks), "checks": [asdict(c) for c in checks]}
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
