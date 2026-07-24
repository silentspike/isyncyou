#!/usr/bin/env python3
"""Verify issue #645 product, cleanup, and test-hook boundaries."""

from __future__ import annotations

import argparse
import json
import re
import zipfile
from dataclasses import asdict, dataclass
from pathlib import Path


HOOK_FEATURE = "agent-account-lifecycle-device-test-hooks"
HOOK_MARKER = b"ISY_AGENT_ACCOUNT_LIFECYCLE_DEVICE_HOOK_V1"


@dataclass
class Check:
    name: str
    passed: bool
    detail: str


def read(root: Path, relative: str) -> str:
    return (root / relative).read_text(encoding="utf-8")


def final_production_prefix(source: str) -> str:
    boundary = source.rfind("#[cfg(test)]\nmod tests")
    return source[:boundary] if boundary >= 0 else source


def function_body(source: str, signature: str) -> str:
    start = source.find(signature)
    if start < 0:
        return ""
    brace = source.find("{", start)
    if brace < 0:
        return ""
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[brace + 1:index]
    return ""


def apk_has_marker(path: Path) -> bool:
    with zipfile.ZipFile(path) as apk:
        native = apk.read("lib/arm64-v8a/libisyncyou_mobile.so")
    return HOOK_MARKER in native


def scan(root: Path, default_apk: Path | None = None, hook_apk: Path | None = None) -> list[Check]:
    app_host_toml = read(root, "crates/app-host/Cargo.toml")
    mobile_toml = read(root, "crates/mobile/Cargo.toml")
    gradle = read(root, "android/app/build.gradle.kts")
    app_host = final_production_prefix(read(root, "crates/app-host/src/lib.rs"))
    webui = final_production_prefix(read(root, "gui/webui/src/lib.rs"))
    app_js = read(root, "gui/webui/src/app.js")
    readme = read(root, "README.md")
    workflows = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted((root / ".github/workflows").glob("*.yml"))
    )
    host_default = re.search(r"default\s*=\s*\[([^]]*)\]", app_host_toml, re.S)
    mobile_default = re.search(r"default\s*=\s*\[([^]]*)\]", mobile_toml, re.S)
    status_body = function_body(app_host, "fn status_json(&self) -> String")
    lifecycle_cleanup = function_body(app_host, "fn delete_provider_product_state_durable(")

    forbidden_status_calls = (
        "refresh_product_credential",
        "revoke_",
        "recover_",
        "reap_",
        "maintenance_",
        ".delete(",
        ".put(",
        "post_json",
    )
    forbidden_cleanup_terms = ("GraphToken", "M365", "m365", "MicrosoftGraph")
    raw_provider_fields = (
        ".raw_error",
        "[\"raw_error\"]",
        ".provider_response",
        "[\"provider_response\"]",
        ".response_body",
        "[\"response_body\"]",
    )

    checks = [
        Check(
            "product_router_has_no_local_only_delete_route",
            "/api/v1/agent/provider/key/delete" not in webui
            and "/api/v1/agent/credential/delete" not in webui,
            "product router exposes lifecycle logout, not local credential deletion",
        ),
        Check(
            "product_lifecycle_has_no_endpoint_override",
            "revoke_endpoint_override" not in app_host
            and "revoke_url_override" not in app_host
            and "ISYNCYOU_REVOKE" not in app_host,
            "provider revoke endpoints remain compiled provider policy",
        ),
        Check(
            "assistant_does_not_render_raw_provider_response",
            not any(field in app_js for field in raw_provider_fields),
            "Assistant lifecycle UI consumes closed codes only",
        ),
        Check(
            "status_is_observational",
            bool(status_body) and not any(term in status_body for term in forbidden_status_calls),
            "status performs no network, recovery, reaping, cleanup, or persistence mutation",
        ),
        Check(
            "lifecycle_does_not_use_local_cli_credentials",
            "local_cli_fallback" not in "\n".join(
                line for line in app_host.splitlines() if "account_lifecycle" in line.lower()
            ),
            "product lifecycle does not consult experimental local CLI auth",
        ),
        Check(
            "cleanup_excludes_graph_and_m365_credentials",
            bool(lifecycle_cleanup)
            and not any(term in lifecycle_cleanup for term in forbidden_cleanup_terms),
            "provider cleanup is bounded to AI provider state",
        ),
        Check(
            "app_host_default_excludes_hook",
            bool(host_default) and HOOK_FEATURE not in host_default.group(1),
            "app-host defaults exclude the #645 hook",
        ),
        Check(
            "mobile_default_excludes_hook",
            bool(mobile_default) and HOOK_FEATURE not in mobile_default.group(1),
            "mobile defaults exclude the #645 hook",
        ),
        Check(
            "android_hook_is_allowlisted_test_only",
            HOOK_FEATURE in gradle and "allowedCargoTestFeatures" in gradle,
            "Gradle forwards only explicitly allowlisted test features",
        ),
        Check(
            "release_workflows_exclude_hook",
            HOOK_FEATURE not in workflows and "--all-features" not in workflows,
            "release workflows do not enable the hook or all features",
        ),
        Check(
            "readme_has_no_manual_provider_token_cleanup",
            not re.search(
                r"(?is)(?:export\s+(?:ANTHROPIC|OPENAI|ISY).*TOKEN|delete\s+.*(?:claude|codex).*token)",
                readme,
            ),
            "README does not teach token export or manual provider-token deletion",
        ),
    ]
    if default_apk is not None:
        checks.append(Check(
            "default_apk_excludes_hook_marker",
            default_apk.is_file() and not apk_has_marker(default_apk),
            "default APK excludes the deliberate #645 marker",
        ))
    if hook_apk is not None:
        checks.append(Check(
            "hook_apk_contains_hook_marker",
            hook_apk.is_file() and apk_has_marker(hook_apk),
            "hook APK contains the deliberate #645 marker",
        ))
    return checks


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=".")
    parser.add_argument("--default-apk")
    parser.add_argument("--hook-apk")
    parser.add_argument("--output")
    args = parser.parse_args()
    checks = scan(
        Path(args.root).resolve(),
        Path(args.default_apk).resolve() if args.default_apk else None,
        Path(args.hook_apk).resolve() if args.hook_apk else None,
    )
    report = {
        "schema_version": 1,
        "ok": all(check.passed for check in checks),
        "checks": [asdict(check) for check in checks],
    }
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(rendered, encoding="utf-8")
    else:
        print(rendered, end="")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
