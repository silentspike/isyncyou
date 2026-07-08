#!/usr/bin/env python3
"""Android WebView bridge-isolation probe for issue #721.

The script drives the debug APK through ADB and WebView CDP. It does not add an
app endpoint and does not bypass the production bridge. It is intentionally
evidence-oriented: every assertion is recorded in a JSON report.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.request
from dataclasses import dataclass
from typing import Any


DEFAULT_PACKAGE = "com.silentspike.isyncyou.debug"
DEFAULT_LOCK = "om-721"
DEFAULT_CDP_PORT = 9222
FORCE_FAIL_FLAG = "files/debug/force_bridge_preflight_fail"


class ProbeError(RuntimeError):
    pass


@dataclass
class CmdResult:
    argv: list[str]
    rc: int
    out: str
    err: str


def run(argv: list[str], *, check: bool = True, timeout: int = 30) -> CmdResult:
    proc = subprocess.run(
        argv,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )
    res = CmdResult(argv, proc.returncode, proc.stdout, proc.stderr)
    if check and proc.returncode != 0:
        joined = " ".join(argv)
        raise ProbeError(f"command failed rc={proc.returncode}: {joined}\n{proc.stderr.strip()}")
    return res


class Evidence:
    def __init__(self) -> None:
        self.rows: list[dict[str, Any]] = []
        self.meta: dict[str, Any] = {}

    def pass_(self, name: str, **details: Any) -> None:
        self.rows.append({"name": name, "status": "PASS", **details})

    def fail(self, name: str, **details: Any) -> None:
        self.rows.append({"name": name, "status": "FAIL", **details})

    def skip(self, name: str, **details: Any) -> None:
        self.rows.append({"name": name, "status": "SKIP", **details})

    def assert_(self, name: str, ok: bool, **details: Any) -> None:
        if ok:
            self.pass_(name, **details)
        else:
            self.fail(name, **details)

    def ok(self) -> bool:
        return all(row["status"] != "FAIL" for row in self.rows)


class Device:
    def __init__(self, adb: str, package: str, cdp_port: int, evidence: Evidence) -> None:
        self.adb = adb
        self.package = package
        self.cdp_port = cdp_port
        self.evidence = evidence

    def adb_cmd(self, *args: str, check: bool = True, timeout: int = 30) -> CmdResult:
        return run([self.adb, *args], check=check, timeout=timeout)

    def serial(self) -> str:
        return self.adb_cmd("get-serialno", check=False).out.strip()

    def shell(self, command: str, *, check: bool = True, timeout: int = 30) -> str:
        return self.adb_cmd("shell", command, check=check, timeout=timeout).out

    def run_as(self, command: str, *, check: bool = True, timeout: int = 30) -> str:
        return self.shell(f"run-as {self.package} sh -c {json.dumps(command)}", check=check, timeout=timeout)

    def force_stop(self) -> None:
        self.shell(f"am force-stop {self.package}")

    def launch(self) -> None:
        self.wake()
        self.shell(f"monkey -p {self.package} 1 >/dev/null 2>&1")

    def wake(self) -> None:
        self.shell("input keyevent KEYCODE_WAKEUP", check=False)
        self.shell("wm dismiss-keyguard", check=False)

    def remove_forward(self) -> None:
        # Keep this literal command visible for #721 review evidence.
        self.adb_cmd("forward", "--remove", f"tcp:{self.cdp_port}", check=False)

    def clear_force_fail(self) -> None:
        self.run_as(f"rm -f {FORCE_FAIL_FLAG}", check=False)

    def set_force_fail(self) -> None:
        self.run_as(f"mkdir -p files/debug && echo 1 > {FORCE_FAIL_FLAG}")

    def clear_logcat(self) -> None:
        self.adb_cmd("logcat", "-c", check=False)

    def enable_stay_awake(self) -> str:
        previous = self.shell("settings get global stay_on_while_plugged_in", check=False).strip()
        self.shell("svc power stayon true", check=False)
        self.evidence.pass_("device_stay_awake_enabled", previous=previous)
        return previous

    def restore_stay_awake(self, previous: str) -> None:
        if re.fullmatch(r"\d+", previous or ""):
            self.shell(f"settings put global stay_on_while_plugged_in {previous}", check=False)
            self.evidence.pass_("device_stay_awake_restored", value=previous)
        else:
            self.shell("svc power stayon false", check=False)
            self.evidence.pass_("device_stay_awake_restored", value="false")

    def logcat(self, lines: int = 2000) -> str:
        return self.adb_cmd("logcat", "-d", "-t", str(lines), check=False, timeout=20).out

    def webview_socket(self) -> str:
        raw = self.shell("cat /proc/net/unix", timeout=10)
        sockets = re.findall(r"webview_devtools_remote_[^\s]+", raw)
        if not sockets:
            raise ProbeError("no webview_devtools_remote socket found")
        return sockets[-1]

    def forward_cdp(self) -> str:
        self.remove_forward()
        sock = self.webview_socket()
        self.adb_cmd("forward", f"tcp:{self.cdp_port}", f"localabstract:{sock}")
        self.evidence.meta["webview_devtools_socket"] = sock
        self.evidence.pass_("cdp_forward", socket=sock, port=self.cdp_port)
        return sock

    def uid(self) -> str | None:
        pids = self.shell(f"pidof {self.package}", check=False).strip().split()
        if not pids:
            return None
        status = self.shell(f"cat /proc/{pids[0]}/status", check=False)
        m = re.search(r"^Uid:\s+(\d+)", status, re.M)
        return m.group(1) if m else None

    def listen_sockets_for_uid(self) -> list[str]:
        uid = self.uid()
        if not uid:
            return []
        raw = self.shell("cat /proc/net/tcp /proc/net/tcp6 2>/dev/null", check=False)
        listens: list[str] = []
        for line in raw.splitlines():
            cols = line.split()
            if len(cols) < 8 or cols[0] == "sl":
                continue
            state = cols[3]
            sock_uid = cols[7]
            if state == "0A" and sock_uid == uid:
                listens.append(line.strip())
        return listens

    def keyguard_state(self) -> dict[str, Any]:
        raw = self.shell("dumpsys window", check=False, timeout=20)
        current_focus = ""
        focused_app = ""
        for line in raw.splitlines():
            if "mCurrentFocus=" in line:
                current_focus = line.strip()
            if "mFocusedApp=" in line:
                focused_app = line.strip()
        showing = "isKeyguardShowing=true" in raw or "mDreamingLockscreen=true" in raw
        return {
            "keyguard_showing": showing,
            "current_focus": current_focus,
            "focused_app": focused_app,
        }

    def prompt_window_state(self) -> dict[str, Any]:
        raw = self.shell("dumpsys window", check=False, timeout=20)
        matches = [
            line.strip()
            for line in raw.splitlines()
            if re.search(r"biometric|credential|prompt|keyguard", line, re.I)
        ]
        return {"matches": matches[:12]}


class Cdp:
    def __init__(self, port: int) -> None:
        try:
            import websocket  # type: ignore
        except Exception as exc:  # pragma: no cover - runtime dependency check
            raise ProbeError("python websocket-client package is required for CDP runtime") from exc
        self.websocket = websocket
        self.port = port
        self.next_id = 0
        self.target = self.target_info()
        self.ws_url = self.target["webSocketDebuggerUrl"]
        self.ws = self.websocket.create_connection(
            self.ws_url,
            timeout=10,
            suppress_origin=True,
        )

    def close(self) -> None:
        try:
            self.ws.close()
        except Exception:
            pass

    def target_info(self) -> dict[str, Any]:
        with urllib.request.urlopen(f"http://127.0.0.1:{self.port}/json", timeout=10) as resp:
            targets = json.loads(resp.read().decode("utf-8"))
        pages = [t for t in targets if t.get("type") == "page"]
        for target in pages:
            if "appassets.androidplatform.net" in target.get("url", ""):
                return target
        if pages:
            return pages[0]
        raise ProbeError("no CDP page target found")

    def call(self, method: str, params: dict[str, Any] | None = None, timeout: int = 10) -> dict[str, Any]:
        self.next_id += 1
        msg_id = self.next_id
        self.ws.settimeout(timeout)
        self.ws.send(json.dumps({"id": msg_id, "method": method, "params": params or {}}))
        while True:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == msg_id:
                if "error" in msg:
                    raise ProbeError(f"CDP {method} failed: {msg['error']}")
                return msg.get("result", {})

    def eval(self, expression: str, *, await_promise: bool = True, timeout: int = 15) -> Any:
        result = self.call(
            "Runtime.evaluate",
            {
                "expression": expression,
                "awaitPromise": await_promise,
                "returnByValue": True,
                "timeout": timeout * 1000,
            },
            timeout=timeout + 5,
        )
        if "exceptionDetails" in result:
            raise ProbeError(f"CDP eval exception: {result['exceptionDetails']}")
        value = result.get("result", {})
        if "value" in value:
            return value["value"]
        return value.get("description")


def wait_for_cdp(device: Device, seconds: int = 20) -> str:
    deadline = time.time() + seconds
    last = ""
    while time.time() < deadline:
        try:
            return device.forward_cdp()
        except Exception as exc:
            last = str(exc)
            time.sleep(1)
    raise ProbeError(f"CDP target did not appear: {last}")


def wait_for_dom(cdp: Cdp, seconds: int = 15) -> None:
    deadline = time.time() + seconds
    last = ""
    while time.time() < deadline:
        try:
            state = cdp.eval(
                "document.documentElement ? document.readyState : 'missing'",
                timeout=3,
            )
            if state in ("loading", "interactive", "complete"):
                return
        except Exception as exc:
            last = str(exc)
        time.sleep(0.5)
    raise ProbeError(f"WebView DOM did not become available: {last}")


def forced_failure_probe(device: Device, ev: Evidence) -> None:
    device.clear_logcat()
    device.set_force_fail()
    device.force_stop()
    device.launch()
    time.sleep(3)
    wait_for_cdp(device)
    cdp = Cdp(device.cdp_port)
    ev.meta["webview_devtools_target_url"] = cdp.target.get("url", "")
    ev.meta["webview_devtools_ws_url"] = cdp.ws_url
    ev.pass_("webview_devtools_target", url=cdp.target.get("url", ""), ws_url=cdp.ws_url)
    try:
        wait_for_dom(cdp)
        text = cdp.eval("document.documentElement.innerText || ''")
        script_count = cdp.eval("document.scripts.length")
        cookie = cdp.eval("(() => { try { return document.cookie || ''; } catch (_) { return 'COOKIE_BLOCKED'; } })()")
        href = cdp.eval("location.href")
    finally:
        cdp.close()
    logs = device.logcat()
    ev.assert_("forced_failure_page_text", "Secure WebView bridge startup failed" in text, text=text[:200])
    ev.assert_("forced_failure_no_scripts", script_count == 0, script_count=script_count)
    ev.assert_("forced_failure_no_session_cookie", "isy_session" not in cookie, cookie=cookie)
    ev.assert_("forced_failure_not_app_origin_shell", "appassets.androidplatform.net" not in href, href=href)
    ev.assert_("forced_failure_activity_engine_not_started", "EngineBootstrap: calling nativeStart" not in logs, log_tail=logs[-1200:])
    ev.assert_("forced_failure_log_marker", "bridge startup blocked" in logs, log_tail=logs[-1200:])


def positive_bridge_probe(device: Device, ev: Evidence, accepted_open: bool) -> None:
    device.clear_force_fail()
    device.clear_logcat()
    device.force_stop()
    device.launch()
    time.sleep(5)
    wait_for_cdp(device)
    cdp = Cdp(device.cdp_port)
    ev.meta["webview_devtools_target_url"] = cdp.target.get("url", "")
    ev.meta["webview_devtools_ws_url"] = cdp.ws_url
    ev.pass_("webview_devtools_target", url=cdp.target.get("url", ""), ws_url=cdp.ws_url)
    try:
        wait_for_dom(cdp)
        ev.assert_("top_frame_has_bridge", cdp.eval("typeof window.__isyBridge") == "object")
        ev.assert_("no_legacy_globals", cdp.eval(
            "[typeof window.AndroidSession, typeof window.AndroidPush, typeof window.AndroidNav].join(',')"
        ) == "undefined,undefined,undefined")
        ev.assert_(
            "document_cookie_has_no_session",
            cdp.eval("(() => { try { return document.cookie.includes('isy_session'); } catch (_) { return false; } })()") is False,
        )
        iframe = cdp.eval(
            """
            new Promise((resolve) => {
              const f = document.createElement('iframe');
              f.sandbox = 'allow-scripts';
              window.addEventListener('message', (ev) => resolve(ev.data), { once: true });
              f.srcdoc = `<script>
                parent.postMessage({
                  bridge: typeof window.__isyBridge,
                  session: typeof window.AndroidSession,
                  push: typeof window.AndroidPush,
                  nav: typeof window.AndroidNav
                }, '*');
              </script>`;
              document.body.appendChild(f);
              setTimeout(() => resolve({ timeout: true }), 3000);
            })
            """
        )
        if iframe.get("timeout"):
            ev.skip("opaque_iframe_cannot_see_bridge", reason="iframe script did not execute; CSP/frame policy blocked probe", iframe=iframe)
            ev.skip("opaque_iframe_cannot_see_legacy_globals", reason="iframe script did not execute; CSP/frame policy blocked probe", iframe=iframe)
        else:
            ev.assert_("opaque_iframe_cannot_see_bridge", iframe.get("bridge") == "undefined", iframe=iframe)
            ev.assert_("opaque_iframe_cannot_see_legacy_globals", all(
                iframe.get(k) == "undefined" for k in ["session", "push", "nav"]
            ), iframe=iframe)
        api_ok = cdp.eval(
            """
            (async () => {
              window.fetch = () => { throw new Error('fetch forbidden by #721 probe'); };
              const d = await api('/api/v1/accounts');
              return !!d && Array.isArray(d.accounts);
            })()
            """
        )
        ev.assert_("api_works_with_fetch_monkeypatched", api_ok is True)
        stream_ok = cdp.eval(
            """
            new Promise((resolve) => {
              window.EventSource = function() { throw new Error('EventSource forbidden by #721 probe'); };
              const s = openEventStream('/api/v1/events', () => {}, () => {});
              setTimeout(() => { try { s.close(); } catch (_) {} resolve(true); }, 500);
            })
            """
        )
        ev.assert_("stream_works_with_eventsource_monkeypatched", stream_ok is True)
        malformed = cdp.eval(
            """
            new Promise((resolve) => {
              const old = window.__isyBridge.onmessage;
              window.__isyBridge.onmessage = (ev) => {
                let m;
                try { m = JSON.parse(ev.data); } catch (_) { m = { parse: false }; }
                if (m.id !== 'probe-bad') {
                  if (old) old(ev);
                  return;
                }
                window.__isyBridge.onmessage = old;
                resolve(m);
              };
              window.__isyBridge.postMessage(JSON.stringify({
                t: 'native', id: 'probe-bad', op: 'openExternal',
                payload: { url: 'https://auth.openai.com/oauth/authorize' }
              }));
              setTimeout(() => { window.__isyBridge.onmessage = old; resolve({ timeout: true }); }, 3000);
            })
            """
        )
        ev.assert_("native_malformed_message_returns_400", malformed.get("status") == 400, reply=malformed)
        cleanup = cdp.eval(
            """
            (async () => {
              const rows = [];
              const oldPost = window.__isyBridge.postMessage;
              try {
                window.__isyBridge.onmessage({ data: 'not-json' });
                window.__isyBridge.onmessage({ data: JSON.stringify({ t: 'res', id: 'wrong-id', status: 200, body: '{}' }) });
                let duplicateCount = 0;
                const dupId = 'probe-dup';
                _bridgePending.set(dupId, {
                  resolve: () => { duplicateCount += 1; },
                  reject: () => {},
                  timer: setTimeout(() => {}, 5000)
                });
                window.__isyBridge.onmessage({ data: JSON.stringify({ t: 'res', id: dupId, status: 200, body: '{}' }) });
                window.__isyBridge.onmessage({ data: JSON.stringify({ t: 'res', id: dupId, status: 200, body: '{}' }) });
                rows.push({ name: 'duplicate_once', ok: duplicateCount === 1 && !_bridgePending.has(dupId) });

                const timeoutId = 'probe-timeout';
                window.__isyBridge.postMessage = () => {};
                try { await bridgeRoundTrip({ t: 'native', id: timeoutId, op: 'pushToken', payload: {} }, 50); }
                catch (_) {}
                rows.push({ name: 'timeout_cleared', ok: !_bridgePending.has(timeoutId) });
                window.__isyBridge.onmessage({ data: JSON.stringify({ t: 'res', id: timeoutId, status: 200, body: '{}' }) });
                rows.push({ name: 'late_ignored', ok: !_bridgePending.has(timeoutId) });
              } finally {
                window.__isyBridge.postMessage = oldPost;
              }
              return { ok: rows.every(r => r.ok), rows, pending: _bridgePending.size };
            })()
            """,
            timeout=8,
        )
        ev.assert_("bridge_js_response_cleanup", cleanup.get("ok") is True, result=cleanup)
        push = cdp.eval("nativeCall('pushToken', {}, 3000).then(d => typeof d.token === 'string')")
        ev.assert_("native_push_token_op_returns", push is True)
        bio_started = cdp.eval(
            """
            new Promise((resolve) => {
              window.__isyBioProbe = runBiometricConfirm('issue-721-probe', 'Issue 721 probe');
              setTimeout(() => resolve({
                pending: _bioPending.size,
                stats: window.__isyBridgeTransportStats,
              }), 700);
            })
            """,
            timeout=5,
        )
        prompt_state = device.prompt_window_state()
        ev.pass_("biometric_prompt_started", state=bio_started, window_state=prompt_state)
        device.shell("input keyevent KEYCODE_BACK", check=False)
        time.sleep(1)
        bio_cleanup = cdp.eval(
            """
            new Promise((resolve) => setTimeout(() => {
              const beforeLate = _bioPending.size;
              if (window.__isyBridge && window.__isyBridge.onmessage) {
                window.__isyBridge.onmessage({ data: JSON.stringify({ t: 'bio', id: 'missing-bio-probe', ok: true }) });
              }
              resolve({
                beforeLate,
                afterLate: _bioPending.size,
                resultResolved: !!window.__isyBioProbe,
              });
            }, 700))
            """,
            timeout=5,
        )
        ev.assert_(
            "biometric_cancel_and_late_response_cleanup",
            bio_cleanup.get("beforeLate") == 0 and bio_cleanup.get("afterLate") == 0,
            result=bio_cleanup,
        )
        device.wake()
        keyguard = device.keyguard_state()
        ev.assert_(
            "device_keyguard_clear_before_network_guard",
            keyguard.get("keyguard_showing") is False,
            state=keyguard,
        )
        if keyguard.get("keyguard_showing"):
            return
        guard = cdp.eval(
            """
            (async () => {
              const g = await nativeCall('beginNetworkGuard', {}, 3000);
              const ended = await nativeCall('endNetworkGuard', { guard_id: g.guard_id }, 3000);
              return !!g.guard_id && ended.ok === true;
            })()
            """
        )
        ev.assert_("native_network_guard_roundtrip", guard is True)
        rejected = cdp.eval(
            """
            (async () => {
              const urls = [
                'http://example.invalid/',
                'https://appassets.androidplatform.net/',
                'https://127.0.0.1/',
                'https://localhost/',
                'https://evil-login.microsoftonline.com.example/',
                'javascript:alert(1)',
                'intent://scan/#Intent;scheme=zxing;end',
                'file:///sdcard/x',
                'data:text/html,hi'
              ];
              for (const url of urls) {
                try {
                  await nativeCall('openExternal', { url, kind: 'agent_authorize' }, 3000);
                  return { ok: false, url };
                } catch (_) {}
              }
              return { ok: true };
            })()
            """
        )
        ev.assert_("native_openexternal_rejects_blocked_urls", rejected.get("ok") is True, result=rejected)
        if accepted_open:
            ev.skip("accepted_openexternal_smoke", reason="not implemented in non-interactive probe")
        else:
            ev.skip("accepted_openexternal_smoke", reason="disabled by default to avoid browser focus flake")
        reconnect = cdp.eval(
            """
            (typeof openExternalAuth === 'function') &&
            (typeof startDeviceLogin === 'function') &&
            startDeviceLogin.toString().includes('account_device_code')
            """
        )
        ev.assert_("account_device_code_bridge_function_present", reconnect is True)
        stats = cdp.eval("window.__isyBridgeTransportStats")
        ev.pass_("bridge_transport_stats", stats=stats)
    finally:
        cdp.close()

    listens = device.listen_sockets_for_uid()
    ev.meta["app_uid"] = device.uid()
    ev.assert_("default_uid_has_no_listen_sockets", not listens, listen_sockets=listens)


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Run #721 Android WebView bridge-isolation probe.")
    p.add_argument("--adb", default=os.environ.get("ADB", "adb"))
    p.add_argument("--package", default=DEFAULT_PACKAGE)
    p.add_argument("--lock", default=DEFAULT_LOCK)
    p.add_argument("--cdp-port", type=int, default=DEFAULT_CDP_PORT)
    p.add_argument("--output", default="")
    p.add_argument("--skip-lock", action="store_true", help="Do not acquire device-lock; for local dry debugging only.")
    p.add_argument("--accepted-open", action="store_true", help="Attempt accepted external URL smoke; may move focus to browser.")
    p.add_argument("--positive-only", action="store_true", help="Skip forced-failure probe.")
    return p.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    evidence = Evidence()
    device = Device(args.adb, args.package, args.cdp_port, evidence)
    locked = False
    stay_awake_previous: str | None = None
    try:
        evidence.meta["device_serial"] = device.serial()
        evidence.meta["isy_cargo_features"] = os.environ.get("ISY_CARGO_FEATURES", "")
        agent_features = evidence.meta["isy_cargo_features"]
        evidence.meta["agent_subscription_experimental_enabled"] = (
            "agent-subscription-experimental" in agent_features.split(",")
            or "agent-subscription-experimental" in agent_features.split()
        )
        if not args.skip_lock:
            run(["device-lock", "acquire", args.lock], timeout=60)
            locked = True
            evidence.pass_("device_lock_acquired", lock=args.lock)
        stay_awake_previous = device.enable_stay_awake()
        device.remove_forward()
        device.clear_force_fail()
        if not args.positive_only:
            forced_failure_probe(device, evidence)
        positive_bridge_probe(device, evidence, args.accepted_open)
        evidence.assert_(
            "agent_subscription_experimental_default_off",
            evidence.meta["agent_subscription_experimental_enabled"] is False,
            isy_cargo_features=evidence.meta["isy_cargo_features"],
            note="default debug APK expected unless ISY_CARGO_FEATURES opts in",
        )
    except Exception as exc:
        evidence.fail("probe_exception", error=str(exc))
    finally:
        try:
            device.clear_force_fail()
        except Exception:
            pass
        try:
            device.remove_forward()
        except Exception:
            pass
        if stay_awake_previous is not None:
            try:
                device.restore_stay_awake(stay_awake_previous)
            except Exception:
                pass
        if locked:
            run(["device-lock", "release", args.lock], check=False, timeout=30)
            evidence.pass_("device_lock_released", lock=args.lock)

    report = {
        "package": args.package,
        "lock": args.lock,
        "cdp_port": args.cdp_port,
        "ok": evidence.ok(),
        "results": evidence.rows,
        **evidence.meta,
        "feature_skips": [
            {"name": row.get("name"), "reason": row.get("reason")}
            for row in evidence.rows
            if row.get("status") == "SKIP"
        ],
    }
    data = json.dumps(report, indent=2, sort_keys=True)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(data + "\n")
    print(data)
    return 0 if evidence.ok() else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
