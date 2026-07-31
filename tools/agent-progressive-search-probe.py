#!/usr/bin/env python3
"""Reduce deterministic or live #643 progressive-search evidence.

The report contains only closed state, counts, and opaque digests. Prompt text,
result text, source identifiers, tokens, provider frames, and raw ToolResults are
kept transient and are never written to evidence.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import re
import shutil
import sys
import time
import unicodedata
import urllib.error
import urllib.parse
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from types import ModuleType
from typing import Callable


ACTIVITY_ID_RE = re.compile(r"^[A-Za-z0-9_-]{22}$")
SAFE_CODE_RE = re.compile(r"^[a-z][a-z0-9_]{0,63}$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SERVICES = {"mail", "onedrive", "calendar", "contacts", "todo", "onenote"}
STAGES = ("names", "bodies", "deep")
STATUSES = {"queued", "running", "complete", "failed", "skipped", "cancelled"}
TERMINAL_STAGE_STATUSES = {"complete", "failed", "skipped", "cancelled"}
MAX_ACTIVITIES = 4
MAX_STAGE_UPDATES = 256
MAX_PARTIAL_UPDATES = 64
MAX_RESULTS = 200
MAX_EVENTS = 4096
MAX_EVENT_BYTES = 72 * 1024
MAX_STREAM_BYTES = 1024 * 1024
MAX_PROMPT_BYTES = 32 * 1024
MAX_REPORT_BYTES = 256 * 1024
FORBIDDEN_PROGRESS_KEYS = {
    "candidate",
    "candidate_handle",
    "candidates",
    "continuation",
    "continuation_token",
    "deep_context",
    "provider_content",
    "body_rel_path",
    "local_path",
    "snippet",
    "excerpt",
}
OTHER_EVENTS = {
    "progress",
    "token",
    "tool_call",
    "tool_result",
    "confirmation_required",
    "error",
    "done",
}


class ProbeError(RuntimeError):
    def __init__(self, code: str):
        super().__init__(code)
        self.code = code


def strict_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise ProbeError("duplicate_json_member")
        result[key] = value
    return result


def canonical_bytes(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")


def opaque_digest(value: str, domain: str) -> str:
    digest = hashlib.sha256()
    digest.update(domain.encode("ascii"))
    digest.update(b"\0")
    digest.update(value.encode("utf-8"))
    return f"sha256:{digest.hexdigest()}"


def read_json(path: Path, max_bytes: int = 1024 * 1024) -> dict[str, object]:
    raw = path.read_bytes()
    if len(raw) > max_bytes:
        raise ProbeError("fixture_too_large")
    try:
        value = json.loads(raw, object_pairs_hook=strict_object)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise ProbeError("fixture_invalid") from error
    if not isinstance(value, dict):
        raise ProbeError("fixture_invalid")
    return value


def read_private_prompt(path: Path) -> str:
    if not path.is_file():
        raise ProbeError("prompt_unavailable")
    raw = path.read_bytes()
    if not raw or len(raw) > MAX_PROMPT_BYTES:
        raise ProbeError("prompt_invalid")
    try:
        prompt = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ProbeError("prompt_invalid") from error
    if not prompt.strip():
        raise ProbeError("prompt_invalid")
    return prompt


def contains_forbidden_key(value: object) -> bool:
    if isinstance(value, dict):
        return any(
            key in FORBIDDEN_PROGRESS_KEYS or contains_forbidden_key(child)
            for key, child in value.items()
        )
    if isinstance(value, list):
        return any(contains_forbidden_key(child) for child in value)
    return False


def valid_text(value: object, maximum: int, *, optional: bool = False) -> bool:
    if value is None:
        return optional
    return (
        isinstance(value, str)
        and bool(value)
        and len(value.encode("utf-8")) <= maximum
        and not any(unicodedata.category(character).startswith("C") for character in value)
    )


def valid_source(value: object, service: str, item_id: str, name: str) -> bool:
    if not isinstance(value, dict) or set(value) != {"service", "item_id", "label"}:
        return False
    label = value.get("label")
    return (
        value.get("service") == service
        and value.get("item_id") == item_id
        and (label is None or label == name)
        and valid_text(label, 192, optional=True)
        and len(canonical_bytes(value)) <= 2 * 1024
    )


def valid_display_path(value: object) -> bool:
    if not valid_text(value, 768, optional=True):
        return False
    if value is None:
        return True
    return (
        not value.startswith("/")
        and "\\" not in value
        and ":" not in value
        and all(component not in {"", ".", ".."} for component in value.split("/"))
    )


def valid_result(value: object) -> bool:
    expected = {
        "result_key",
        "change",
        "service",
        "item_id",
        "name",
        "item_type",
        "display_path",
        "sender",
        "body_available",
        "source",
    }
    if not isinstance(value, dict) or set(value) != expected:
        return False
    result_key = value.get("result_key")
    service = value.get("service")
    item_id = value.get("item_id")
    name = value.get("name")
    if (
        not isinstance(result_key, str)
        or not ACTIVITY_ID_RE.fullmatch(result_key)
        or value.get("change") not in {"add", "enrich"}
        or service not in SERVICES
        or not valid_text(item_id, 512)
        or not valid_text(name, 192)
        or not valid_text(value.get("item_type"), 64)
        or not valid_display_path(value.get("display_path"))
        or not valid_text(value.get("sender"), 256, optional=True)
        or type(value.get("body_available")) is not bool
    ):
        return False
    return valid_source(value.get("source"), service, item_id, name)


@dataclass
class ActivityState:
    stages: dict[str, str] = field(default_factory=dict)
    stage_updates: int = 0
    partial_updates: int = 0
    next_sequence: int = 0
    result_keys: set[str] = field(default_factory=set)
    result_keys_by_stage: dict[str, set[str]] = field(default_factory=dict)
    replay_digests: dict[int, str] = field(default_factory=dict)


class ProgressiveReducer:
    def __init__(self) -> None:
        self.activities: dict[str, ActivityState] = {}
        self.event_names: list[str] = []
        self.stage_facts: list[dict[str, object]] = []
        self.partial_facts: list[dict[str, object]] = []
        self.sources: dict[tuple[str, str], None] = {}
        self.replays = 0
        self.terminal_reason: str | None = None
        self.error_code: str | None = None

    def accept(self, event: dict[str, object]) -> None:
        if len(self.event_names) >= MAX_EVENTS or len(canonical_bytes(event)) > MAX_EVENT_BYTES:
            raise ProbeError("event_budget_exhausted")
        name = event.get("event")
        if not isinstance(name, str) or name not in OTHER_EVENTS | {
            "stage_progress",
            "partial_result",
        }:
            raise ProbeError("event_invalid")
        if contains_forbidden_key(event):
            raise ProbeError("private_projection_observed")
        if self.terminal_reason is not None:
            raise ProbeError("event_after_terminal")
        self.event_names.append(name)
        if name == "stage_progress":
            self._accept_stage(event)
        elif name == "partial_result":
            self._accept_partial(event)
        elif name == "error":
            code = event.get("message")
            if not isinstance(code, str) or not SAFE_CODE_RE.fullmatch(code):
                raise ProbeError("error_event_invalid")
            self.error_code = code
        elif name == "done":
            reason = event.get("reason")
            if reason not in {"complete", "error", "cancelled", "pending_confirmation"}:
                raise ProbeError("terminal_event_invalid")
            self.terminal_reason = str(reason)

    def _activity(self, event: dict[str, object], *, create: bool) -> tuple[str, ActivityState]:
        activity_id = event.get("activity_id")
        if not isinstance(activity_id, str) or not ACTIVITY_ID_RE.fullmatch(activity_id):
            raise ProbeError("activity_id_invalid")
        state = self.activities.get(activity_id)
        if state is None:
            if not create or len(self.activities) >= MAX_ACTIVITIES:
                raise ProbeError("activity_invalid")
            state = ActivityState()
            self.activities[activity_id] = state
        return activity_id, state

    def _accept_stage(self, event: dict[str, object]) -> None:
        expected = {
            "event",
            "schema_version",
            "activity_id",
            "activity_kind",
            "stage",
            "status",
            "scanned",
            "total",
            "hits",
            "current_item",
            "coverage_complete",
            "budget_reached",
            "continuation_available",
        }
        if (
            set(event) != expected
            or contains_forbidden_key(event)
            or event.get("schema_version") != 1
            or event.get("activity_kind") != "archive_search"
            or event.get("stage") not in STAGES
            or event.get("status") not in STATUSES
            or not self._valid_counter(event.get("scanned"))
            or not self._valid_counter(event.get("hits"))
            or (
                event.get("total") is not None
                and not self._valid_counter(event.get("total"))
            )
            or not valid_text(event.get("current_item"), 160, optional=True)
            or any(
                event.get(key) not in {None, True, False}
                for key in (
                    "coverage_complete",
                    "budget_reached",
                    "continuation_available",
                )
            )
        ):
            raise ProbeError("stage_event_invalid")
        activity_id, state = self._activity(event, create=True)
        if state.stage_updates >= MAX_STAGE_UPDATES:
            raise ProbeError("stage_update_limit")
        stage = str(event["stage"])
        status = str(event["status"])
        stage_index = STAGES.index(stage)
        prior_statuses = [state.stages.get(prior) for prior in STAGES[:stage_index]]
        if status == "queued":
            ordered = all(prior is not None for prior in prior_statuses)
        elif status == "running":
            ordered = all(prior == "complete" for prior in prior_statuses)
        else:
            ordered = all(prior in TERMINAL_STAGE_STATUSES for prior in prior_statuses)
        if not ordered:
            raise ProbeError("stage_order_invalid")
        previous = state.stages.get(stage)
        if previous is None and status != "queued":
            raise ProbeError("stage_transition_invalid")
        if previous in TERMINAL_STAGE_STATUSES and previous != status:
            raise ProbeError("stage_transition_invalid")
        if previous == "queued" and status not in {
            "queued",
            "running",
            "failed",
            "skipped",
            "cancelled",
        }:
            raise ProbeError("stage_transition_invalid")
        if previous == "running" and status == "queued":
            raise ProbeError("stage_transition_invalid")
        state.stage_updates += 1
        state.stages[stage] = status
        self.stage_facts.append(
            {
                "activity": opaque_digest(activity_id, "issue-643-activity"),
                "stage": stage,
                "status": status,
                "scanned": event["scanned"],
                "total": event["total"],
                "hits": event["hits"],
                "current_item_present": event["current_item"] is not None,
                "coverage_complete": event["coverage_complete"],
                "budget_reached": event["budget_reached"],
                "continuation_available": event["continuation_available"],
            }
        )

    def _accept_partial(self, event: dict[str, object]) -> None:
        expected = {
            "event",
            "schema_version",
            "activity_id",
            "stage",
            "sequence",
            "items",
        }
        if (
            set(event) != expected
            or contains_forbidden_key(event)
            or event.get("schema_version") != 1
            or event.get("stage") not in STAGES
            or type(event.get("sequence")) is not int
            or not 0 <= int(event["sequence"]) <= 65535
            or not isinstance(event.get("items"), list)
            or len(event["items"]) > 20
            or any(not valid_result(item) for item in event["items"])
        ):
            raise ProbeError("partial_event_invalid")
        activity_id, state = self._activity(event, create=False)
        stage = str(event["stage"])
        sequence = int(event["sequence"])
        digest = hashlib.sha256(canonical_bytes(event)).hexdigest()
        if sequence < state.next_sequence:
            if state.replay_digests.get(sequence) != digest:
                raise ProbeError("partial_replay_conflict")
            self.replays += 1
            return
        if (
            sequence != state.next_sequence
            or state.partial_updates >= MAX_PARTIAL_UPDATES
            or state.stages.get(stage) != "running"
        ):
            raise ProbeError("partial_sequence_invalid")
        next_keys = set(state.result_keys)
        digests: list[str] = []
        for item in event["items"]:
            result_key = str(item["result_key"])
            known = result_key in next_keys
            if (item["change"] == "add" and known) or (
                item["change"] == "enrich" and not known
            ):
                raise ProbeError("result_change_invalid")
            next_keys.add(result_key)
            state.result_keys_by_stage.setdefault(stage, set()).add(result_key)
            if len(next_keys) > MAX_RESULTS:
                raise ProbeError("result_limit")
            source = item["source"]
            self.sources[(str(source["service"]), str(source["item_id"]))] = None
            digests.append(opaque_digest(result_key, "issue-643-result"))
        state.result_keys = next_keys
        state.replay_digests[sequence] = digest
        state.partial_updates += 1
        state.next_sequence += 1
        self.partial_facts.append(
            {
                "activity": opaque_digest(activity_id, "issue-643-activity"),
                "stage": stage,
                "sequence": sequence,
                "item_count": len(event["items"]),
                "result_key_digests": digests,
            }
        )

    @staticmethod
    def _valid_counter(value: object) -> bool:
        return type(value) is int and 0 <= int(value) <= 1_000_000

    def report(
        self,
        source_resolver: Callable[[str, str], bool] | None = None,
    ) -> dict[str, object]:
        if (
            self.terminal_reason is None
            or not self.event_names
            or self.event_names[-1] != "done"
            or self.event_names.count("done") != 1
            or not self.activities
        ):
            raise ProbeError("terminal_state_missing")
        if any(
            status not in TERMINAL_STAGE_STATUSES
            for activity in self.activities.values()
            for status in activity.stages.values()
        ):
            raise ProbeError("open_stage_at_terminal")
        resolved = None
        if source_resolver is not None:
            resolved = bool(self.sources) and all(
                source_resolver(service, item_id) for service, item_id in self.sources
            )
        result_count = sum(len(activity.result_keys) for activity in self.activities.values())
        budget_stop = any(
            fact["budget_reached"] is True for fact in self.stage_facts
        )
        continuation_available = any(
            fact["continuation_available"] is True for fact in self.stage_facts
        )
        return {
            "ordered_event_types": self.event_names,
            "stage_updates": self.stage_facts,
            "partial_updates": self.partial_facts,
            "activity_count": len(self.activities),
            "result_count": result_count,
            "replay_count": self.replays,
            "source_count": len(self.sources),
            "all_sources_resolved": resolved,
            "budget_stop_observed": budget_stop,
            "continuation_available_observed": continuation_available,
            "terminal_reason": self.terminal_reason,
            "error_code": self.error_code,
        }


def reduce_events(
    events: list[object],
    source_resolver: Callable[[str, str], bool] | None = None,
) -> tuple[ProgressiveReducer, dict[str, object]]:
    reducer = ProgressiveReducer()
    for event in events:
        if not isinstance(event, dict):
            raise ProbeError("event_invalid")
        reducer.accept(event)
    return reducer, reducer.report(source_resolver)


def validate_scripted_fixture(document: dict[str, object]) -> dict[str, object]:
    if set(document) != {
        "schema_version",
        "fixture_kind",
        "provider_script",
        "events",
        "source_resolution",
    }:
        raise ProbeError("fixture_invalid")
    if document.get("schema_version") != 1 or document.get("fixture_kind") != "synthetic":
        raise ProbeError("fixture_invalid")
    script = document.get("provider_script")
    resolutions = document.get("source_resolution")
    events = document.get("events")
    if (
        not isinstance(script, dict)
        or set(script) != {"selected_result_keys"}
        or not isinstance(script.get("selected_result_keys"), list)
        or not script["selected_result_keys"]
        or any(
            not isinstance(value, str) or not ACTIVITY_ID_RE.fullmatch(value)
            for value in script["selected_result_keys"]
        )
        or not isinstance(resolutions, dict)
        or any(
            not isinstance(key, str)
            or not ACTIVITY_ID_RE.fullmatch(key)
            or type(value) is not bool
            for key, value in resolutions.items()
        )
        or not isinstance(events, list)
    ):
        raise ProbeError("fixture_invalid")
    reducer, report = reduce_events(events)
    observed_keys = {
        key for activity in reducer.activities.values() for key in activity.result_keys
    }
    selected = set(script["selected_result_keys"])
    if not selected.issubset(observed_keys):
        raise ProbeError("scripted_selection_missing")
    if set(resolutions) != observed_keys:
        raise ProbeError("source_resolution_incomplete")
    report["all_sources_resolved"] = bool(resolutions) and all(resolutions.values())
    report["scripted_selection_count"] = len(selected)
    keyword_keys = {
        key
        for activity in reducer.activities.values()
        for stage in ("names", "bodies")
        for key in activity.result_keys_by_stage.get(stage, set())
    }
    deep_keys = {
        key
        for activity in reducer.activities.values()
        for key in activity.result_keys_by_stage.get("deep", set())
    }
    report["keywordless_selection_observed"] = selected.issubset(deep_keys - keyword_keys)
    if not report["keywordless_selection_observed"]:
        raise ProbeError("scripted_selection_not_keywordless")
    return report


def load_epic_probe() -> ModuleType:
    path = Path(__file__).with_name("agent-epic-closeout-probe.py")
    spec = importlib.util.spec_from_file_location("isyncyou_agent_epic_probe", path)
    if spec is None or spec.loader is None:
        raise ProbeError("runtime_helpers_unavailable")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def stream_live_turn(
    epic: ModuleType,
    client: object,
    turn_id: str,
    timeout: float,
) -> ProgressiveReducer:
    query = urllib.parse.urlencode({"turn": turn_id})
    request = client._request(  # type: ignore[attr-defined]
        "GET", f"/api/v1/agent/stream?{query}", timeout=timeout
    )
    reducer = ProgressiveReducer()
    deadline = time.monotonic() + timeout
    total_bytes = 0
    try:
        with client.opener.open(request, timeout=min(timeout, 10.0)) as response:  # type: ignore[attr-defined]
            if response.status != 200 or response.headers.get_content_type() != "text/event-stream":
                raise ProbeError("turn_stream_unavailable")
            while time.monotonic() < deadline:
                line = response.readline(MAX_EVENT_BYTES + 1)
                if not line:
                    break
                total_bytes += len(line)
                if len(line) > MAX_EVENT_BYTES or total_bytes > MAX_STREAM_BYTES:
                    raise ProbeError("turn_stream_too_large")
                if not line.startswith(b"data:"):
                    continue
                try:
                    event = json.loads(line[5:].strip(), object_pairs_hook=strict_object)
                except (json.JSONDecodeError, UnicodeDecodeError) as error:
                    raise ProbeError("turn_stream_invalid") from error
                if not isinstance(event, dict):
                    raise ProbeError("turn_stream_invalid")
                reducer.accept(event)
                if reducer.terminal_reason is not None:
                    break
    except urllib.error.HTTPError as error:
        error.close()
        raise ProbeError("turn_stream_unavailable") from error
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise ProbeError("turn_stream_failed") from error
    if reducer.terminal_reason is None:
        raise ProbeError("turn_stream_missing_terminal")
    return reducer


def run_live(args: argparse.Namespace) -> dict[str, object]:
    epic = load_epic_probe()
    implementation = epic.validate_git_object(
        args.implementation_commit, "commit", "implementation_commit"
    )
    host, port, base = epic.loopback_endpoint(args.endpoint or args.bind)
    runtime, remove_runtime = epic.private_runtime_root(
        Path(args.runtime_root) if args.runtime_root else None
    )
    daemon = None
    try:
        binary_digest = None
        if args.daemon_bin:
            epic.require_free_port(host, port)
            binary = Path(args.daemon_bin).resolve()
            binary_digest = epic.sha256_file(binary)
            daemon = epic.start_daemon(
                binary, Path(args.config).resolve(), f"{host}:{port}", runtime
            )
        client, shell_ready, strict_cookie, status_ready, status = epic.wait_for_runtime(
            base, daemon, args.startup_timeout
        )
        provider = status.get("selected_provider")
        if provider not in {"claude", "codex"} or status.get("connected") is not True:
            raise ProbeError("provider_not_ready")
        client.load_agent_capability()
        account = epic.select_runtime_account(
            client, Path(args.account_id_file) if args.account_id_file else None
        )
        session_id = epic.select_or_create_session(client)
        epic.load_session_records(client, session_id)
        prompt = read_private_prompt(Path(args.prompt_file))
        request_id = str(uuid.uuid4())
        turn_body = {
            "request_id": request_id,
            "session_id": session_id,
            "account": account,
            "prompt": prompt,
        }
        first = client.json(
            "POST",
            "/api/v1/agent/turn",
            cap=True,
            value=turn_body,
            timeout=min(args.turn_timeout, 10.0),
        )
        replay = client.json(
            "POST",
            "/api/v1/agent/turn",
            cap=True,
            value=turn_body,
            timeout=min(args.turn_timeout, 10.0),
        )
        turn_id = first.get("turn")
        if (
            not isinstance(turn_id, str)
            or not turn_id
            or len(turn_id) > 128
            or replay.get("turn") != turn_id
        ):
            raise ProbeError("turn_admission_invalid")
        reducer = stream_live_turn(epic, client, turn_id, args.turn_timeout)

        def resolve_source(service: str, item_id: str) -> bool:
            return epic.source_is_listed(
                client, account, service, item_id
            ) and epic.source_view_resolves(client, account, service, item_id)

        progress = reducer.report(resolve_source)
        status_query = urllib.parse.urlencode(
            {
                "session_id": session_id,
                "route": "agent_turn",
                "request_id": request_id,
            }
        )
        state, code, terminal = epic.read_request_status(client, status_query)
        passed = (
            shell_ready
            and strict_cookie
            and status_ready
            and progress["terminal_reason"] == "complete"
            and progress["all_sources_resolved"] is True
            and state == "committed"
            and terminal is True
        )
        return {
            "schema_version": 1,
            "mode": "live",
            "state": "pass" if passed else "fail",
            "implementation_commit": implementation,
            "managed_daemon": daemon is not None,
            "daemon_binary_sha256": binary_digest,
            "provider": provider,
            "request": opaque_digest(request_id, "issue-643-request"),
            "turn": opaque_digest(turn_id, "issue-643-turn"),
            "retry_reused_turn": True,
            "request_status": {"state": state, "code": code, "terminal": terminal},
            "progressive_search": progress,
            "cleanup": {"daemon_stopped": True, "runtime_files_cleaned": True},
            "redaction": redaction_contract(),
        }
    finally:
        if daemon is not None:
            daemon.stop()
        for child in runtime.iterdir() if runtime.exists() else ():
            if child.is_file() or child.is_symlink():
                child.unlink(missing_ok=True)
            elif child.is_dir():
                shutil.rmtree(child)
        if remove_runtime and runtime.exists():
            runtime.rmdir()


def redaction_contract() -> dict[str, bool]:
    return {
        "prompt_included": False,
        "sender_or_email_included": False,
        "body_or_snippet_included": False,
        "source_id_or_path_included": False,
        "token_included": False,
        "raw_tool_result_included": False,
        "provider_request_included": False,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("fixture", "live"))
    parser.add_argument("--implementation-commit", required=True)
    parser.add_argument("--fixture")
    parser.add_argument("--prompt-file")
    endpoint = parser.add_mutually_exclusive_group()
    endpoint.add_argument("--endpoint")
    endpoint.add_argument("--daemon-bin")
    parser.add_argument("--config")
    parser.add_argument("--bind", default="127.0.0.1:8871")
    parser.add_argument("--runtime-root")
    parser.add_argument("--account-id-file")
    parser.add_argument("--startup-timeout", type=float, default=20.0)
    parser.add_argument("--turn-timeout", type=float, default=1200.0)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    if not SHA_RE.fullmatch(args.implementation_commit):
        parser.error("--implementation-commit must be a full lowercase commit SHA")
    if args.mode == "fixture":
        if not args.fixture:
            parser.error("fixture mode requires --fixture")
        if any((args.prompt_file, args.endpoint, args.daemon_bin, args.config)):
            parser.error("runtime arguments are invalid in fixture mode")
    else:
        if not args.prompt_file:
            parser.error("live mode requires --prompt-file")
        if not args.endpoint and not args.daemon_bin:
            parser.error("live mode requires --endpoint or --daemon-bin")
        if args.daemon_bin and not args.config:
            parser.error("--config is required with --daemon-bin")
        if args.fixture:
            parser.error("--fixture is invalid in live mode")
    if not 0 < args.startup_timeout <= 120 or not 0 < args.turn_timeout <= 1200:
        parser.error("timeout is outside the bounded range")
    return args


def run(args: argparse.Namespace) -> tuple[dict[str, object], int]:
    if args.mode == "live":
        report = run_live(args)
        return report, 0 if report["state"] == "pass" else 2
    document = read_json(Path(args.fixture))
    progressive = validate_scripted_fixture(document)
    passed = (
        progressive["terminal_reason"] == "complete"
        and progressive["all_sources_resolved"] is True
        and progressive["keywordless_selection_observed"] is True
    )
    report = {
        "schema_version": 1,
        "mode": "fixture",
        "state": "pass" if passed else "fail",
        "implementation_commit": args.implementation_commit,
        "fixture_sha256": f"sha256:{hashlib.sha256(Path(args.fixture).read_bytes()).hexdigest()}",
        "fixture_kind": "synthetic",
        "progressive_search": progressive,
        "redaction": redaction_contract(),
    }
    return report, 0 if passed else 2


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        report, status = run(args)
        encoded = canonical_bytes(report)
        if len(encoded) > MAX_REPORT_BYTES:
            raise ProbeError("report_too_large")
    except ProbeError as error:
        report = {
            "schema_version": 1,
            "mode": args.mode,
            "state": "fail",
            "code": error.code,
            "redaction": redaction_contract(),
        }
        status = 1
    output = Path(args.out)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return status


if __name__ == "__main__":
    raise SystemExit(main())
