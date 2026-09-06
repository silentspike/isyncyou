#!/usr/bin/env python3
"""Crash-safe, redacted Android evidence controller for issue #642.

Private account, item, request, and foreground bindings exist only in the owner-only
state file. Standard output contains one closed status code per command.

Select each controlled synthetic unread message visibly before register-fixture.
After proposing the action, click Confirm, then run observe-prompt while the real
system prompt is open. Approve P1 or cancel P2, then run observe. Cleanup can always
restore registered fixtures, but cannot supply missing scenario observations.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
import re
import socket
import stat
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Callable, Iterator, Protocol


SCHEMA_VERSION = 2
PACKAGE = "com.silentspike.isyncyou.debug"
ROWS = {"P1", "P2"}
EXPECTATIONS = {"read", "unread"}
MAX_STATE_BYTES = 64 * 1024
MAX_CDP_BYTES = 64 * 1024
GRAPH_POLL_ATTEMPTS = 20
# Android refreshes its StoreArchive cache on a 30-second product loop. Cover one
# complete interval plus scheduler jitter without claiming that `/sync/now` exists.
STORE_POLL_ATTEMPTS = 90
POLL_INTERVAL_SECONDS = 0.5
HEX_64 = re.compile(r"^[0-9a-f]{64}$")
COMPONENT = re.compile(r"^[A-Za-z0-9._$]+/[A-Za-z0-9._$]+$")


class ProbeBlocked(RuntimeError):
    """A closed operational failure that must retain cleanup state."""


class ProductAdapter(Protocol):
    def capture_foreground(self) -> str | None: ...

    def resolve_unread_fixture(
        self, row: str, excluded_item_ids: set[str]
    ) -> tuple[str, str]: ...

    def store_is_read(self, account: str, item_id: str) -> bool: ...

    def graph_is_read(self, account: str, item_id: str) -> bool: ...

    def refresh_store(self) -> bool: ...

    def set_read(self, account: str, item_id: str, is_read: bool) -> bool: ...

    def restore_foreground(self, component: str | None) -> bool: ...

    def observe_native_prompt(self, account: str) -> dict[str, str]: ...

    def scenario_verified(self, account: str, binding: dict[str, str], approved: bool) -> bool: ...


def _select_visible_product_page(
    targets: object,
    is_visible: Callable[[str], bool],
) -> dict[str, object]:
    if not isinstance(targets, list):
        raise ProbeBlocked("webview_target_unavailable")
    visible: list[dict[str, object]] = []
    for target in targets:
        if (
            not isinstance(target, dict)
            or target.get("type") != "page"
            or "appassets.androidplatform.net" not in str(target.get("url", ""))
            or not isinstance(target.get("webSocketDebuggerUrl"), str)
        ):
            continue
        try:
            if is_visible(str(target["webSocketDebuggerUrl"])):
                visible.append(target)
        except ProbeBlocked:
            continue
    if len(visible) != 1:
        raise ProbeBlocked("webview_target_unavailable")
    return visible[0]


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ProbeBlocked("state_invalid")
        result[key] = value
    return result


def _require_private_state_path(path: Path) -> None:
    absolute = path.absolute()
    if absolute != Path("/tmp") and Path("/tmp") not in absolute.parents:
        raise ProbeBlocked("state_path_invalid")


def _validate_regular_owner_file(path: Path, *, allow_empty: bool = False) -> None:
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
        raise ProbeBlocked("state_file_invalid")
    if metadata.st_uid != os.getuid() or stat.S_IMODE(metadata.st_mode) != 0o600:
        raise ProbeBlocked("state_file_invalid")
    if metadata.st_size > MAX_STATE_BYTES or (not allow_empty and metadata.st_size == 0):
        raise ProbeBlocked("state_size_invalid")


def _validate_state(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ProbeBlocked("state_invalid")
    allowed = {
        "schema_version", "package", "apk_sha256", "prior_foreground",
        "rows", "cleanup_retry_was_required", "foreground_restored", "state",
    }
    if set(value) - allowed or value.get("schema_version") != SCHEMA_VERSION:
        raise ProbeBlocked("state_invalid")
    if value.get("package") != PACKAGE or not HEX_64.fullmatch(str(value.get("apk_sha256", ""))):
        raise ProbeBlocked("state_invalid")
    prior = value.get("prior_foreground")
    if prior is not None and (not isinstance(prior, str) or not COMPONENT.fullmatch(prior)):
        raise ProbeBlocked("state_invalid")
    if not isinstance(value.get("cleanup_retry_was_required"), bool):
        raise ProbeBlocked("state_invalid")
    if not isinstance(value.get("foreground_restored"), bool):
        raise ProbeBlocked("state_invalid")
    if value.get("state") not in {"prepared", "active", "cleaned", "blocked"}:
        raise ProbeBlocked("state_invalid")
    rows = value.get("rows")
    if not isinstance(rows, dict) or set(rows) - ROWS:
        raise ProbeBlocked("state_invalid")
    for row_name, row in rows.items():
        if not isinstance(row, dict):
            raise ProbeBlocked("state_invalid")
        row_allowed = {
            "phase", "account", "item_id", "revert_required", "effect_observed",
            "cleanup_done", "revert_write_acknowledged", "graph_verified",
            "store_verified",
            "failure_code",
            "observation_state", "observation_graph_verified", "observation_store_verified",
            "scenario_binding", "scenario_verified",
        }
        if set(row) - row_allowed:
            raise ProbeBlocked("state_invalid")
        if row.get("phase") not in {
            "cleanup_registered", "submitted", "observed", "cleaned", "blocked",
        }:
            raise ProbeBlocked("state_invalid")
        if not isinstance(row.get("account"), str) or not row["account"]:
            raise ProbeBlocked("state_invalid")
        if not isinstance(row.get("item_id"), str) or not row["item_id"]:
            raise ProbeBlocked("state_invalid")
        for field in (
            "revert_required", "effect_observed", "cleanup_done",
            "revert_write_acknowledged", "graph_verified", "store_verified",
            "observation_graph_verified", "observation_store_verified", "scenario_verified",
        ):
            if not isinstance(row.get(field), bool):
                raise ProbeBlocked("state_invalid")
        if row.get("observation_state") not in {"not_started", "prompt_observed", "verified", "failed"}:
            raise ProbeBlocked("state_invalid")
        binding = row.get("scenario_binding")
        if binding is not None and (
            not isinstance(binding, dict)
            or set(binding) != {"pending_id", "session_id", "turn_id", "audit_watermark"}
            or any(not isinstance(v, str) or not v or len(v) > 128 for v in binding.values())
        ):
            raise ProbeBlocked("state_invalid")
        if row.get("failure_code") is not None and row["failure_code"] not in {
            "observation_failed", "revert_failed", "store_verification_failed",
            "graph_verification_failed", "store_refresh_failed",
            "foreground_restore_failed",
        }:
            raise ProbeBlocked("state_invalid")
        if row_name == "P2" and row.get("effect_observed"):
            raise ProbeBlocked("state_invalid")
    return value


def _load_state(path: Path) -> dict[str, object]:
    _require_private_state_path(path)
    _validate_regular_owner_file(path)
    raw = path.read_bytes()
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProbeBlocked("state_invalid") from error
    return _validate_state(value)


def _atomic_write_json(
    path: Path, value: dict[str, object], *, fail_at: str | None = None
) -> None:
    _validate_state(value)
    encoded = (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(encoded) > MAX_STATE_BYTES:
        raise ProbeBlocked("state_size_invalid")
    parent = path.parent
    fd, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.tmp-", dir=parent)
    temporary = Path(temporary_name)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb", closefd=True) as stream:
            stream.write(encoded)
            stream.flush()
            os.fsync(stream.fileno())
        if fail_at == "before_replace":
            raise RuntimeError("injected_before_replace")
        os.replace(temporary, path)
        directory_fd = os.open(parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        if fail_at == "after_replace":
            raise RuntimeError("injected_after_replace")
    finally:
        if temporary.exists():
            temporary.unlink()


@contextlib.contextmanager
def _state_lock(path: Path) -> Iterator[None]:
    _require_private_state_path(path)
    lock_path = path.with_name(path.name + ".lock")
    flags = os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open(lock_path, flags, 0o600)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid():
            raise ProbeBlocked("state_lock_invalid")
        os.fchmod(fd, 0o600)
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)


def prepare_state(path: Path, package: str, apk_sha256: str, adapter: ProductAdapter) -> None:
    if package != PACKAGE or not HEX_64.fullmatch(apk_sha256):
        raise ProbeBlocked("prepare_invalid")
    _require_private_state_path(path)
    with _state_lock(path):
        if path.exists():
            _validate_regular_owner_file(path, allow_empty=True)
            if path.stat().st_size:
                raise ProbeBlocked("state_already_initialized")
        state: dict[str, object] = {
            "schema_version": SCHEMA_VERSION,
            "package": package,
            "apk_sha256": apk_sha256,
            "prior_foreground": adapter.capture_foreground(),
            "rows": {},
            "cleanup_retry_was_required": False,
            "foreground_restored": False,
            "state": "prepared",
        }
        _atomic_write_json(path, state)


def register_fixture(path: Path, row_name: str, adapter: ProductAdapter) -> None:
    if row_name not in ROWS:
        raise ProbeBlocked("row_invalid")
    with _state_lock(path):
        state = _load_state(path)
        if state["state"] not in {"prepared", "active"}:
            raise ProbeBlocked("state_transition_invalid")
        rows = state["rows"]
        assert isinstance(rows, dict)
        if row_name in rows:
            raise ProbeBlocked("row_already_registered")
        excluded = {
            str(row["item_id"])
            for row in rows.values()
            if isinstance(row, dict) and isinstance(row.get("item_id"), str)
        }
        account, item_id = adapter.resolve_unread_fixture(row_name, excluded)
        if not account or not item_id or item_id in excluded:
            raise ProbeBlocked("fixture_unavailable")
        if adapter.graph_is_read(account, item_id) is not False or adapter.store_is_read(account, item_id) is not False:
            raise ProbeBlocked("fixture_unavailable")
        rows[row_name] = {
            "phase": "cleanup_registered",
            "account": account,
            "item_id": item_id,
            "revert_required": True,
            "effect_observed": False,
            "cleanup_done": False,
            "revert_write_acknowledged": False,
            "graph_verified": False,
            "store_verified": False,
            "failure_code": None,
            "observation_state": "not_started",
            "observation_graph_verified": False,
            "observation_store_verified": False,
            "scenario_binding": None,
            "scenario_verified": False,
        }
        state["state"] = "active"
        _atomic_write_json(path, state)


def _poll_store(
    adapter: ProductAdapter, account: str, item_id: str, expected_read: bool
) -> bool:
    for attempt in range(STORE_POLL_ATTEMPTS):
        if adapter.store_is_read(account, item_id) is expected_read:
            return True
        if attempt + 1 < STORE_POLL_ATTEMPTS:
            time.sleep(POLL_INTERVAL_SECONDS)
    return False


def _poll_graph(
    adapter: ProductAdapter, account: str, item_id: str, expected_read: bool
) -> bool:
    for attempt in range(GRAPH_POLL_ATTEMPTS):
        if adapter.graph_is_read(account, item_id) is expected_read:
            return True
        if attempt + 1 < GRAPH_POLL_ATTEMPTS:
            time.sleep(POLL_INTERVAL_SECONDS)
    return False


def observe_prompt(path: Path, row_name: str, adapter: ProductAdapter) -> None:
    with _state_lock(path):
        state = _load_state(path)
        row = state["rows"].get(row_name)
        if not isinstance(row, dict) or row["phase"] != "cleanup_registered" or row["observation_state"] != "not_started":
            raise ProbeBlocked("state_transition_invalid")
        binding = adapter.observe_native_prompt(str(row["account"]))
        if not isinstance(binding, dict) or any(
            isinstance(other.get("scenario_binding"), dict)
            and (other["scenario_binding"].get("pending_id") == binding.get("pending_id")
                 or (other["scenario_binding"].get("session_id") == binding.get("session_id")
                     and other["scenario_binding"].get("turn_id") == binding.get("turn_id")))
            for other in state["rows"].values()
        ):
            raise ProbeBlocked("observation_failed")
        row["scenario_binding"] = binding
        row["observation_state"] = "prompt_observed"
        _atomic_write_json(path, state)


def observe(path: Path, row_name: str, expected: str, adapter: ProductAdapter) -> None:
    if row_name not in ROWS or expected not in EXPECTATIONS:
        raise ProbeBlocked("observation_invalid")
    expected_read = expected == "read"
    if (row_name == "P1") is not expected_read:
        raise ProbeBlocked("observation_invalid")
    with _state_lock(path):
        state = _load_state(path)
        rows = state["rows"]
        assert isinstance(rows, dict)
        row = rows.get(row_name)
        if not isinstance(row, dict) or row.get("phase") != "cleanup_registered" or row["observation_state"] != "prompt_observed":
            raise ProbeBlocked("state_transition_invalid")
        row["phase"] = "submitted"
        # An interruption or exception must remain a failed observation even if
        # a later cleanup succeeds. Only the complete observation changes this.
        row["observation_state"] = "failed"
        _atomic_write_json(path, state)
        account, item_id = str(row["account"]), str(row["item_id"])
        scenario_ok = False
        for attempt in range(STORE_POLL_ATTEMPTS):
            if adapter.scenario_verified(account, row["scenario_binding"], expected_read) is True:
                scenario_ok = True
                break
            if attempt + 1 < STORE_POLL_ATTEMPTS:
                time.sleep(POLL_INTERVAL_SECONDS)
        if not scenario_ok:
            raise ProbeBlocked("observation_failed")
        row["scenario_verified"] = True
        if not _poll_graph(adapter, account, item_id, expected_read):
            row["phase"] = "blocked"
            row["failure_code"] = "graph_verification_failed"
            state["state"] = "blocked"
            _atomic_write_json(path, state)
            raise ProbeBlocked("graph_verification_failed")
        row["observation_graph_verified"] = True
        if not adapter.refresh_store():
            row["phase"] = "blocked"
            row["failure_code"] = "store_refresh_failed"
            state["state"] = "blocked"
            _atomic_write_json(path, state)
            raise ProbeBlocked("store_refresh_failed")
        if not _poll_store(adapter, account, item_id, expected_read):
            row["phase"] = "blocked"
            row["failure_code"] = "observation_failed"
            state["state"] = "blocked"
            _atomic_write_json(path, state)
            raise ProbeBlocked("observation_failed")
        row["phase"] = "observed"
        row["effect_observed"] = expected_read
        row["observation_store_verified"] = True
        row["observation_state"] = "verified"
        _atomic_write_json(path, state)


def cleanup(path: Path, adapter: ProductAdapter, restore_foreground: bool) -> None:
    with _state_lock(path):
        state = _load_state(path)
        rows = state["rows"]
        assert isinstance(rows, dict)
        failed = False
        for row_name, row in rows.items():
            assert isinstance(row, dict)
            if row["cleanup_done"]:
                continue
            account, item_id = str(row["account"]), str(row["item_id"])
            try:
                currently_read = adapter.graph_is_read(account, item_id)
                if row_name == "P2" and currently_read:
                    # Revert unexpected effects for safety, but retain failure of
                    # the no-effect claim even if its cleanup succeeds.
                    row["observation_state"] = "failed"
                if currently_read:
                    if not adapter.set_read(account, item_id, False):
                        raise ProbeBlocked("revert_failed")
                    row["revert_write_acknowledged"] = True
                if not _poll_graph(adapter, account, item_id, False):
                    raise ProbeBlocked("graph_verification_failed")
                row["graph_verified"] = True
                if not adapter.refresh_store():
                    raise ProbeBlocked("store_refresh_failed")
                if not _poll_store(adapter, account, item_id, False):
                    raise ProbeBlocked("store_verification_failed")
                row["store_verified"] = True
                row["cleanup_done"] = True
                row["phase"] = "cleaned"
                row["failure_code"] = None
            except ProbeBlocked as error:
                failed = True
                row["phase"] = "blocked"
                row["failure_code"] = str(error)
        if restore_foreground:
            restored = adapter.restore_foreground(state.get("prior_foreground"))
            state["foreground_restored"] = restored
            if not restored:
                failed = True
                for row in rows.values():
                    if isinstance(row, dict) and not row["cleanup_done"]:
                        row["failure_code"] = "foreground_restore_failed"
        if failed:
            state["cleanup_retry_was_required"] = True
            state["state"] = "blocked"
            _atomic_write_json(path, state)
            raise ProbeBlocked("cleanup_blocked")
        state["state"] = "cleaned"
        _atomic_write_json(path, state)


def reduce_state(path: Path, output: Path) -> dict[str, object]:
    with _state_lock(path):
        state = _load_state(path)
        rows = state["rows"]
        assert isinstance(rows, dict)
        if state["state"] != "cleaned" or set(rows) != ROWS:
            raise ProbeBlocked("reduction_blocked")
        if not state["foreground_restored"] or not all(
            isinstance(row, dict) and row["cleanup_done"]
            and row["graph_verified"] and row["store_verified"]
            and row["observation_state"] == "verified" and row["scenario_binding"] is not None
            and row["scenario_verified"] and row["observation_graph_verified"]
            and row["observation_store_verified"]
            and row["effect_observed"] is (name == "P1")
            for name, row in rows.items()
        ):
            raise ProbeBlocked("reduction_blocked")
        report = {
            "schema_version": 1,
            "scope": "issue-642-default-apk",
            "apk_sha256": state["apk_sha256"],
            "rows": {
                row_name: {
                    "result": "pass",
                    "effect_observed": rows[row_name]["effect_observed"],
                    "revert_write_acknowledged": rows[row_name]["revert_write_acknowledged"],
                    "graph_verified": rows[row_name]["graph_verified"],
                    "store_verified": rows[row_name]["store_verified"],
                    "cleanup_done": rows[row_name]["cleanup_done"],
                    "native_prompt_observed": True,
                    "scenario_verified": rows[row_name]["scenario_verified"],
                    "observation_graph_verified": rows[row_name]["observation_graph_verified"],
                    "observation_store_verified": rows[row_name]["observation_store_verified"],
                }
                for row_name in sorted(ROWS)
            },
            "cleanup_retry_was_required": state["cleanup_retry_was_required"],
            "foreground_restored": state["foreground_restored"],
            "redaction": {
                "account_identity_included": False,
                "item_or_request_reference_included": False,
                "device_identity_included": False,
                "confirmation_authority_included": False,
                "raw_platform_output_included": False,
            },
        }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return report


class AndroidProductAdapter:
    def __init__(self, adb: str = "adb") -> None:
        self.adb = adb

    def _adb(self, *args: str, timeout: int = 20) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.adb, *args], text=True, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, timeout=timeout, check=False,
        )

    def capture_foreground(self) -> str | None:
        result = self._adb("shell", "dumpsys", "window")
        if result.returncode:
            raise ProbeBlocked("foreground_unavailable")
        for line in result.stdout.splitlines():
            if "mCurrentFocus" not in line and "mFocusedApp" not in line:
                continue
            match = re.search(r"([A-Za-z0-9._$]+/[A-Za-z0-9._$]+)", line)
            if match and COMPONENT.fullmatch(match.group(1)):
                return match.group(1)
        raise ProbeBlocked("foreground_unavailable")

    @contextlib.contextmanager
    def _cdp(self) -> Iterator["CdpClient"]:
        sockets = self._adb("shell", "cat", "/proc/net/unix").stdout
        matches = sorted(set(re.findall(r"webview_devtools_remote_[A-Za-z0-9._-]+", sockets)))
        if len(matches) != 1:
            raise ProbeBlocked("webview_target_unavailable")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = int(listener.getsockname()[1])
        forward = f"tcp:{port}"
        if self._adb("forward", forward, f"localabstract:{matches[0]}").returncode:
            raise ProbeBlocked("webview_target_unavailable")
        try:
            with urllib.request.urlopen(
                f"http://127.0.0.1:{port}/json", timeout=5
            ) as response:
                raw = response.read(MAX_CDP_BYTES + 1)
            if len(raw) > MAX_CDP_BYTES:
                raise ProbeBlocked("webview_target_unavailable")
            targets = json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object)
            def is_visible(websocket_url: str) -> bool:
                with CdpClient(websocket_url) as candidate:
                    return candidate.evaluate(
                        "document.visibilityState === 'visible'"
                    ) is True

            page = _select_visible_product_page(targets, is_visible)
            with CdpClient(str(page["webSocketDebuggerUrl"])) as client:
                if client.evaluate("document.visibilityState === 'visible'") is not True:
                    raise ProbeBlocked("webview_target_unavailable")
                yield client
        except (OSError, ValueError, json.JSONDecodeError) as error:
            raise ProbeBlocked("webview_target_unavailable") from error
        finally:
            self._adb("forward", "--remove", forward)

    def _evaluate(self, expression: str) -> object:
        with self._cdp() as client:
            return client.evaluate(expression)

    def resolve_unread_fixture(
        self, row: str, excluded_item_ids: set[str]
    ) -> tuple[str, str]:
        excluded = json.dumps(sorted(excluded_item_ids))
        expression = f"""(async () => {{
          const account = String(App.account || '');
          const excluded = new Set({excluded});
          const item = App.route === 'mail' ? Mail.selected : null;
          const visible = [...document.querySelectorAll('.mail-item.active')]
            .some(node => node.getClientRects().length && node.dataset.id === item?.remote_id);
          if (!visible || !item || item.item_type !== 'message' || !item.remote_id
            || item.preview?.isRead !== false || excluded.has(String(item.remote_id))) return null;
          return item ? {{account, item_id:String(item.remote_id)}} : null;
        }})()"""
        value = self._evaluate(expression)
        if not isinstance(value, dict):
            raise ProbeBlocked("fixture_unavailable")
        account, item_id = value.get("account"), value.get("item_id")
        if not isinstance(account, str) or not account or not isinstance(item_id, str) or not item_id:
            raise ProbeBlocked("fixture_unavailable")
        return account, item_id

    def store_is_read(self, account: str, item_id: str) -> bool:
        expression = f"""(async () => {{
          const account = {json.dumps(account)};
          const itemId = {json.dumps(item_id)};
          const data = await api('/api/v1/items?' + qs({{account, service:'mail', limit:1000}}));
          const item = (data.items || []).find(value => String(value.remote_id) === itemId);
          if (!item) throw new Error('fixture_missing');
          return item.preview?.isRead;
        }})()"""
        value = self._evaluate(expression)
        if not isinstance(value, bool):
            raise ProbeBlocked("store_verification_failed")
        return value

    def graph_is_read(self, account: str, item_id: str) -> bool:
        expression = f"""(async () => {{
          const data = await postJson('/api/v1/mail/read-state', CAP.mailwrite, {{
            request_id: crypto.randomUUID(), account: {json.dumps(account)}, id: {json.dumps(item_id)}
          }});
          return data.is_read;
        }})()"""
        value = self._evaluate(expression)
        if not isinstance(value, bool):
            raise ProbeBlocked("graph_verification_failed")
        return value

    def refresh_store(self) -> bool:
        expression = """(async () => {
          const state = await api('/api/v1/sync/state');
          if (!state.enabled) return 'mobile_periodic';
          await postJson('/api/v1/sync/now', CAP.sync,
            {request_id: crypto.randomUUID()});
          return 'triggered';
        })()"""
        return self._evaluate(expression) in {"mobile_periodic", "triggered"}

    def set_read(self, account: str, item_id: str, is_read: bool) -> bool:
        expression = f"""(async () => {{
          await postJson('/api/v1/mail/read', CAP.mailwrite, {{
            request_id: crypto.randomUUID(), account: {json.dumps(account)},
            id: {json.dumps(item_id)}, is_read: {str(is_read).lower()}
          }});
          return true;
        }})()"""
        return self._evaluate(expression) is True

    def restore_foreground(self, component: str | None) -> bool:
        if component is None:
            return False
        return self._adb("shell", "am", "start", "-n", component).returncode == 0

    def observe_native_prompt(self, account: str) -> dict[str, str]:
        window = self._adb("shell", "dumpsys", "window")
        # Require the focused native system dialog, not an arbitrary mention of
        # keyguard/biometric capability in a background window dump.
        if window.returncode or not any(
            "mCurrentFocus=" in line and re.search(r"BiometricPrompt|BiometricDialog|AuthContainerView|ConfirmDeviceCredential", line)
            for line in window.stdout.splitlines()
        ):
            raise ProbeBlocked("observation_failed")
        value = self._evaluate(f"""(async () => {{
          if (String(App.account) !== {json.dumps(account)} || _bioPending.size !== 1) return null;
          const records = [...AssistantState.pendingCardsById.values()].filter(record =>
            record.status === 'confirming' && record.preview === 'Mark this email as read');
          if (records.length !== 1) return null;
          const record = records[0];
          const activity = await api('/api/v1/activity?' + qs({{account: App.account, limit:500}}));
          if (!Array.isArray(activity.runs)
            || activity.runs.some(run => !Number.isSafeInteger(run.id))) return null;
          return {{pending_id:record.pending_id, session_id:record.session_id, turn_id:record.turn_id,
            audit_watermark:String(Math.max(0, ...activity.runs.map(run => Number(run.id))))}};
        }})()""")
        if not isinstance(value, dict):
            raise ProbeBlocked("observation_failed")
        return value

    def scenario_verified(self, account: str, binding: dict[str, str], approved: bool) -> bool:
        value = self._evaluate(f"""(async () => {{
          const binding = {json.dumps(binding)};
          if (String(App.account) !== {json.dumps(account)} || _bioPending.size !== 0) return false;
          const record = AssistantState.pendingCardsById.get(binding.pending_id);
          if (!record || AssistantState.confirmAttemptsByPendingId.has(binding.pending_id)) return false;
          const approved = {str(approved).lower()};
          if (approved ? record.status !== 'confirmed'
            : record.status !== 'pending' || record.error !== nativeConfirmationMessage('cancelled')) return false;
          const history = await request('GET', '/api/v1/agent/session/history?' + qs({{
            session_id:binding.session_id, limit:100
          }}), {{capToken:CAP.agent}});
          if (!Array.isArray(history.records) || history.refreshing || history.next_cursor) return false;
          const operations = history.records.filter(r => r.turn_id === binding.turn_id)
            .map(r => r.kind && typeof r.kind === 'object' ? r.kind : r)
            .filter(r => r.kind === 'pending_operation' || r.kind === 'operation_state');
          if (!operations.length || operations.at(-1).code !== (approved ? 'completed' : 'confirmation_required')) return false;
          const activity = await api('/api/v1/activity?' + qs({{account:App.account, limit:500}}));
          if (!Array.isArray(activity.runs)
            || activity.runs.some(run => !Number.isSafeInteger(run.id))
            || (activity.runs.length >= 500 && Math.min(...activity.runs.map(run => run.id)) > Number(binding.audit_watermark))) return false;
          const newAudits = activity.runs.filter(run => run.kind === 'audit:agent-confirm'
            && Number(run.id) > Number(binding.audit_watermark));
          return approved ? newAudits.some(run => {{
            try {{return JSON.parse(run.summary).state === 'completed';}} catch (_) {{return false;}}
          }}) : newAudits.length === 0;
        }})()""")
        return value is True


class CdpClient:
    def __init__(self, websocket_url: str) -> None:
        try:
            import websocket
        except ImportError as error:
            raise ProbeBlocked("cdp_client_unavailable") from error
        parsed = urllib.parse.urlparse(websocket_url)
        if parsed.hostname not in {"127.0.0.1", "localhost"}:
            raise ProbeBlocked("webview_target_unavailable")
        self._connection = websocket.create_connection(
            websocket_url, timeout=10, suppress_origin=True
        )
        self._sequence = 0

    def __enter__(self) -> "CdpClient":
        return self

    def __exit__(self, *_args: object) -> None:
        self._connection.close()

    def evaluate(self, expression: str) -> object:
        self._sequence += 1
        request_id = self._sequence
        self._connection.send(json.dumps({
            "id": request_id,
            "method": "Runtime.evaluate",
            "params": {
                "expression": expression,
                "awaitPromise": True,
                "returnByValue": True,
            },
        }))
        while True:
            message = json.loads(self._connection.recv(), object_pairs_hook=_unique_object)
            if message.get("id") != request_id:
                continue
            if "error" in message or message.get("result", {}).get("exceptionDetails"):
                raise ProbeBlocked("product_request_failed")
            result = message.get("result", {}).get("result", {})
            return result.get("value")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=(
        "prepare", "register-fixture", "observe-prompt", "observe", "cleanup", "reduce",
    ))
    parser.add_argument("--state", required=True)
    parser.add_argument("--package")
    parser.add_argument("--apk-sha256")
    parser.add_argument("--row", choices=sorted(ROWS))
    parser.add_argument("--expected", choices=sorted(EXPECTATIONS))
    parser.add_argument("--restore-foreground", action="store_true")
    parser.add_argument("--verify")
    parser.add_argument("--out")
    parser.add_argument("--adb", default="adb")
    return parser


def main() -> int:
    args = _parser().parse_args()
    path = Path(args.state).absolute()
    adapter = AndroidProductAdapter(args.adb)
    try:
        if args.command == "prepare":
            prepare_state(path, args.package or "", args.apk_sha256 or "", adapter)
            status = "prepared"
        elif args.command == "register-fixture":
            register_fixture(path, args.row or "", adapter)
            status = "cleanup_registered"
        elif args.command == "observe-prompt":
            observe_prompt(path, args.row or "", adapter)
            status = "prompt_observed"
        elif args.command == "observe":
            observe(path, args.row or "", args.expected or "", adapter)
            status = "observed"
        elif args.command == "cleanup":
            if args.verify != "graph,store" or not args.restore_foreground:
                raise ProbeBlocked("cleanup_invalid")
            cleanup(path, adapter, True)
            status = "cleaned"
        else:
            if not args.out:
                raise ProbeBlocked("reduction_invalid")
            reduce_state(path, Path(args.out))
            status = "reduced"
        print(status)
        return 0
    except ProbeBlocked:
        print("blocked")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
