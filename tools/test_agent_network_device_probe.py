import importlib.util
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from subprocess import CompletedProcess


MODULE_PATH = Path(__file__).with_name("agent-network-device-probe.py")
SPEC = importlib.util.spec_from_file_location("agent_network_device_probe", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class FakeRunner:
    def run(self, *args, timeout=20):
        if args == ("get-state",):
            return CompletedProcess(args, 0, "device\n", "")
        if args[:3] == ("shell", "pm", "path"):
            return CompletedProcess(args, 0, "package:/redacted/base.apk\n", "")
        if args[:2] == ("shell", "pidof"):
            return CompletedProcess(args, 0, "123\n", "")
        if args[:4] == ("shell", "dumpsys", "activity", "services"):
            return CompletedProcess(args, 0, "ServiceRecord NetworkCriticalGuardService", "")
        return CompletedProcess(args, 1, "", "")


class AgentNetworkDeviceProbeTest(unittest.TestCase):
    def make_apk(self, root: Path, marker: bool) -> Path:
        apk = root / "app.apk"
        body = b"native" + (MODULE.HOOK_MARKER if marker else b"")
        with zipfile.ZipFile(apk, "w") as archive:
            archive.writestr("lib/arm64-v8a/libisyncyou_mobile.so", body)
        return apk

    def test_report_is_redacted_and_scope_bound(self):
        with tempfile.TemporaryDirectory() as tmp:
            apk = self.make_apk(Path(tmp), marker=True)
            report = MODULE.collect(FakeRunner(), apk, "hook")
            self.assertTrue(report["marker_matches_scope"])
            self.assertTrue(report["network_guard_service_running"])
            rendered = str(report)
            self.assertNotIn("package:/redacted", rendered)
            self.assertNotIn("123", rendered)
            self.assertFalse(report["redaction"]["serial_included"])


if __name__ == "__main__":
    unittest.main()
