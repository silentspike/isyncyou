import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("verify-agent-account-lifecycle-boundary.py")
SPEC = importlib.util.spec_from_file_location("verify_agent_account_lifecycle_boundary", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


def fixture(root: Path, *, status: str = "let value = 1;", default_hook: bool = False) -> None:
    default = '["agent-account-lifecycle-device-test-hooks"]' if default_hook else "[]"
    files = {
        "crates/app-host/Cargo.toml": f"default = {default}\n",
        "crates/mobile/Cargo.toml": "default = []\n",
        "android/app/build.gradle.kts": (
            'val allowedCargoTestFeatures = setOf("agent-account-lifecycle-device-test-hooks")\n'
        ),
        "crates/app-host/src/lib.rs": (
            f"fn status_json(&self) -> String {{ {status} String::new() }}\n"
            "fn delete_provider_product_state_durable() { let provider = 1; }\n"
        ),
        "gui/webui/src/lib.rs": "fn router() {}\n",
        "gui/webui/src/app.js": "const state = lifecycle.code;\n",
        "README.md": "# Product\n",
        ".github/workflows/release.yml": "name: release\n",
    }
    for relative, content in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


class AgentAccountLifecycleBoundaryTest(unittest.TestCase):
    def test_clean_fixture_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture(root)
            self.assertTrue(all(check.passed for check in MODULE.scan(root)))

    def test_mutated_default_feature_and_status_write_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture(root, status="repository.put(value);", default_hook=True)
            failed = {check.name for check in MODULE.scan(root) if not check.passed}
            self.assertIn("app_host_default_excludes_hook", failed)
            self.assertIn("status_is_observational", failed)

    def test_local_delete_route_and_graph_cleanup_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture(root)
            (root / "gui/webui/src/lib.rs").write_text(
                'route("/api/v1/agent/credential/delete");\n', encoding="utf-8"
            )
            (root / "crates/app-host/src/lib.rs").write_text(
                "fn status_json(&self) -> String { String::new() }\n"
                "fn delete_provider_product_state_durable() { delete(MicrosoftGraph); }\n",
                encoding="utf-8",
            )
            failed = {check.name for check in MODULE.scan(root) if not check.passed}
            self.assertIn("product_router_has_no_local_only_delete_route", failed)
            self.assertIn("cleanup_excludes_graph_and_m365_credentials", failed)


if __name__ == "__main__":
    unittest.main()
