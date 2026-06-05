#!/usr/bin/env python3
"""Live PBS round-trip probe for iSyncYou.

Requires a real Proxmox Backup Server repository. The probe creates a temporary
iSyncYou config, runs `isyncyou pbs-backup`, restores the created snapshot into a
temporary directory, verifies `manifest.json` and `store.db`, then forgets the
test snapshot. Secrets are read from a file or environment and are never printed.

Usage:
    ISYNCYOU_PBS_REPOSITORY='user@realm!token@host:datastore' \
    ISYNCYOU_PBS_PASSWORD_FILE=/run/secrets/pbs-token \
    ISYNCYOU_PBS_FINGERPRINT='AA:BB:...' \
    python3 tools/live_pbs_roundtrip.py
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path


FINGERPRINT_ENV = "ISYNCYOU_PBS_FINGERPRINT"
KEEP_ENV = "ISYNCYOU_PBS_KEEP_SNAPSHOT"
NAMESPACE_ENV = "ISYNCYOU_PBS_NAMESPACE"
PASSWORD_ENV = "ISYNCYOU_PBS_PASSWORD"
PASSWORD_FILE_ENV = "ISYNCYOU_PBS_PASSWORD_FILE"
REPOSITORY_ENV = "ISYNCYOU_PBS_REPOSITORY"


def fail(msg: str, code: int = 1) -> None:
    print(f"error: {msg}", file=sys.stderr)
    raise SystemExit(code)


def run(cmd: list[str], env: dict[str, str] | None = None) -> str:
    proc = subprocess.run(
        cmd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd[:2])} failed with {proc.returncode}:\n{proc.stdout}")
    return proc.stdout


def password_file(tmp: Path) -> tuple[Path, str]:
    explicit = os.environ.get(PASSWORD_FILE_ENV)
    if explicit:
        path = Path(explicit)
        secret = path.read_text(encoding="utf-8").strip()
        if not secret:
            fail(f"{PASSWORD_FILE_ENV} is empty")
        return path, secret

    secret = os.environ.get(PASSWORD_ENV, "").strip()
    if not secret:
        fail(f"set {PASSWORD_FILE_ENV} or {PASSWORD_ENV}", 2)
    path = tmp / "pbs.token"
    path.write_text(secret, encoding="utf-8")
    path.chmod(0o600)
    return path, secret


def write_config(tmp: Path, repo: str, pbs_pass: Path) -> Path:
    cfg = tmp / "isyncyou.toml"
    lines = [
        "[[accounts]]",
        'id = "primary"',
        'username = "live-pbs-probe@example.invalid"',
        f'sync_root = "{tmp / "sync"}"',
        f'archive_root = "{tmp / "archive"}"',
        "",
        "[pbs]",
        f'repository = "{repo}"',
        f'password_file = "{pbs_pass}"',
    ]
    if fp := os.environ.get(FINGERPRINT_ENV):
        lines.append(f'fingerprint = "{fp}"')
    if ns := os.environ.get(NAMESPACE_ENV):
        lines.append(f'namespace = "{ns}"')
    cfg.write_text("\n".join(lines) + "\n", encoding="utf-8")
    cfg.chmod(0o600)
    return cfg


def pbs_env(secret: str) -> dict[str, str]:
    env = os.environ.copy()
    env["PBS_PASSWORD"] = secret
    if fp := os.environ.get(FINGERPRINT_ENV):
        env["PBS_FINGERPRINT"] = fp
    return env


def forget_snapshot(snapshot: str, repo: str, env: dict[str, str]) -> int:
    cmd = ["proxmox-backup-client", "snapshot", "forget", snapshot, "--repository", repo]
    if ns := os.environ.get(NAMESPACE_ENV):
        cmd.extend(["--ns", ns])
    proc = subprocess.run(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if proc.returncode != 0:
        print(proc.stdout, file=sys.stderr)
    return proc.returncode


def snapshot_remaining(snapshot: str, repo: str, env: dict[str, str]) -> int:
    cmd = [
        "proxmox-backup-client",
        "snapshot",
        "list",
        "--repository",
        repo,
        "--output-format",
        "json",
    ]
    if ns := os.environ.get(NAMESPACE_ENV):
        cmd.extend(["--ns", ns])
    out = run(cmd, env=env)
    return sum(1 for item in json.loads(out) if item.get("snapshot") == snapshot)


def main() -> int:
    repo_root = Path(__file__).resolve().parent.parent
    default_bin = repo_root / "target" / "debug" / "isyncyou"
    isyncyou = Path(os.environ.get("ISYNCYOU_BIN", str(default_bin)))
    if not isyncyou.exists():
        fail(f"{isyncyou} not found; build the workspace first or set ISYNCYOU_BIN", 2)

    repo = os.environ.get(REPOSITORY_ENV, "").strip()
    if not repo:
        fail(f"set {REPOSITORY_ENV}", 2)

    tmp = Path(tempfile.mkdtemp(prefix="isyncyou-live-pbs."))
    snapshot: str | None = None
    try:
        (tmp / "sync").mkdir()
        (tmp / "archive").mkdir()
        (tmp / "restore").mkdir()
        pbs_pass, secret = password_file(tmp)
        cfg = write_config(tmp, repo, pbs_pass)
        env = pbs_env(secret)

        print("config_check=running")
        run([str(isyncyou), "check", "--config", str(cfg)])

        backup_out = run([str(isyncyou), "pbs-backup", "--config", str(cfg), "--account", "primary"])
        match = re.search(r"PBS snapshot (host/[^\s]+)", backup_out)
        if not match:
            raise RuntimeError(f"could not parse snapshot id from backup output:\n{backup_out}")
        snapshot = match.group(1)
        print(f"backup_snapshot={snapshot}")

        restore_dir = tmp / "restore"
        run([str(isyncyou), "pbs-restore", "--config", str(cfg), "--snapshot", snapshot, "--into", str(restore_dir)])

        manifest = json.loads((restore_dir / "manifest.json").read_text(encoding="utf-8"))
        store = restore_dir / "store.db"
        with sqlite3.connect(store) as conn:
            schema = conn.execute("pragma user_version").fetchone()[0]
            tables = conn.execute(
                "select count(*) from sqlite_master where type = 'table'"
            ).fetchone()[0]

        if manifest.get("account") != "primary":
            raise RuntimeError(f"unexpected manifest account: {manifest.get('account')!r}")
        if not store.exists() or tables <= 0:
            raise RuntimeError("restored store.db is missing or has no tables")

        print(f"restore_manifest_account={manifest['account']}")
        print(f"restore_manifest_schema_version={manifest['schema_version']}")
        print(f"restore_store_user_version={schema}")
        print(f"restore_store_table_count={tables}")

        if os.environ.get(KEEP_ENV) == "1":
            print("cleanup_snapshot=kept")
        else:
            rc = forget_snapshot(snapshot, repo, env)
            if rc != 0:
                raise RuntimeError(f"snapshot cleanup failed for {snapshot}")
            remaining = snapshot_remaining(snapshot, repo, env)
            print(f"cleanup_snapshot_remaining={remaining}")
            if remaining != 0:
                raise RuntimeError(f"snapshot still listed after cleanup: {snapshot}")

        print("VERDICT PBS ROUNDTRIP YES")
        return 0
    finally:
        if snapshot and os.environ.get(KEEP_ENV) != "1":
            # Best-effort second cleanup path if verification failed after backup.
            try:
                pbs_pass, secret = password_file(tmp)
                env = pbs_env(secret)
                if snapshot_remaining(snapshot, repo, env) > 0:
                    forget_snapshot(snapshot, repo, env)
            except Exception:
                pass
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
