#!/usr/bin/env python3
"""Generate a HEAD-pinned evidence manifest with captured command output.

`docs/evidence/sample-manifest.json` is the **curated spec**: it lists which evidence
entries exist (id, requirement, claim, method, command, test/artifact). This generator
turns that spec into a **freshly produced** manifest:

  * `commit` is stamped to the current Git HEAD and `generated_at` to now, so the result
    passes `check_evidence.py --require-head` (it is bound to the exact tree it ran on).
  * for `test` / `command` / `example` entries it **runs the command** and captures a
    machine-readable excerpt of real stdout/stderr into `evidence`, with `result` set
    from the exit code (honest pass/fail).
  * `probe` entries are **live checks that need credentials**, so they are carried over
    from the curated manifest unchanged (their `evidence` is a real prior live result)
    with a `notes` marker that they were not re-executed during generation — unless
    `--run-live` is passed (then they run too, e.g. on a credentialed nightly).

Output is written to `--out` (default `docs/evidence/generated-manifest.json`, which is
git-ignored — the curated manifest stays the committed source of truth). Validate it with:

    python3 tools/check_evidence.py --manifest docs/evidence/generated-manifest.json --require-head

Usage:
    python3 tools/generate_evidence_manifest.py
    python3 tools/generate_evidence_manifest.py --out /tmp/m.json --run-live
"""

from __future__ import annotations

import argparse
import datetime
import json
import subprocess
import sys
from pathlib import Path

RUN_METHODS = {"test", "command", "example"}
MAX_EVIDENCE = 600  # chars of captured output kept per entry


def git_head(root: Path) -> str:
    out = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "HEAD"],
        check=True, stdout=subprocess.PIPE, text=True,
    )
    return out.stdout.strip()


def excerpt(text: str) -> str:
    """A compact, single-spaced machine-readable excerpt of captured output."""
    lines = [ln.rstrip() for ln in text.splitlines() if ln.strip()]
    joined = " | ".join(lines)
    if len(joined) > MAX_EVIDENCE:
        joined = joined[: MAX_EVIDENCE - 1] + "…"
    return joined or "(no output)"


def run_entry(entry: dict, root: Path, timeout: int) -> dict:
    """Run one runnable entry's command and return an updated copy with captured output."""
    out = dict(entry)
    try:
        proc = subprocess.run(
            entry["command"], shell=True, cwd=str(root),
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, timeout=timeout,
        )
        out["result"] = "pass" if proc.returncode == 0 else "fail"
        out["evidence"] = excerpt(proc.stdout)
    except subprocess.TimeoutExpired:
        out["result"] = "fail"
        out["evidence"] = f"(timed out after {timeout}s)"
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description="Generate a HEAD-pinned evidence manifest")
    ap.add_argument("--root", default=".")
    ap.add_argument("--spec", default="docs/evidence/sample-manifest.json")
    ap.add_argument("--out", default="docs/evidence/generated-manifest.json")
    ap.add_argument("--timeout", type=int, default=900, help="per-command timeout (s)")
    ap.add_argument("--run-live", action="store_true", help="also execute probe (live) entries")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    spec = json.loads((root / args.spec).read_text(encoding="utf-8"))
    head = git_head(root)
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    entries_out = []
    ran = carried = failed = 0
    for entry in spec.get("entries", []):
        method = entry.get("method")
        if method in RUN_METHODS or (method == "probe" and args.run_live):
            print(f"  running {entry['id']} ({method}): {entry['command']}", file=sys.stderr)
            updated = run_entry(entry, root, args.timeout)
            ran += 1
            if updated["result"] == "fail":
                failed += 1
                print(f"    -> FAIL: {updated['evidence'][:160]}", file=sys.stderr)
            entries_out.append(updated)
        else:
            # live probe carried forward (needs credentials; not re-executed here)
            carried_entry = dict(entry)
            note = carried_entry.get("notes", "")
            marker = "carried from the curated manifest; live probe not re-executed during generation (needs credentials, run with --run-live)."
            carried_entry["notes"] = (note + " " if note else "") + marker
            entries_out.append(carried_entry)
            carried += 1

    manifest = {
        "manifest_version": "1",
        "generated_at": now,
        "commit": head,
        "entries": entries_out,
    }
    out_path = root / args.out
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    print(
        f"generated {out_path.name}: {len(entries_out)} entries | {ran} executed "
        f"({failed} failed) | {carried} live carried | commit {head[:12]}"
    )
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
