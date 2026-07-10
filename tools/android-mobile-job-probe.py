#!/usr/bin/env python3
"""State-safe ADB probe for #626 mobile-job recovery evidence.

The probe records only bounded booleans and the caller's requested state. It never
prints connectivity dumps, notification dumps, account data, or native responses.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


class ProbeError(RuntimeError):
    pass


def run(argv: list[str], check: bool = True) -> str:
    result = subprocess.run(argv, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and result.returncode != 0:
        raise ProbeError(f"command failed: {' '.join(argv)}")
    return result.stdout


def adb(adb_path: str, *args: str, check: bool = True) -> str:
    return run([adb_path, *args], check=check)


def shell(adb_path: str, command: str, check: bool = True) -> str:
    return adb(adb_path, "shell", command, check=check)


def setting(adb_path: str, name: str) -> str:
    return shell(adb_path, f"settings get global {name}", check=False).strip()


def bool_setting(value: str) -> bool | None:
    if value in {"1", "true", "on"}:
        return True
    if value in {"0", "false", "off"}:
        return False
    return None


def validated_network(adb_path: str) -> bool:
    text = shell(adb_path, "dumpsys connectivity", check=False)
    # Keep the raw dump in memory only. This is deliberately a conservative proof:
    # any explicit VALIDATED network is true; otherwise the result is false.
    return bool(re.search(r"\bVALIDATED\b", text))


def snapshot(adb_path: str, package: str) -> dict[str, object]:
    wifi = shell(adb_path, "cmd wifi status", check=False)
    data = shell(adb_path, "cmd phone data get", check=False)
    permission = shell(adb_path, f"cmd appops get {package} POST_NOTIFICATION", check=False)
    channel = shell(adb_path, "dumpsys notification", check=False)
    return {
        "serial": adb(adb_path, "get-serialno", check=False).strip(),
        "package": package,
        "airplane_mode": bool_setting(setting(adb_path, "airplane_mode_on")),
        "wifi_enabled": bool(re.search(r"Wi-Fi is (?:enabled|on)", wifi, re.I)),
        "mobile_data_enabled": not bool(re.search(r"disabled", data, re.I)),
        "stay_awake": setting(adb_path, "stay_on_while_plugged_in"),
        "notifications_allowed": not bool(re.search(r"ignore|deny", permission, re.I)),
        "mobile_jobs_channel_present": "mobile_jobs" in channel,
        "validated_network": validated_network(adb_path),
    }


def write_state(path: Path, state: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(state, sort_keys=True, indent=2) + "\n", encoding="utf-8")


def read_state(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or "serial" not in value:
        raise ProbeError("invalid state file")
    return value


def set_bool(adb_path: str, command: str, value: object) -> None:
    if value is True:
        shell(adb_path, command + " enable", check=False)
    elif value is False:
        shell(adb_path, command + " disable", check=False)


def restore_state(adb_path: str, state: dict[str, object]) -> None:
    airplane = state.get("airplane_mode")
    shell(adb_path, f"cmd connectivity airplane-mode {'enable' if airplane else 'disable'}", check=False)
    set_bool(adb_path, "cmd wifi", state.get("wifi_enabled"))
    set_bool(adb_path, "svc data", state.get("mobile_data_enabled"))
    stay = state.get("stay_awake")
    if isinstance(stay, str) and stay.isdigit():
        shell(adb_path, f"settings put global stay_on_while_plugged_in {stay}", check=False)


def enter_network_loss(adb_path: str, state_path: Path, package: str) -> None:
    state = read_state(state_path)
    shell(adb_path, "cmd connectivity airplane-mode enable")
    shell(adb_path, "cmd wifi disable", check=False)
    shell(adb_path, "svc data disable", check=False)
    if validated_network(adb_path):
        restore_state(adb_path, state)
        raise ProbeError("validated network still present after network-loss setup")


def lock(args: argparse.Namespace) -> bool:
    if args.skip_lock:
        return False
    run(["device-lock", "acquire", args.lock])
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("snapshot-state", "enter-network-loss", "restore-network", "restore-state"))
    parser.add_argument("--adb", default="adb")
    parser.add_argument("--package", default="com.silentspike.isyncyou.debug")
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--lock", default="ag-626")
    parser.add_argument("--skip-lock", action="store_true")
    args = parser.parse_args()
    owns_lock = False
    try:
        owns_lock = lock(args)
        if args.command == "snapshot-state":
            write_state(args.output or args.state, snapshot(args.adb, args.package))
        elif args.command == "enter-network-loss":
            enter_network_loss(args.adb, args.state, args.package)
        else:
            restore_state(args.adb, read_state(args.state))
        return 0
    except (OSError, ProbeError, json.JSONDecodeError) as error:
        print(f"probe failed: {error}", file=sys.stderr)
        return 1
    finally:
        if owns_lock:
            subprocess.run(["device-lock", "release", args.lock], check=False)


if __name__ == "__main__":
    raise SystemExit(main())
