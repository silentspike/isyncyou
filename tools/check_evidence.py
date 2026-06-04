#!/usr/bin/env python3
"""Evidence-manifest validator.

Validates an evidence manifest against `docs/evidence/manifest.schema.json` and
cross-checks that every `requirement` it cites actually exists in
`docs/requirements/*.yml`. Together with `check_traceability.py` (requirements ->
tests) this closes the loop: requirement -> test -> evidence, each link mechanically
checked.

Exit code is non-zero on a schema violation, a dangling requirement reference, or a
duplicate evidence id, so it can be wired into CI.

Usage:
    python3 tools/check_evidence.py
    python3 tools/check_evidence.py --manifest docs/evidence/sample-manifest.json
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

try:
    import jsonschema
except ImportError:  # pragma: no cover - environment guard
    print("error: jsonschema is required (pip install jsonschema)", file=sys.stderr)
    sys.exit(2)

try:
    import yaml
except ImportError:  # pragma: no cover
    print("error: PyYAML is required (pip install pyyaml)", file=sys.stderr)
    sys.exit(2)


def requirement_ids(root: Path) -> set[str]:
    ids: set[str] = set()
    req_dir = root / "docs" / "requirements"
    for yml in sorted(req_dir.glob("*.yml")):
        doc = yaml.safe_load(yml.read_text(encoding="utf-8")) or {}
        for req in doc.get("requirements", []) or []:
            if isinstance(req, dict) and "id" in req:
                ids.add(req["id"])
    return ids


def main() -> int:
    ap = argparse.ArgumentParser(description="Evidence-manifest validator")
    ap.add_argument("--root", default=".")
    ap.add_argument("--manifest", default="docs/evidence/sample-manifest.json")
    ap.add_argument("--schema", default="docs/evidence/manifest.schema.json")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    schema_path = root / args.schema
    manifest_path = root / args.manifest

    for p in (schema_path, manifest_path):
        if not p.is_file():
            print(f"error: {p} not found", file=sys.stderr)
            return 2

    schema = json.loads(schema_path.read_text(encoding="utf-8"))
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

    errors: list[str] = []

    # 1) JSON Schema validation (format checks enabled so date-time is enforced).
    validator = jsonschema.Draft202012Validator(
        schema, format_checker=jsonschema.Draft202012Validator.FORMAT_CHECKER
    )
    for e in sorted(validator.iter_errors(manifest), key=lambda e: list(e.path)):
        loc = "/".join(str(p) for p in e.path) or "(root)"
        errors.append(f"schema: {loc}: {e.message}")

    # 2) Cross-checks (only meaningful if the document is structurally an object).
    if isinstance(manifest, dict):
        known = requirement_ids(root)
        seen_ev: set[str] = set()
        for entry in manifest.get("entries", []) or []:
            if not isinstance(entry, dict):
                continue
            ev = entry.get("id", "<no-id>")
            if ev in seen_ev:
                errors.append(f"{ev}: duplicate evidence id")
            seen_ev.add(ev)
            req = entry.get("requirement")
            if req and req not in known:
                errors.append(
                    f"{ev}: requirement '{req}' not found in docs/requirements/*.yml"
                )

    n_entries = len(manifest.get("entries", [])) if isinstance(manifest, dict) else 0
    print(f"evidence manifest: {n_entries} entr{'y' if n_entries == 1 else 'ies'} | {manifest_path.name}")

    if errors:
        print(f"\nFAIL — {len(errors)} problem(s):")
        for e in errors:
            print(f"  - {e}")
        return 1

    print("OK — manifest is schema-valid and every cited requirement exists.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
