#!/usr/bin/env python3
"""Verify GitHub workflow actions are pinned to immutable commit SHAs."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

USES_RE = re.compile(r"^\s*(?:-\s*)?uses:\s*['\"]?([^'\"\s#]+)")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")


def is_local_action(ref: str) -> bool:
    return ref.startswith("./") or ref.startswith("../")


def split_ref(value: str) -> tuple[str, str] | None:
    if "@" not in value:
        return None
    action, ref = value.rsplit("@", 1)
    if not action or not ref:
        return None
    return action, ref


def workflow_files(root: Path) -> list[Path]:
    workflows = root / ".github" / "workflows"
    if not workflows.is_dir():
        return []
    return sorted([*workflows.glob("*.yml"), *workflows.glob("*.yaml")])


def check_file(path: Path, root: Path) -> tuple[int, list[str]]:
    errors: list[str] = []
    checked = 0
    rel = path.relative_to(root)

    for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        match = USES_RE.match(line)
        if not match:
            continue
        value = match.group(1)
        if is_local_action(value):
            continue
        split = split_ref(value)
        if split is None:
            errors.append(f"{rel}:{lineno}: action ref is missing '@': {value}")
            continue
        action, ref = split
        checked += 1
        if not SHA_RE.fullmatch(ref):
            errors.append(
                f"{rel}:{lineno}: {action}@{ref} is not pinned to a 40-char commit SHA"
            )

    return checked, errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root (default: .)")
    args = parser.parse_args()

    root = Path(args.root).resolve()
    files = workflow_files(root)
    if not files:
        print(f"error: no workflow files found under {root / '.github' / 'workflows'}", file=sys.stderr)
        return 2

    total = 0
    errors: list[str] = []
    for path in files:
        checked, file_errors = check_file(path, root)
        total += checked
        errors.extend(file_errors)

    print(f"workflow action pins: {total} action refs checked | {len(files)} workflow file(s)")
    if errors:
        print(f"\nFAIL - {len(errors)} unpinned action ref(s):")
        for err in errors:
            print(f"  - {err}")
        return 1

    print("OK - every external workflow action is pinned to a commit SHA.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
