import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("verify-agent-network-boundary.py")
SPEC = importlib.util.spec_from_file_location("verify_agent_network_boundary", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class AgentNetworkBoundaryTest(unittest.TestCase):
    def test_scanner_detects_default_feature_and_unsafe_callback_writer(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            files = {
                "crates/mobile/Cargo.toml": 'default = ["agent-network-device-test-hooks"]\n',
                "crates/app-host/Cargo.toml": 'default = []\n',
                "android/app/build.gradle.kts": 'val allowedCargoTestFeatures = setOf("agent-network-device-test-hooks")\n',
                "gui/webui/src/app.js": 'const CONNECTIVITY_COPY = {};\n',
                "crates/app-host/src/lib.rs": 'const CODEX_CALLBACK_DIAGNOSTICS_FILE: &str = "codex-debug.txt";\nwrite(CODEX_CALLBACK_DIAGNOSTICS_FILE);\n',
                ".github/workflows/release.yml": "name: release\n",
            }
            for name, content in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
            failed = {c.name for c in MODULE.scan(root) if not c.passed}
            self.assertIn("mobile_default_excludes_hook", failed)
            self.assertIn("callback_debug_file_is_cleanup_only", failed)
            self.assertIn("assistant_has_closed_settings_action", failed)


if __name__ == "__main__":
    unittest.main()
