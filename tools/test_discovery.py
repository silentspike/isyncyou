#!/usr/bin/env python3
"""Small shared test-name discovery helpers for Rust and Android Kotlin."""

from __future__ import annotations

import re
from pathlib import Path


def rust_test_exists(root: Path, name: str) -> bool:
    pattern = re.compile(r"\bfn\s+" + re.escape(name) + r"\s*\(")
    for directory in ("crates", "bin", "gui"):
        base = root / directory
        if not base.is_dir():
            continue
        for source in base.rglob("*.rs"):
            try:
                if pattern.search(source.read_text(encoding="utf-8", errors="ignore")):
                    return True
            except OSError:
                continue
    return False


def kotlin_test_exists(root: Path, name: str) -> bool:
    """Match only functions immediately annotated with JUnit @Test.

    Helpers, production functions, comments, and string literals are deliberately
    not accepted as evidence. Android unit and instrumentation source sets are the
    only Kotlin trees considered.
    """
    pattern = re.compile(
        r"@Test(?:\s*\([^)]*\))?\s*"
        r"(?:@[A-Za-z0-9_.]+\s*)*"
        r"(?:public\s+|private\s+|internal\s+|protected\s+)?"
        r"fun\s+" + re.escape(name) + r"\s*\(",
        re.MULTILINE,
    )
    for relative in ("android/app/src/test", "android/app/src/androidTest"):
        base = root / relative
        if not base.is_dir():
            continue
        for source in base.rglob("*.kt"):
            try:
                if pattern.search(source.read_text(encoding="utf-8", errors="ignore")):
                    return True
            except OSError:
                continue
    return False


def test_exists(root: Path, name: str) -> bool:
    return rust_test_exists(root, name) or kotlin_test_exists(root, name)
