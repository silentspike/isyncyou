#!/usr/bin/env python3
"""Closed release-ref classification and release-object validation for release.yml."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


STABLE_REF = re.compile(r"^refs/tags/v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
COMMIT = re.compile(r"^[0-9a-f]{40}$")
EXPECTED_ASSETS = {
    "isyncyou-linux-x86_64.tar.gz",
    "isyncyou-linux-x86_64.tar.gz.sha256",
    "isyncyou-x86_64.AppImage",
    "isyncyou-x86_64.AppImage.sha256",
    "isyncyou-windows-x86_64.zip",
    "isyncyou-windows-x86_64.zip.sha256",
    "isyncyou-android-arm64.apk",
    "isyncyou-android-arm64.apk.sha256",
    "SHA256SUMS",
    "isyncyou.sbom.cdx.json",
    "isyncyou-linux-x86_64.tar.gz.cosign.bundle",
    "isyncyou-x86_64.AppImage.cosign.bundle",
    "isyncyou-windows-x86_64.zip.cosign.bundle",
    "isyncyou-android-arm64.apk.cosign.bundle",
    "isyncyou.sbom.cdx.json.cosign.bundle",
    "SHA256SUMS.cosign.bundle",
}


class ContractError(ValueError):
    """A release input or postcondition violates the closed contract."""


def classify(event: str, ref: str, sha: str, expected_commit: str | None = "") -> tuple[str, str]:
    if not isinstance(sha, str) or not COMMIT.fullmatch(sha):
        raise ContractError("invalid github sha")
    if event == "workflow_dispatch":
        if ref != "refs/heads/main":
            raise ContractError("rc dispatch must use main")
        if not isinstance(expected_commit, str) or not COMMIT.fullmatch(expected_commit) or expected_commit != sha:
            raise ContractError("expected commit does not match github sha")
        return "rc", ""
    if event == "push" and STABLE_REF.fullmatch(ref):
        return "stable", ref.removeprefix("refs/tags/")
    raise ContractError("invalid release ref")


def validate_release_object(data: dict, mode: str, tag: str, sha: str) -> None:
    if data.get("tag_name") != tag:
        raise ContractError("release tag mismatch")
    if data.get("draft") is not False:
        raise ContractError("release is draft")
    if data.get("prerelease") is not (mode == "rc"):
        raise ContractError("release prerelease flag mismatch")
    if data.get("target_commitish") != sha:
        raise ContractError("release target mismatch")
    names = {asset.get("name") for asset in data.get("assets", []) if isinstance(asset, dict)}
    missing = sorted(EXPECTED_ASSETS - names)
    if missing:
        raise ContractError("missing release assets: " + ",".join(missing))


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    classify_parser = subparsers.add_parser("classify")
    classify_parser.add_argument("--event", required=True)
    classify_parser.add_argument("--ref", required=True)
    classify_parser.add_argument("--sha", required=True)
    classify_parser.add_argument("--expected-commit", default="")
    validate_parser = subparsers.add_parser("validate-release")
    validate_parser.add_argument("--json", type=Path, required=True)
    validate_parser.add_argument("--mode", choices=("rc", "stable"), required=True)
    validate_parser.add_argument("--tag", required=True)
    validate_parser.add_argument("--sha", required=True)
    args = parser.parse_args()
    try:
        if args.command == "classify":
            mode, tag = classify(args.event, args.ref, args.sha, args.expected_commit)
            print(json.dumps({"mode": mode, "tag": tag}, separators=(",", ":")))
        else:
            validate_release_object(json.loads(args.json.read_text()), args.mode, args.tag, args.sha)
    except (ContractError, json.JSONDecodeError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
