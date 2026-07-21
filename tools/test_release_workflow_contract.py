import copy
import unittest
from pathlib import Path

from tools.release_workflow_contract import (
    ContractError,
    EXPECTED_ASSETS,
    candidate_rc_tags,
    classify,
    require_absent_http_status,
    select_matching_rc,
    validate_release_object,
)


SHA = "a" * 40


class ReleaseWorkflowContractTest(unittest.TestCase):
    def test_release_ref_accepts_only_exact_stable_semver_tags(self):
        self.assertEqual(classify("push", "refs/tags/v1.2.3", SHA), ("stable", "v1.2.3"))
        self.assertEqual(classify("push", "refs/tags/v0.0.0", SHA), ("stable", "v0.0.0"))

    def test_release_ref_rejects_rc_beta_and_malformed_tags(self):
        for ref in (
            "refs/tags/v1.2.3-rc.1",
            "refs/tags/v1.2.3-beta",
            "refs/tags/v01.2.3",
            "refs/tags/v1.2",
            "refs/heads/main",
        ):
            with self.subTest(ref=ref), self.assertRaises(ContractError):
                classify("push", ref, SHA)

    def test_workflow_dispatch_requires_main_and_exact_expected_commit(self):
        self.assertEqual(classify("workflow_dispatch", "refs/heads/main", SHA, SHA), ("rc", ""))
        for ref, expected in (("refs/heads/dev", SHA), ("refs/heads/main", "b" * 40), ("refs/heads/main", "short")):
            with self.subTest(ref=ref, expected=expected), self.assertRaises(ContractError):
                classify("workflow_dispatch", ref, SHA, expected)

    def test_workflow_dispatch_requires_main_for_rc(self):
        with self.assertRaises(ContractError):
            classify("workflow_dispatch", "refs/heads/dev", SHA, SHA)

    def test_workflow_dispatch_requires_exact_expected_commit_match_before_build(self):
        for expected in ("b" * 40, "short", None):
            with self.subTest(expected=expected), self.assertRaises(ContractError):
                classify("workflow_dispatch", "refs/heads/main", SHA, expected)

    def test_release_postcondition_rejects_draft_wrong_target_or_missing_assets(self):
        release = {
            "tag_name": "v1.0.0-rc.9",
            "draft": False,
            "prerelease": True,
            "target_commitish": SHA,
            "assets": [{"name": name} for name in EXPECTED_ASSETS],
        }
        validate_release_object(release, "rc", "v1.0.0-rc.9", SHA)
        for field, value in (("draft", True), ("target_commitish", "b" * 40), ("prerelease", False)):
            changed = copy.deepcopy(release)
            changed[field] = value
            with self.subTest(field=field), self.assertRaises(ContractError):
                validate_release_object(changed, "rc", "v1.0.0-rc.9", SHA)
        changed = copy.deepcopy(release)
        changed["assets"].pop()
        with self.assertRaises(ContractError):
            validate_release_object(changed, "rc", "v1.0.0-rc.9", SHA)

    def test_release_postcondition_rejects_draft_or_wrong_target(self):
        release = self._valid_rc_release()
        for field, value in (("draft", True), ("target_commitish", "b" * 40)):
            changed = copy.deepcopy(release)
            changed[field] = value
            with self.subTest(field=field), self.assertRaises(ContractError):
                validate_release_object(changed, "rc", "v1.0.0-rc.9", SHA)

    def test_release_postcondition_requires_expected_assets(self):
        release = self._valid_rc_release()
        release["assets"].pop()
        with self.assertRaises(ContractError):
            validate_release_object(release, "rc", "v1.0.0-rc.9", SHA)

    def test_stable_rc_selection_requires_release_and_peeled_tag_commit(self):
        release = self._valid_rc_release()
        releases = [release]
        self.assertEqual(candidate_rc_tags(releases, SHA), ["v1.0.0-rc.9"])
        self.assertEqual(
            select_matching_rc(releases, {"v1.0.0-rc.9": SHA}, SHA),
            "v1.0.0-rc.9",
        )
        with self.assertRaises(ContractError):
            select_matching_rc(releases, {"v1.0.0-rc.9": "b" * 40}, SHA)

    def test_stable_rc_selection_rejects_target_only_spoof_and_bad_assets(self):
        release = self._valid_rc_release()
        wrong_target = copy.deepcopy(release)
        wrong_target["target_commitish"] = "b" * 40
        self.assertEqual(candidate_rc_tags([wrong_target], SHA), [])
        bad_assets = copy.deepcopy(release)
        bad_assets["assets"] = [{"name": "isyncyou-android-arm64.apk"}]
        self.assertEqual(candidate_rc_tags([bad_assets], SHA), [])
        draft = copy.deepcopy(release)
        draft["draft"] = True
        self.assertEqual(candidate_rc_tags([draft], SHA), [])

    def test_release_validation_rejects_duplicate_assets_and_wrong_tag_class(self):
        release = self._valid_rc_release()
        release["assets"].append(copy.deepcopy(release["assets"][0]))
        with self.assertRaises(ContractError):
            validate_release_object(release, "rc", "v1.0.0-rc.9", SHA)
        with self.assertRaises(ContractError):
            validate_release_object(self._valid_rc_release(), "rc", "v1.0.0", SHA)

    def test_release_preflight_is_dependency_of_all_build_and_publish_jobs(self):
        workflow = Path(".github/workflows/release.yml").read_text()
        self.assertIn("expected_commit:", workflow)
        self.assertIn("needs: [preflight, android-apk]", workflow)
        self.assertIn("needs: [preflight]", workflow)
        self.assertIn("if: needs.preflight.outputs.mode == 'rc'", workflow)
        self.assertIn("if: needs.preflight.outputs.mode == 'stable'", workflow)
        self.assertNotIn("if: startsWith(github.ref, 'refs/tags/v')", workflow)

    def test_rc_and_stable_preflight_run_before_build_and_postconditions_are_required(self):
        workflow = Path(".github/workflows/release.yml").read_text()
        preflight = workflow.index("  preflight:")
        android = workflow.index("  android-apk:")
        build = workflow.index("  build:")
        self.assertLess(preflight, build)
        self.assertLess(preflight, android)
        self.assertIn("require-absent", workflow)
        self.assertIn("release existence check unavailable", workflow)
        self.assertIn("tag existence check unavailable", workflow)
        self.assertIn("stable tag commit is not on main", workflow)
        self.assertIn("matching non-draft RC", workflow)
        self.assertIn("Verify the published release object", workflow)
        self.assertIn("validate-release", workflow)

    def test_rc_preflight_rejects_existing_tag_or_release(self):
        workflow = Path(".github/workflows/release.yml").read_text()
        self.assertIn("require-absent", workflow)
        self.assertIn('--status "$release_status"', workflow)
        self.assertIn('--resource release', workflow)
        self.assertIn('--resource tag', workflow)
        self.assertNotIn('releases/tags/$RELEASE_TAG" >/dev/null 2>&1', workflow)

    def test_release_preflight_accepts_only_authoritative_not_found(self):
        for resource in ("release", "tag"):
            require_absent_http_status(404, resource)
            for status in (0, 200, 401, 403, 429, 500, 503):
                with self.subTest(resource=resource, status=status), self.assertRaises(ContractError):
                    require_absent_http_status(status, resource)

    def test_stable_preflight_requires_peeled_main_tag_matching_rc_and_no_release(self):
        workflow = Path(".github/workflows/release.yml").read_text()
        self.assertIn("stable tag commit is not on main", workflow)
        self.assertIn("matching non-draft RC", workflow)
        self.assertIn("candidate-rc-tags", workflow)
        self.assertIn("select-rc", workflow)
        self.assertIn('git rev-list -n1 "$rc"', workflow)
        self.assertIn('--resource release', workflow)

    @staticmethod
    def _valid_rc_release():
        return {
            "tag_name": "v1.0.0-rc.9",
            "draft": False,
            "prerelease": True,
            "target_commitish": SHA,
            "assets": [{"name": name} for name in EXPECTED_ASSETS],
        }


if __name__ == "__main__":
    unittest.main()
