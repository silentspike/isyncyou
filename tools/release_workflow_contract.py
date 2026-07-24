#!/usr/bin/env python3
"""Closed release-ref classification and release-object validation for release.yml."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path


STABLE_REF = re.compile(r"^refs/tags/v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
RC_TAG = re.compile(
    r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-rc\.(0|[1-9][0-9]*)$"
)
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


def require_absent_http_status(status: int, resource: str) -> None:
    """Accept only an authoritative GitHub API 404 for a missing resource."""
    if resource not in {"release", "tag"}:
        raise ContractError("invalid release resource")
    if status == 404:
        return
    if status == 200:
        raise ContractError(f"{resource} already exists")
    raise ContractError(f"{resource} existence check unavailable")


def _asset_names(data: dict) -> set[str]:
    assets = data.get("assets")
    if not isinstance(assets, list) or any(not isinstance(asset, dict) for asset in assets):
        raise ContractError("invalid release assets")
    names = [asset.get("name") for asset in assets]
    if any(not isinstance(name, str) or not name for name in names):
        raise ContractError("invalid release asset name")
    if len(names) != len(set(names)):
        raise ContractError("duplicate release asset name")
    return set(names)


def candidate_rc_tags(data: object, sha: str) -> list[str]:
    """Return only RC releases whose object claims the exact candidate commit.

    The caller must still resolve each returned Git tag and pass that map to
    ``select_matching_rc``. A release object's target text is not commit proof.
    """
    if not COMMIT.fullmatch(sha):
        raise ContractError("invalid github sha")
    if not isinstance(data, list) or any(not isinstance(item, dict) for item in data):
        raise ContractError("invalid release list")
    result: list[str] = []
    seen: set[str] = set()
    required = {
        "isyncyou-android-arm64.apk",
        "isyncyou-android-arm64.apk.sha256",
    }
    for item in data:
        tag = item.get("tag_name")
        if not isinstance(tag, str) or not RC_TAG.fullmatch(tag):
            continue
        if item.get("draft") is not False or item.get("prerelease") is not True:
            continue
        if item.get("target_commitish") != sha:
            continue
        if not required.issubset(_asset_names(item)):
            continue
        if tag in seen:
            raise ContractError("duplicate RC release tag")
        seen.add(tag)
        result.append(tag)
    return result


def select_matching_rc(data: object, tag_commits: object, sha: str) -> str:
    if not isinstance(tag_commits, dict) or any(
        not isinstance(tag, str)
        or not isinstance(commit, str)
        or not COMMIT.fullmatch(commit)
        for tag, commit in tag_commits.items()
    ):
        raise ContractError("invalid RC tag commit map")
    for tag in candidate_rc_tags(data, sha):
        if tag_commits.get(tag) == sha:
            return tag
    raise ContractError("no matching RC tag commit")


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
    if not isinstance(data, dict) or not COMMIT.fullmatch(sha):
        raise ContractError("invalid release object")
    expected_tag = RC_TAG if mode == "rc" else STABLE_REF
    tag_value = tag if mode == "rc" else f"refs/tags/{tag}"
    if not expected_tag.fullmatch(tag_value):
        raise ContractError("invalid release tag")
    if data.get("tag_name") != tag:
        raise ContractError("release tag mismatch")
    if data.get("draft") is not False:
        raise ContractError("release is draft")
    if data.get("prerelease") is not (mode == "rc"):
        raise ContractError("release prerelease flag mismatch")
    if data.get("target_commitish") != sha:
        raise ContractError("release target mismatch")
    names = _asset_names(data)
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
    candidates_parser = subparsers.add_parser("candidate-rc-tags")
    candidates_parser.add_argument("--json", type=Path, required=True)
    candidates_parser.add_argument("--sha", required=True)
    select_parser = subparsers.add_parser("select-rc")
    select_parser.add_argument("--json", type=Path, required=True)
    select_parser.add_argument("--tag-commits-json", type=Path, required=True)
    select_parser.add_argument("--sha", required=True)
    validate_parser = subparsers.add_parser("validate-release")
    validate_parser.add_argument("--json", type=Path, required=True)
    validate_parser.add_argument("--mode", choices=("rc", "stable"), required=True)
    validate_parser.add_argument("--tag", required=True)
    validate_parser.add_argument("--sha", required=True)
    absent_parser = subparsers.add_parser("require-absent")
    absent_parser.add_argument("--status", type=int, required=True)
    absent_parser.add_argument("--resource", choices=("release", "tag"), required=True)
    args = parser.parse_args()
    try:
        if args.command == "classify":
            mode, tag = classify(args.event, args.ref, args.sha, args.expected_commit)
            print(json.dumps({"mode": mode, "tag": tag}, separators=(",", ":")))
        elif args.command == "candidate-rc-tags":
            releases = json.loads(args.json.read_text())
            for tag in candidate_rc_tags(releases, args.sha):
                print(tag)
        elif args.command == "select-rc":
            releases = json.loads(args.json.read_text())
            tag_commits = json.loads(args.tag_commits_json.read_text())
            print(select_matching_rc(releases, tag_commits, args.sha))
        elif args.command == "validate-release":
            validate_release_object(json.loads(args.json.read_text()), args.mode, args.tag, args.sha)
        else:
            require_absent_http_status(args.status, args.resource)
    except (ContractError, json.JSONDecodeError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
