import importlib.util
import json
import os
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from subprocess import CompletedProcess


MODULE_PATH = Path(__file__).with_name("agent-account-lifecycle-device-probe.py")
SPEC = importlib.util.spec_from_file_location("agent_account_lifecycle_device_probe", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class FakeRunner:
    def run(self, *args, timeout=20):
        if args == ("get-state",):
            return CompletedProcess(args, 0, "device\n", "")
        if args[:3] == ("shell", "pm", "path"):
            return CompletedProcess(args, 0, "package:/sensitive/device/path/base.apk\n", "")
        if args[:2] == ("shell", "pidof"):
            return CompletedProcess(args, 0, "9876\n", "")
        if args[:4] == ("shell", "dumpsys", "activity", "services"):
            return CompletedProcess(
                args, 0,
                "serial=private ServiceRecord NetworkCriticalGuardService reason=credential_revoke",
                "",
            )
        return CompletedProcess(args, 1, "", "")


class AgentAccountLifecycleDeviceProbeTest(unittest.TestCase):
    def make_apk(self, root: Path, marker: bool) -> Path:
        apk = root / "app.apk"
        body = b"native" + (MODULE.HOOK_MARKER if marker else b"")
        with zipfile.ZipFile(apk, "w") as archive:
            archive.writestr("lib/arm64-v8a/libisyncyou_mobile.so", body)
        return apk

    def write_observation(self, root: Path, **overrides) -> Path:
        value = {
            "provider": "codex",
            "operation": "switch",
            "result": "pass",
            "initial_state": "connected",
            "final_state": "connected",
            "server_revoke_2xx": True,
            "old_generation_cleaned": True,
            "new_generation_ready": True,
            "post_turn_completed": True,
            "same_account_rejected": False,
            "oauth_guard_ended_before_revoke_guard": True,
            "credential_revoke_guard_observed": True,
            "candidate_retained_when_outcome_unknown": False,
            "hook_checkpoint": None,
        }
        value.update(overrides)
        path = root / "observation.json"
        path.write_text(json.dumps(value), encoding="utf-8")
        os.chmod(path, 0o600)
        return path

    def test_report_is_redacted_and_scope_bound(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            root = Path(tmp)
            report = MODULE.collect(
                FakeRunner(), self.make_apk(root, marker=True), "hook",
                self.write_observation(root),
            )
            self.assertTrue(report["marker_matches_scope"])
            self.assertTrue(report["credential_revoke_guard_running"])
            rendered = json.dumps(report)
            for forbidden in ["sensitive", "9876", "serial=private", "access_token", "email"]:
                self.assertNotIn(forbidden, rendered)
            self.assertFalse(report["redaction"]["serial_included"])

    def test_default_scope_rejects_hook_marker(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            apk = self.make_apk(Path(tmp), marker=True)
            report = MODULE.collect(FakeRunner(), apk, "default")
            self.assertFalse(report["marker_matches_scope"])

    def test_observation_rejects_unknown_and_duplicate_fields(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            root = Path(tmp)
            unknown = self.write_observation(root, account_email="private@example.invalid")
            with self.assertRaisesRegex(ValueError, "unsupported fields"):
                MODULE._load_observation(unknown)
            unknown.write_text('{"provider":"codex","provider":"claude"}', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate fields"):
                MODULE._load_observation(unknown)

    def test_observation_rejects_symlink_even_when_target_is_in_tmp(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            root = Path(tmp)
            target = self.write_observation(root)
            link = root / "observation-link.json"
            link.symlink_to(target)
            with self.assertRaisesRegex(ValueError, "non-symlink"):
                MODULE._load_observation(link)


if __name__ == "__main__":
    unittest.main()
