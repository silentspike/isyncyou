#!/usr/bin/env python3
"""Requirements traceability checker.

Loads every `docs/requirements/*.yml`, validates the requirement schema, and — for
requirements marked `status: implemented` — confirms that each `verified_by` entry
points at something that actually exists in the tree (a Rust test function, or a
file). `status: planned` requirements are tracked but not required to have tests yet
(their tests land with the implementation), though any `design:` document they cite
must already exist.

Exit code is non-zero on any schema violation or dangling reference, so this can be
wired into CI as a required check. It has no third-party dependencies beyond PyYAML.

Usage:
    python3 tools/check_traceability.py            # from the repo root
    python3 tools/check_traceability.py --root .   # explicit root
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

try:
    import yaml
except ImportError:  # pragma: no cover - environment guard
    print("error: PyYAML is required (pip install pyyaml)", file=sys.stderr)
    sys.exit(2)

REQUIRED_FIELDS = ("id", "title", "statement", "status", "acceptance", "verified_by")
VALID_STATUS = ("implemented", "planned")
ID_RE = re.compile(r"^REQ-[A-Z]+-\d{3}$")

# Directories scanned for Rust test functions referenced by `verified_by: [{test: ...}]`.
SOURCE_DIRS = ("crates", "bin", "gui")


def find_rust_sources(root: Path) -> list[Path]:
    files: list[Path] = []
    for d in SOURCE_DIRS:
        base = root / d
        if base.is_dir():
            files.extend(base.rglob("*.rs"))
    return files


def test_exists(name: str, sources: list[Path], _cache: dict[str, bool]) -> bool:
    if name in _cache:
        return _cache[name]
    pat = re.compile(r"\bfn\s+" + re.escape(name) + r"\s*\(")
    found = False
    for f in sources:
        try:
            if pat.search(f.read_text(encoding="utf-8", errors="ignore")):
                found = True
                break
        except OSError:
            continue
    _cache[name] = found
    return found


def check_requirement(req: dict, root: Path, sources: list[Path], cache: dict) -> list[str]:
    errs: list[str] = []
    rid = req.get("id", "<no-id>")

    for field in REQUIRED_FIELDS:
        if field not in req:
            errs.append(f"{rid}: missing required field '{field}'")
    if errs:
        return errs

    if not ID_RE.match(req["id"]):
        errs.append(f"{rid}: id must match REQ-<AREA>-NNN")
    if req["status"] not in VALID_STATUS:
        errs.append(f"{rid}: status must be one of {VALID_STATUS}, got '{req['status']}'")
    if not isinstance(req["acceptance"], list) or not req["acceptance"]:
        errs.append(f"{rid}: acceptance must be a non-empty list")

    design = req.get("design")
    if design and not (root / design).exists():
        errs.append(f"{rid}: design document '{design}' does not exist")

    vby = req.get("verified_by") or []
    if not isinstance(vby, list):
        errs.append(f"{rid}: verified_by must be a list")
        return errs

    if req["status"] == "implemented" and not vby:
        errs.append(f"{rid}: status 'implemented' requires at least one verified_by entry")

    for entry in vby:
        if not isinstance(entry, dict) or len(entry) != 1:
            errs.append(f"{rid}: each verified_by entry must be a single test:/file: mapping")
            continue
        kind, value = next(iter(entry.items()))
        if kind == "test":
            if req["status"] == "implemented" and not test_exists(value, sources, cache):
                errs.append(f"{rid}: verified_by test '{value}' not found in source tree")
        elif kind == "file":
            if not (root / value).exists():
                errs.append(f"{rid}: verified_by file '{value}' does not exist")
        else:
            errs.append(f"{rid}: unknown verified_by kind '{kind}' (expected test/file)")

    return errs


def main() -> int:
    ap = argparse.ArgumentParser(description="Requirements traceability checker")
    ap.add_argument("--root", default=".", help="repository root (default: .)")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    req_dir = root / "docs" / "requirements"
    if not req_dir.is_dir():
        print(f"error: {req_dir} not found", file=sys.stderr)
        return 2

    yml_files = sorted(req_dir.glob("*.yml"))
    if not yml_files:
        print(f"error: no requirement files in {req_dir}", file=sys.stderr)
        return 2

    sources = find_rust_sources(root)
    cache: dict[str, bool] = {}
    seen_ids: set[str] = set()
    all_errs: list[str] = []
    counts = {"implemented": 0, "planned": 0}

    for yml in yml_files:
        try:
            doc = yaml.safe_load(yml.read_text(encoding="utf-8")) or {}
        except yaml.YAMLError as e:
            all_errs.append(f"{yml.name}: YAML parse error: {e}")
            continue
        reqs = doc.get("requirements")
        if not isinstance(reqs, list):
            all_errs.append(f"{yml.name}: top-level 'requirements' list missing")
            continue
        for req in reqs:
            if not isinstance(req, dict):
                all_errs.append(f"{yml.name}: requirement entries must be mappings")
                continue
            rid = req.get("id", "<no-id>")
            if rid in seen_ids:
                all_errs.append(f"{rid}: duplicate requirement id")
            seen_ids.add(rid)
            errs = check_requirement(req, root, sources, cache)
            all_errs.extend(errs)
            if not errs and req.get("status") in counts:
                counts[req["status"]] += 1

    total = counts["implemented"] + counts["planned"]
    print(f"requirements: {total} total | {counts['implemented']} implemented | "
          f"{counts['planned']} planned | {len(yml_files)} file(s)")

    if all_errs:
        print(f"\nFAIL — {len(all_errs)} problem(s):")
        for e in all_errs:
            print(f"  - {e}")
        return 1

    print("OK — every requirement is well-formed and every implemented requirement "
          "is traceable to existing tests.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
