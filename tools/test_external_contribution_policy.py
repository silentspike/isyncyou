#!/usr/bin/env python3
"""Contract tests for public feedback routing and external PR rejection."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import textwrap
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

    def run_policy(self, metadata: dict, api_status: int = 0) -> tuple[int, list]:
        """Execute the actual workflow shell with real jq and isolated GitHub I/O."""
        script = textwrap.dedent(self.workflow.split("        run: |\n", 1)[1])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
            fake_gh = root / "gh"
            fake_gh.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, pathlib, sys\n"
                "root = pathlib.Path(os.environ['POLICY_FIXTURE'])\n"
                "with (root / 'calls.jsonl').open('a') as calls:\n"
                "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if sys.argv[1:] == ['api', 'repos/example/project/pulls/123']:\n"
                "    if int(os.environ['POLICY_API_STATUS']):\n"
                "        sys.exit(int(os.environ['POLICY_API_STATUS']))\n"
                "    print((root / 'metadata.json').read_text())\n"
                "elif sys.argv[1:4] not in (['api', '--method', 'PATCH'], ['api', '--method', 'POST']):\n"
                "    sys.exit(99)\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o700)
            env = {
                **os.environ,
                "PATH": f"{root}:{os.environ['PATH']}",
                "POLICY_FIXTURE": str(root),
                "POLICY_API_STATUS": str(api_status),
                "REPOSITORY": "example/project",
                "PR_NUMBER": "123",
            }
            result = subprocess.run(
                ["bash", "-c", script], env=env, capture_output=True,
                text=True, timeout=10, check=False,
            )
            calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
            return result.returncode, calls

    @staticmethod
    def metadata(
        association: str = "NONE", login: str = "external-user",
        head_repo: str = "example/fork",
    ) -> dict:
        return {
            "number": 123,
            "base": {"repo": {"full_name": "example/project"}},
            "head": {"repo": {"full_name": head_repo}},
            "user": {"login": login},
            "author_association": association,
        }

    def test_current_allowed_associations_prevent_stale_event_closure(self) -> None:
        for association in ("OWNER", "MEMBER", "COLLABORATOR"):
            with self.subTest(association=association):
                code, calls = self.run_policy(self.metadata(association))
                self.assertEqual(code, 0)
                self.assertEqual(len(calls), 1)

    def test_current_external_associations_are_closed_and_explained(self) -> None:
        for association in ("NONE", "CONTRIBUTOR", "FIRST_TIMER", "FIRST_TIME_CONTRIBUTOR", "MANNEQUIN"):
            with self.subTest(association=association):
                code, calls = self.run_policy(self.metadata(association))
                self.assertEqual(code, 0)
                self.assertEqual(len(calls), 3)
                self.assertEqual(calls[1][:4], ["api", "--method", "PATCH", "repos/example/project/pulls/123"])
                self.assertIn("state=closed", calls[1])
                self.assertEqual(calls[2][:4], ["api", "--method", "POST", "repos/example/project/issues/123/comments"])

    def test_dependabot_allowance_requires_same_repository(self) -> None:
        for repository, expected_calls in (("example/project", 1), ("example/fork", 3)):
            with self.subTest(repository=repository):
                code, calls = self.run_policy(self.metadata(login="dependabot[bot]", head_repo=repository))
                self.assertEqual(code, 0)
                self.assertEqual(len(calls), expected_calls)

    def test_api_failure_stops_before_mutation(self) -> None:
        code, calls = self.run_policy(self.metadata(), api_status=1)
        self.assertNotEqual(code, 0)
        self.assertEqual(len(calls), 1)

    def test_invalid_metadata_stops_before_mutation(self) -> None:
        for mutation in ({}, {"number": 456}, {"author_association": None},
                         {"author_association": "unexpected"}, {"user": {"login": ""}},
                         {"base": {"repo": {"full_name": "example/other"}}}):
            with self.subTest(mutation=mutation):
                metadata = {**self.metadata(), **mutation} if mutation else {}
                code, calls = self.run_policy(metadata)
                self.assertNotEqual(code, 0)
                self.assertEqual(len(calls), 1)


if __name__ == "__main__":
    unittest.main()
