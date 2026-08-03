#!/usr/bin/env python3
"""Contract tests for public feedback routing and external PR rejection."""

from __future__ import annotations

import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "external-pull-request-policy.yml"


class ExternalContributionPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_privileged_workflow_is_metadata_only(self) -> None:
        self.assertIn("pull_request_target:", self.workflow)
        self.assertIn("types: [opened, reopened]", self.workflow)
        self.assertIn("permissions: {}", self.workflow)
        self.assertIn("issues: write", self.workflow)
        self.assertIn("pull-requests: write", self.workflow)
        self.assertIn("timeout-minutes: 5", self.workflow)

        forbidden = (
            "uses:",
            "actions/checkout",
            "github.event.pull_request.head.ref",
            "github.event.pull_request.title",
            "github.event.pull_request.body",
            "secrets.",
        )
        for value in forbidden:
            with self.subTest(value=value):
                self.assertNotIn(value, self.workflow)

    def test_only_reviewed_actor_classes_bypass_auto_close(self) -> None:
        for association in ("OWNER", "MEMBER", "COLLABORATOR"):
            with self.subTest(association=association):
                self.assertIn(
                    f"author_association != '{association}'", self.workflow
                )

        self.assertIn("user.login != 'dependabot[bot]'", self.workflow)
        self.assertIn(
            "head.repo.full_name != github.repository", self.workflow
        )
        self.assertNotIn("github-actions[bot]", self.workflow)
        self.assertNotIn("head.ref", self.workflow)

    def test_closed_message_routes_public_and_security_feedback(self) -> None:
        self.assertIn(
            "https://github.com/silentspike/isyncyou-feedback/issues/new/choose",
            self.workflow,
        )
        self.assertIn(
            "https://github.com/silentspike/isyncyou/security/advisories/new",
            self.workflow,
        )
        self.assertIn('"repos/${REPOSITORY}/pulls/${PR_NUMBER}"', self.workflow)
        self.assertIn("--raw-field state=closed", self.workflow)
        self.assertLess(
            self.workflow.index('"repos/${REPOSITORY}/pulls/${PR_NUMBER}"'),
            self.workflow.index('"repos/${REPOSITORY}/issues/${PR_NUMBER}/comments"'),
        )

    def test_public_docs_do_not_invite_source_pull_requests(self) -> None:
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        contributing = (ROOT / "CONTRIBUTING.md").read_text(encoding="utf-8")
        chooser = (
            ROOT / ".github" / "ISSUE_TEMPLATE" / "config.yml"
        ).read_text(encoding="utf-8")

        self.assertNotIn("Issues and PRs are welcome", readme)
        self.assertIn("isyncyou-feedback", readme)
        self.assertIn("isyncyou-feedback", contributing)
        self.assertIn("collaborator-only engineering", contributing)
        self.assertIn("isyncyou-feedback", chooser)
        self.assertIn("security/advisories/new", chooser)


if __name__ == "__main__":
    unittest.main()
