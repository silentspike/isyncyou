import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).with_name("agent-authorization-policy-device-probe.py")
SPEC = importlib.util.spec_from_file_location("agent_authorization_policy_device_probe", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class FakeAdapter:
    def __init__(self) -> None:
        self.graph_items = {"private-item-p1": False, "private-item-p2": False}
        self.store_items = dict(self.graph_items)
        self.calls = []
        self.restore_ok = True
        self.set_ok = True
        self.scenario_ok = True
        self.prompt_open = True
        self.prompt_sequence = 0

    def capture_foreground(self):
        self.calls.append(("capture_foreground",))
        return "private.package/private.Activity"

    def resolve_unread_fixture(self, row, excluded_item_ids):
        item_id = "private-item-p1" if row == "P1" else "private-item-p2"
        if item_id in excluded_item_ids or self.store_items[item_id]:
            raise MODULE.ProbeBlocked("fixture_unavailable")
        self.calls.append(("resolve", row))
        return "private-account", item_id

    def store_is_read(self, account, item_id):
        self.calls.append(("store_read", item_id))
        return self.store_items[item_id]

    def graph_is_read(self, account, item_id):
        self.calls.append(("graph_read", item_id))
        return self.graph_items[item_id]

    def refresh_store(self):
        self.calls.append(("refresh_store",))
        self.store_items = dict(self.graph_items)
        return True

    def set_read(self, account, item_id, is_read):
        self.calls.append(("set_read", item_id, is_read))
        if not self.set_ok:
            return False
        self.graph_items[item_id] = is_read
        return True

    def restore_foreground(self, component):
        self.calls.append(("restore_foreground", component))
        return self.restore_ok

    def observe_native_prompt(self, account):
        if not self.prompt_open:
            raise MODULE.ProbeBlocked("observation_failed")
        self.prompt_sequence += 1
        return {"pending_id": f"synthetic-pending-{self.prompt_sequence}", "session_id": "synthetic-session",
                "turn_id": f"synthetic-turn-{self.prompt_sequence}", "audit_watermark": "0"}

    def scenario_verified(self, account, binding, approved):
        return self.scenario_ok


class AgentAuthorizationPolicyDeviceProbeTest(unittest.TestCase):
    APK_HASH = "a" * 64

    def state_path(self, root):
        path = Path(root) / "state.json"
        path.touch(mode=0o600)
        os.chmod(path, 0o600)
        return path

    def prepare(self, root, adapter):
        path = self.state_path(root)
        MODULE.prepare_state(path, MODULE.PACKAGE, self.APK_HASH, adapter)
        return path

    def register_both(self, path, adapter):
        MODULE.register_fixture(path, "P1", adapter)
        MODULE.register_fixture(path, "P2", adapter)

    def observe(self, path, row, expected, adapter):
        MODULE.observe_prompt(path, row, adapter)
        MODULE.observe(path, row, expected, adapter)

    def test_device_probe_records_revert_intent_before_mutation_submission(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            MODULE.register_fixture(path, "P1", adapter)
            state = MODULE._load_state(path)
            row = state["rows"]["P1"]
            self.assertEqual(row["phase"], "cleanup_registered")
            self.assertTrue(row["revert_required"])
            self.assertFalse(any(call[0] == "set_read" for call in adapter.calls))

    def test_device_probe_state_transition_is_atomic_bounded_and_crash_safe(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            before = path.read_bytes()
            state = MODULE._load_state(path)
            state["state"] = "active"
            with self.assertRaisesRegex(RuntimeError, "injected_before_replace"):
                MODULE._atomic_write_json(path, state, fail_at="before_replace")
            self.assertEqual(path.read_bytes(), before)
            with self.assertRaisesRegex(RuntimeError, "injected_after_replace"):
                MODULE._atomic_write_json(path, state, fail_at="after_replace")
            self.assertEqual(MODULE._load_state(path)["state"], "active")
            MODULE.register_fixture(path, "P1", adapter)
            state = MODULE._load_state(path)
            state["rows"]["P1"]["account"] = "x" * 70000
            with self.assertRaisesRegex(MODULE.ProbeBlocked, "state_size_invalid"):
                MODULE._atomic_write_json(path, state)

    def test_device_probe_cleanup_reverts_and_graph_store_verifies_before_success(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            adapter.graph_items["private-item-p1"] = True
            adapter.refresh_store()
            self.observe(path, "P1", "read", adapter)
            self.observe(path, "P2", "unread", adapter)
            MODULE.cleanup(path, adapter, True)
            state = MODULE._load_state(path)
            self.assertEqual(state["state"], "cleaned")
            self.assertFalse(adapter.graph_items["private-item-p1"])
            self.assertFalse(adapter.store_items["private-item-p1"])
            self.assertTrue(state["rows"]["P1"]["revert_write_acknowledged"])
            self.assertTrue(all(row["graph_verified"] for row in state["rows"].values()))
            self.assertTrue(all(row["store_verified"] for row in state["rows"].values()))

    def test_device_probe_cleanup_failure_retains_owner_state_and_returns_blocked(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            MODULE.register_fixture(path, "P1", adapter)
            adapter.graph_items["private-item-p1"] = True
            adapter.refresh_store()
            adapter.set_ok = False
            with self.assertRaisesRegex(MODULE.ProbeBlocked, "cleanup_blocked"):
                MODULE.cleanup(path, adapter, True)
            state = MODULE._load_state(path)
            self.assertEqual(state["state"], "blocked")
            self.assertTrue(state["cleanup_retry_was_required"])
            self.assertEqual(state["rows"]["P1"]["failure_code"], "revert_failed")
            self.assertIn("private-item-p1", path.read_text(encoding="utf-8"))

    def test_device_probe_restores_prior_foreground_without_unconditional_force_stop(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            MODULE.register_fixture(path, "P2", adapter)
            self.observe(path, "P2", "unread", adapter)
            MODULE.cleanup(path, adapter, True)
            self.assertIn(
                ("restore_foreground", "private.package/private.Activity"), adapter.calls
            )
            self.assertFalse(any("force-stop" in call for call in adapter.calls))

    def test_android_adapter_captures_focus_from_supported_window_dump(self):
        completed = MODULE.subprocess.CompletedProcess(
            args=["adb"], returncode=0,
            stdout="mCurrentFocus=Window{1 u0 private.package/private.Activity}\n",
            stderr="",
        )
        adapter = MODULE.AndroidProductAdapter()
        with mock.patch.object(adapter, "_adb", return_value=completed) as adb_call:
            self.assertEqual(
                adapter.capture_foreground(), "private.package/private.Activity"
            )
        adb_call.assert_called_once_with("shell", "dumpsys", "window")

    def test_android_adapter_selects_exactly_one_visible_product_page(self):
        targets = [
            {
                "type": "page",
                "url": "https://appassets.androidplatform.net/",
                "webSocketDebuggerUrl": "ws://hidden",
            },
            {
                "type": "page",
                "url": "https://appassets.androidplatform.net/",
                "webSocketDebuggerUrl": "ws://visible",
            },
        ]

        selected = MODULE._select_visible_product_page(
            targets, lambda url: url == "ws://visible"
        )

        self.assertEqual(selected["webSocketDebuggerUrl"], "ws://visible")

    def test_android_adapter_rejects_ambiguous_visible_product_pages(self):
        targets = [
            {
                "type": "page",
                "url": "https://appassets.androidplatform.net/",
                "webSocketDebuggerUrl": f"ws://page-{index}",
            }
            for index in range(2)
        ]

        with self.assertRaisesRegex(MODULE.ProbeBlocked, "webview_target_unavailable"):
            MODULE._select_visible_product_page(targets, lambda _url: True)

    def test_android_adapter_refresh_uses_scheduler_or_mobile_periodic_loop(self):
        adapter = MODULE.AndroidProductAdapter()
        for result in ("triggered", "mobile_periodic"):
            with self.subTest(result=result), mock.patch.object(
                adapter, "_evaluate", return_value=result
            ) as evaluate:
                self.assertTrue(adapter.refresh_store())
                expression = evaluate.call_args.args[0]
                self.assertIn("/api/v1/sync/state", expression)
                self.assertIn("/api/v1/sync/now", expression)
        with mock.patch.object(adapter, "_evaluate", return_value="unexpected"):
            self.assertFalse(adapter.refresh_store())

    def test_device_probe_redacted_reduction_omits_account_item_request_and_device_ids(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            adapter.graph_items["private-item-p1"] = True
            adapter.refresh_store()
            self.observe(path, "P1", "read", adapter)
            self.observe(path, "P2", "unread", adapter)
            MODULE.cleanup(path, adapter, True)
            output = Path(tmp) / "reduced.json"
            report = MODULE.reduce_state(path, output)
            rendered = json.dumps(report)
            for forbidden in (
                "private-account", "private-item", "private.package",
                "request_id", "device_serial", "action_hash",
            ):
                self.assertNotIn(forbidden, rendered)
            self.assertTrue(report["foreground_restored"])

    def test_cleanup_without_scenario_observations_cannot_pass(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            MODULE.cleanup(path, adapter, True)
            output = Path(tmp) / "report.json"
            with self.assertRaisesRegex(MODULE.ProbeBlocked, "reduction_blocked"):
                MODULE.reduce_state(path, output)
            self.assertFalse(output.exists())
            self.assertFalse(any(call[0] == "set_read" for call in adapter.calls))

    def test_unread_without_native_prompt_is_not_denial_evidence(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            with self.assertRaisesRegex(MODULE.ProbeBlocked, "state_transition_invalid"):
                MODULE.observe(path, "P2", "unread", adapter)
            adapter.prompt_open = False
            with self.assertRaisesRegex(MODULE.ProbeBlocked, "observation_failed"):
                MODULE.observe_prompt(path, "P2", adapter)

    def test_same_native_scenario_cannot_supply_both_rows(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            binding = adapter.observe_native_prompt("synthetic-account")
            with mock.patch.object(adapter, "observe_native_prompt", return_value=binding):
                MODULE.observe_prompt(path, "P1", adapter)
                with self.assertRaisesRegex(MODULE.ProbeBlocked, "observation_failed"):
                    MODULE.observe_prompt(path, "P2", adapter)

    def test_failed_scenario_then_successful_cleanup_never_becomes_pass(self):
        for failure in ("scenario", "graph", "store", "exception"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory(dir="/tmp") as tmp:
                adapter = FakeAdapter()
                path = self.prepare(tmp, adapter)
                self.register_both(path, adapter)
                adapter.graph_items["private-item-p1"] = True
                self.observe(path, "P1", "read", adapter)
                MODULE.observe_prompt(path, "P2", adapter)
                if failure == "scenario":
                    adapter.scenario_ok = False
                with mock.patch.object(MODULE, "_poll_graph", return_value=failure != "graph"), \
                     mock.patch.object(MODULE, "STORE_POLL_ATTEMPTS", 1), \
                     mock.patch.object(MODULE, "_poll_store", return_value=failure != "store"), \
                     mock.patch.object(adapter, "scenario_verified", side_effect=MODULE.ProbeBlocked("observation_failed") if failure == "exception" else None, return_value=adapter.scenario_ok):
                    with self.assertRaises(MODULE.ProbeBlocked):
                        MODULE.observe(path, "P2", "unread", adapter)
                MODULE.cleanup(path, adapter, True)
                state = MODULE._load_state(path)
                self.assertEqual(state["state"], "cleaned")
                self.assertEqual(state["rows"]["P2"]["observation_state"], "failed")
                with self.assertRaisesRegex(MODULE.ProbeBlocked, "reduction_blocked"):
                    MODULE.reduce_state(path, Path(tmp) / "report.json")

    def test_approved_observation_cannot_be_fabricated_by_cleanup_readback(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            MODULE.observe_prompt(path, "P1", adapter)
            with mock.patch.object(MODULE, "_poll_graph", return_value=False):
                with self.assertRaises(MODULE.ProbeBlocked):
                    MODULE.observe(path, "P1", "read", adapter)
            self.observe(path, "P2", "unread", adapter)
            MODULE.cleanup(path, adapter, True)
            with self.assertRaisesRegex(MODULE.ProbeBlocked, "reduction_blocked"):
                MODULE.reduce_state(path, Path(tmp) / "report.json")

    def test_complete_observations_survive_cleanup_retry(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            adapter.graph_items["private-item-p1"] = True
            self.observe(path, "P1", "read", adapter)
            self.observe(path, "P2", "unread", adapter)
            adapter.set_ok = False
            with self.assertRaises(MODULE.ProbeBlocked):
                MODULE.cleanup(path, adapter, True)
            adapter.set_ok = True
            MODULE.cleanup(path, adapter, True)
            report = MODULE.reduce_state(path, Path(tmp) / "report.json")
            self.assertTrue(report["cleanup_retry_was_required"])
            self.assertTrue(all(row["result"] == "pass" for row in report["rows"].values()))

    def test_missing_read_boolean_is_not_unread(self):
        adapter = MODULE.AndroidProductAdapter()
        for value in (None, "false", 0, {}):
            with self.subTest(value=value), mock.patch.object(adapter, "_evaluate", return_value=value):
                for reader in (adapter.store_is_read, adapter.graph_is_read):
                    with self.assertRaises(MODULE.ProbeBlocked):
                        reader("synthetic-account", "synthetic-item")

    def test_cleanup_without_registered_fixture_restores_foreground_but_cannot_reduce(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            MODULE.cleanup(path, adapter, True)
            self.assertTrue(MODULE._load_state(path)["foreground_restored"])
            self.assertFalse(any(call[0] == "set_read" for call in adapter.calls))
            with self.assertRaises(MODULE.ProbeBlocked):
                MODULE.reduce_state(path, Path(tmp) / "report.json")

    def test_unexpected_denial_effect_is_reverted_but_never_passes(self):
        with tempfile.TemporaryDirectory(dir="/tmp") as tmp:
            adapter = FakeAdapter()
            path = self.prepare(tmp, adapter)
            self.register_both(path, adapter)
            adapter.graph_items["private-item-p1"] = True
            self.observe(path, "P1", "read", adapter)
            self.observe(path, "P2", "unread", adapter)
            adapter.graph_items["private-item-p2"] = True
            MODULE.cleanup(path, adapter, True)
            self.assertFalse(adapter.graph_items["private-item-p2"])
            with self.assertRaises(MODULE.ProbeBlocked):
                MODULE.reduce_state(path, Path(tmp) / "report.json")

    def test_production_scenario_expressions_require_native_history_and_audit_facts(self):
        adapter = MODULE.AndroidProductAdapter()
        binding = {"pending_id": "synthetic-pending", "session_id": "synthetic-session",
                   "turn_id": "synthetic-turn", "audit_watermark": "1000"}
        expressions = {}
        for approved in (True, False):
            with mock.patch.object(adapter, "_evaluate", return_value=True) as evaluate:
                adapter.scenario_verified("synthetic-account", binding, approved)
                expressions[str(approved)] = evaluate.call_args.args[0]
        script = r'''
const fs = require('fs'), vm = require('vm'), assert = require('assert');
const expressions = JSON.parse(fs.readFileSync(0, 'utf8'));
function fixture(approved) {
  const record = {status: approved ? 'confirmed' : 'pending', error: 'cancelled'};
  const history = {records: [{turn_id: 'synthetic-turn', kind: {
    kind: approved ? 'operation_state' : 'pending_operation',
    code: approved ? 'completed' : 'confirmation_required'}}], refreshing: false, next_cursor: null};
  const activity = {runs: approved ? [{id: 1001, kind: 'audit:agent-confirm',
    summary: JSON.stringify({state: 'completed'})}] : [{id: 1000, kind: 'audit:agent-confirm', summary: '{}'}]};
  const context = {App: {account: 'synthetic-account'}, _bioPending: new Map(),
    AssistantState: {pendingCardsById: new Map([['synthetic-pending', record]]), confirmAttemptsByPendingId: new Map()},
    nativeConfirmationMessage: () => 'cancelled', request: async () => history,
    api: async () => activity, qs: () => '', CAP: {agent: 'synthetic-cap'}};
  return {context, history, activity, record};
}
(async () => {
  for (const approved of [true, false]) {
    const expression = expressions[approved ? 'True' : 'False'];
    const run = async (mutate, expected) => {
      const f = fixture(approved); mutate(f);
      assert.strictEqual(await vm.runInNewContext(expression, f.context), expected);
    };
    await run(() => {}, true);
    await run(f => {f.history.refreshing = true;}, false);
    await run(f => {f.history.next_cursor = 'next';}, false);
    await run(f => {f.history.records[0].turn_id = 'other';}, false);
    await run(f => {f.record.status = 'failed';}, false);
    await run(f => {f.context._bioPending.set('pending', {});}, false);
    await run(f => {f.context.AssistantState.confirmAttemptsByPendingId.set('synthetic-pending', {});}, false);
    await run(f => {f.activity.runs = Array.from({length: 500}, (_, i) => ({id: 2000 + i, kind: 'backup', summary: '{}'}));}, false);
    await run(f => {
      const expected = f.activity.runs[0];
      f.activity.runs = [expected, ...Array.from({length: 499}, (_, i) => ({id: 999 - i, kind: 'backup', summary: '{}'}))];
    }, true);
    if (!approved) await run(f => {f.activity.runs.push({id: 1001, kind: 'audit:agent-confirm', summary: '{}'});}, false);
    else await run(f => {f.activity.runs = [];}, false);
  }
})().catch(error => {console.error(error); process.exit(1);});
'''
        result = subprocess.run(["node", "-e", script], input=json.dumps(expressions),
                                text=True, capture_output=True, timeout=20, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_native_prompt_requires_focused_system_window_and_product_binding(self):
        adapter = MODULE.AndroidProductAdapter()
        binding = FakeAdapter().observe_native_prompt("synthetic-account")
        for window, accepted in [
            ("mCurrentFocus=Window{1 u0 BiometricPrompt}\n", True),
            ("mCurrentFocus=Window{1 u0 app/Activity}\nBiometricPrompt background\n", False),
        ]:
            completed = MODULE.subprocess.CompletedProcess(["adb"], 0, window, "")
            with self.subTest(accepted=accepted), mock.patch.object(adapter, "_adb", return_value=completed), \
                 mock.patch.object(adapter, "_evaluate", return_value=binding):
                if accepted:
                    self.assertEqual(adapter.observe_native_prompt("synthetic-account"), binding)
                else:
                    with self.assertRaises(MODULE.ProbeBlocked):
                        adapter.observe_native_prompt("synthetic-account")


if __name__ == "__main__":
    unittest.main()
