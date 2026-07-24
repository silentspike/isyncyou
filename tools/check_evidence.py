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
    python3 tools/check_evidence.py --manifest generated.json --require-head
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

from test_discovery import rust_test_exists, test_exists

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


SOURCE_DIRS = ("crates", "bin", "gui")


def git_head(root: Path) -> str | None:
    """Current Git HEAD, or None if the root is not a Git checkout."""
    try:
        out = subprocess.run(
            ["git", "-C", str(root), "rev-parse", "HEAD"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return out.stdout.strip()


def git_commit_exists(root: Path, commit: str) -> bool:
    try:
        subprocess.run(
            ["git", "-C", str(root), "cat-file", "-e", f"{commit}^{{commit}}"],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return False
    return True


def main() -> int:
    ap = argparse.ArgumentParser(description="Evidence-manifest validator")
    ap.add_argument("--root", default=".")
    ap.add_argument("--manifest", default="docs/evidence/sample-manifest.json")
    ap.add_argument("--schema", default="docs/evidence/manifest.schema.json")
    ap.add_argument(
        "--require-head",
        action="store_true",
        help="require manifest.commit to equal the current Git HEAD (for generated CI artifacts)",
    )
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
        head = git_head(root)
        manifest_commit = manifest.get("commit")
        if head and isinstance(manifest_commit, str):
            if not git_commit_exists(root, manifest_commit):
                errors.append(f"manifest commit does not exist in this repo: {manifest_commit}")
            if args.require_head and manifest_commit != head:
                errors.append(
                    "manifest commit does not match HEAD "
                    f"(manifest={manifest_commit!r}, HEAD={head})"
                )

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
            # A `test`-method entry must name a real test and that test must exist —
            # so the evidence is traceable to executable code, not decorative.
            if entry.get("method") == "test":
                tname = entry.get("test")
                if not tname:
                    errors.append(f"{ev}: method 'test' requires a 'test' field naming the function")
                elif not test_exists(root, tname):
                    errors.append(f"{ev}: test '{tname}' not found in the source tree")
            # Any cited artifact that is a repo-relative file must exist.
            artifact = entry.get("artifact")
            if artifact and "/" in artifact and not artifact.startswith(("http://", "https://")):
                if not (root / artifact).exists():
                    errors.append(f"{ev}: artifact '{artifact}' does not exist")

    n_entries = len(manifest.get("entries", [])) if isinstance(manifest, dict) else 0
    print(f"evidence manifest: {n_entries} entr{'y' if n_entries == 1 else 'ies'} | {manifest_path.name}")

    if errors:
        print(f"\nFAIL — {len(errors)} problem(s):")
        for e in errors:
            print(f"  - {e}")
        return 1

    print(
        "OK — manifest is schema-valid; manifest commit exists"
        + (" and matches HEAD" if args.require_head else "")
        + "; every cited requirement exists; every test entry names a real test; "
        "every artifact exists."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
