#!/usr/bin/env python3
"""Generate a CycloneDX SBOM from the locked Cargo dependency graph."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import subprocess
import sys
import uuid
from pathlib import Path


def run_metadata(root: Path) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return json.loads(out.stdout)


def timestamp() -> str:
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch:
        try:
            ts = dt.datetime.fromtimestamp(int(epoch), tz=dt.timezone.utc)
        except ValueError:
            raise SystemExit("SOURCE_DATE_EPOCH must be an integer Unix timestamp")
    else:
        ts = dt.datetime.now(tz=dt.timezone.utc)
    return ts.replace(microsecond=0).isoformat().replace("+00:00", "Z")


def purl(name: str, version: str) -> str:
    return f"pkg:cargo/{name}@{version}"


def package_component(pkg: dict, workspace_ids: set[str]) -> dict:
    ref = purl(pkg["name"], pkg["version"])
    component = {
        "type": "application" if pkg["id"] in workspace_ids else "library",
        "bom-ref": ref,
        "name": pkg["name"],
        "version": pkg["version"],
        "purl": ref,
    }
    license_expr = pkg.get("license")
    if license_expr:
        component["licenses"] = [{"expression": license_expr}]
    if pkg.get("repository"):
        component["externalReferences"] = [
            {"type": "vcs", "url": pkg["repository"]},
        ]
    return component


def build_bom(metadata: dict) -> dict:
    workspace_ids = set(metadata.get("workspace_members", []))
    packages = {pkg["id"]: pkg for pkg in metadata.get("packages", [])}
    id_to_ref = {pkg_id: purl(pkg["name"], pkg["version"]) for pkg_id, pkg in packages.items()}
    root_version = next(
        (packages[pkg_id]["version"] for pkg_id in workspace_ids if pkg_id in packages),
        "0.0.0",
    )
    components = [
        package_component(pkg, workspace_ids)
        for pkg in sorted(packages.values(), key=lambda p: (p["name"], p["version"], p["id"]))
    ]
    dependencies = []
    resolve = metadata.get("resolve") or {}
    for node in sorted(resolve.get("nodes", []), key=lambda n: id_to_ref.get(n["id"], n["id"])):
        if node["id"] not in id_to_ref:
            continue
        dependencies.append(
            {
                "ref": id_to_ref[node["id"]],
                "dependsOn": sorted(
                    id_to_ref[dep] for dep in node.get("dependencies", []) if dep in id_to_ref
                ),
            }
        )

    serial_seed = json.dumps(
        {
            "root": metadata.get("workspace_root"),
            "packages": sorted(id_to_ref.values()),
        },
        sort_keys=True,
    )
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, serial_seed)}",
        "version": 1,
        "metadata": {
            "timestamp": timestamp(),
            "tools": [
                {
                    "vendor": "silentspike",
                    "name": "tools/generate_sbom.py",
                    "version": "1",
                }
            ],
            "component": {
                "type": "application",
                "bom-ref": f"pkg:cargo/isyncyou-workspace@{root_version}",
                "name": "isyncyou-workspace",
                "version": root_version,
            },
        },
        "components": components,
        "dependencies": dependencies,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", default=".", help="repository root")
    ap.add_argument("--output", required=True, help="SBOM output path")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    out = Path(args.output)
    bom = build_bom(run_metadata(root))
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(bom, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {out} ({len(bom['components'])} components)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
