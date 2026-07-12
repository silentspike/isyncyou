#!/usr/bin/env python3
"""Static product-build boundary checks for issue #627."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]


class AgentExperimentalBoundaryTest(unittest.TestCase):
    def test_release_build_excludes_agent_subscription_experimental(self) -> None:
        workflows = (
            ".github/workflows/release.yml",
            ".github/workflows/pr-staging.yml",
            ".github/workflows/pr-main.yml",
        )
        for relative in workflows:
            source = (ROOT / relative).read_text(encoding="utf-8")
            self.assertNotIn("agent-subscription-experimental", source, relative)
            self.assertNotIn("--all-features", source, relative)

        release = (ROOT / ".github/workflows/release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("cargo build --release --workspace", release)
        self.assertIn("./gradlew :app:assembleRelease", release)


if __name__ == "__main__":
    unittest.main()
