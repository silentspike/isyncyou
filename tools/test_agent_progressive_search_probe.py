from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("agent-progressive-search-probe.py")
SPEC = importlib.util.spec_from_file_location("agent_progressive_search_probe", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
probe = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = probe
SPEC.loader.exec_module(probe)

ACTIVITY = "abcdefghijklmnopqrstuv"
RESULT = "zyxwvutsrqponmlkjihgfe"


def stage(
    name: str,
    status: str,
    *,
    scanned: int = 1,
    hits: int = 1,
    coverage: bool | None = None,
    budget: bool | None = None,
    continuation: bool | None = None,
) -> dict[str, object]:
    return {
        "event": "stage_progress",
        "schema_version": 1,
        "activity_id": ACTIVITY,
        "activity_kind": "archive_search",
        "stage": name,
        "status": status,
        "scanned": scanned,
        "total": None,
        "hits": hits,
        "current_item": "synthetic fixture",
        "coverage_complete": coverage,
        "budget_reached": budget,
        "continuation_available": continuation,
    }


def partial(sequence: int = 0) -> dict[str, object]:
    return {
        "event": "partial_result",
        "schema_version": 1,
        "activity_id": ACTIVITY,
        "stage": "deep",
        "sequence": sequence,
        "items": [
            {
                "result_key": RESULT,
                "change": "add",
                "service": "mail",
                "item_id": "synthetic-item",
                "name": "Private title must not enter report",
                "item_type": "message",
                "display_path": "Inbox/Synthetic",
                "sender": "private@example.invalid",
                "body_available": True,
                "source": {
                    "service": "mail",
                    "item_id": "synthetic-item",
                    "label": "Private title must not enter report",
                },
            }
        ],
    }


def events() -> list[dict[str, object]]:
    return [
        stage("names", "queued", scanned=0, hits=0),
        stage("bodies", "queued", scanned=0, hits=0),
        stage("deep", "queued", scanned=0, hits=0),
        stage("names", "running"),
        stage("names", "complete"),
        stage("bodies", "running"),
        stage("bodies", "complete"),
        stage("deep", "running"),
        partial(),
        stage(
            "deep",
            "complete",
            coverage=True,
            budget=False,
            continuation=False,
        ),
        {"event": "token", "text": "private answer"},
        {"event": "done", "reason": "complete"},
    ]


class ProgressiveSearchProbeTest(unittest.TestCase):
    def test_ordered_fixture_reduces_to_closed_redacted_facts(self) -> None:
        _reducer, report = probe.reduce_events(events(), lambda _service, _item: True)
        self.assertEqual(report["terminal_reason"], "complete")
        self.assertEqual(report["activity_count"], 1)
        self.assertEqual(report["result_count"], 1)
        self.assertTrue(report["all_sources_resolved"])
        encoded = json.dumps(report)
        for private in [
            "private answer",
            "private@example.invalid",
            "Private body excerpt",
            "synthetic-item",
            "Inbox/Synthetic",
        ]:
            self.assertNotIn(private, encoded)
        self.assertIn("sha256:", encoded)

    def test_scripted_fake_provider_selection_requires_observed_result(self) -> None:
        document = {
            "schema_version": 1,
            "fixture_kind": "synthetic",
            "provider_script": {
                "selected_result_keys": [RESULT],
            },
            "events": events(),
            "source_resolution": {RESULT: True},
        }
        report = probe.validate_scripted_fixture(document)
        self.assertEqual(report["scripted_selection_count"], 1)
        self.assertTrue(report["keywordless_selection_observed"])
        document["provider_script"]["selected_result_keys"] = [ACTIVITY]
        with self.assertRaisesRegex(probe.ProbeError, "scripted_selection_missing"):
            probe.validate_scripted_fixture(document)

    def test_conflicting_old_partial_replay_is_rejected(self) -> None:
        replay = partial()
        conflicting = partial()
        conflicting["items"][0]["item_type"] = "different"
        fixture = events()
        fixture.insert(9, replay)
        _reducer, report = probe.reduce_events(fixture)
        self.assertEqual(report["replay_count"], 1)
        fixture[9] = conflicting
        with self.assertRaisesRegex(probe.ProbeError, "partial_replay_conflict"):
            probe.reduce_events(fixture)

    def test_later_stage_before_prior_terminal_is_rejected(self) -> None:
        invalid = [
            stage("names", "queued"),
            stage("bodies", "queued"),
            stage("deep", "queued"),
            stage("names", "running"),
            stage("bodies", "running"),
        ]
        with self.assertRaisesRegex(probe.ProbeError, "stage_order_invalid"):
            probe.reduce_events(invalid)

    def test_failed_stage_allows_later_stages_to_close_skipped(self) -> None:
        fixture = [
            stage("names", "queued"),
            stage("bodies", "queued"),
            stage("deep", "queued"),
            stage("names", "running"),
            stage("names", "failed"),
            stage("bodies", "skipped"),
            stage("deep", "skipped"),
            {"event": "done", "reason": "error"},
        ]
        _reducer, report = probe.reduce_events(fixture)
        self.assertEqual(report["terminal_reason"], "error")
        self.assertEqual([row["status"] for row in report["stage_updates"][-3:]], [
            "failed",
            "skipped",
            "skipped",
        ])

    def test_cancelled_stage_allows_later_stages_to_close_cancelled(self) -> None:
        fixture = [
            stage("names", "queued"),
            stage("bodies", "queued"),
            stage("deep", "queued"),
            stage("names", "running"),
            stage("names", "cancelled"),
            stage("bodies", "cancelled"),
            stage("deep", "cancelled"),
            {"event": "done", "reason": "cancelled"},
        ]
        _reducer, report = probe.reduce_events(fixture)
        self.assertEqual(report["terminal_reason"], "cancelled")

    def test_model_private_progress_member_is_rejected(self) -> None:
        invalid = stage("names", "queued")
        invalid["continuation"] = "private"
        with self.assertRaisesRegex(probe.ProbeError, "private_projection_observed"):
            probe.reduce_events([invalid])

    def test_fixture_mode_writes_no_private_values(self) -> None:
        document = {
            "schema_version": 1,
            "fixture_kind": "synthetic",
            "provider_script": {
                "selected_result_keys": [RESULT],
            },
            "events": events(),
            "source_resolution": {RESULT: True},
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "fixture.json"
            output = root / "report.json"
            fixture.write_text(json.dumps(document), encoding="utf-8")
            status = probe.main(
                [
                    "fixture",
                    "--implementation-commit",
                    "a" * 40,
                    "--fixture",
                    str(fixture),
                    "--out",
                    str(output),
                ]
            )
            self.assertEqual(status, 0)
            encoded = output.read_text(encoding="utf-8")
            self.assertIn('"state": "pass"', encoded)
            self.assertNotIn("private@example.invalid", encoded)
            self.assertNotIn("Private body excerpt", encoded)
            self.assertNotIn("synthetic-item", encoded)


if __name__ == "__main__":
    unittest.main()
