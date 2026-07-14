// Deterministic mobile-bridge UI smoke for #640 connectivity diagnostics.
import { chromium } from "playwright";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const AGENT_CAP = "fixture-agent-cap";

function asset(relative) {
  return fs.readFileSync(path.join(ROOT, relative));
}

function serve() {
  const index = asset("gui/webui/src/index.html");
  const css = asset("gui/webui/src/app.css");
  const font = asset("gui/webui/src/assets/inter-var.woff2");
  const js = asset("gui/webui/src/app.js").toString("utf8").replace(
    /__([A-Z0-9_]+_CAP_TOKEN)__/g,
    (_match, token) => token === "AGENT_CAP_TOKEN" ? AGENT_CAP : "",
  );
  return http.createServer((req, res) => {
    const pathname = new URL(req.url, "http://127.0.0.1").pathname;
    const table = {
      "/": ["text/html; charset=utf-8", index],
      "/app.css": ["text/css; charset=utf-8", css],
      "/app.js": ["application/javascript; charset=utf-8", Buffer.from(js)],
      "/app.woff2": ["font/woff2", font],
    };
    const found = table[pathname];
    if (!found) {
      res.writeHead(pathname === "/favicon.ico" ? 204 : 404);
      return res.end();
    }
    res.writeHead(200, { "content-type": found[0], "cache-control": "no-store" });
    res.end(found[1]);
  });
}

function assert(report, name, passed, detail = null) {
  report.assertions.push({ name, passed, detail });
  if (!passed) throw new Error(`assertion failed: ${name}`);
}

async function main() {
  const output = process.argv[2] || "/tmp/isyncyou-640-ui-smoke.json";
  const report = { schema_version: 1, ok: false, assertions: [], native_settings_hints: [], preflight_calls: 0 };
  const server = serve();
  let browser;
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const origin = `http://127.0.0.1:${server.address().port}`;
    browser = await chromium.launch();
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.addInitScript(({ cap }) => {
      localStorage.setItem("isy_agent_privacy_consent_v1", JSON.stringify({ version: 1, accepted: true, provider: "claude" }));
      const state = { preflight: 0, settings: [], streamId: null };
      window.__networkSmokeState = state;
      const reply = (message) => setTimeout(() => window.__isyBridge.onmessage({ data: JSON.stringify(message) }), 0);
      const body = (value) => JSON.stringify(value);
      window.__isyBridge = {
        postMessage(raw) {
          const msg = JSON.parse(raw);
          if (msg.t === "native") {
            let result = {};
            if (msg.op === "beginNetworkGuard") result = { guard_id: "guard-fixture" };
            else if (msg.op === "captureNetworkSnapshot") result = { snapshot_id: "snapshot-fixture" };
            else if (msg.op === "bindNetworkGuard") result = { ok: true };
            else if (msg.op === "openNetworkSettings") {
              state.settings.push(msg.payload.hint);
              result = { ok: true };
            } else if (msg.op === "endNetworkGuard" || msg.op === "pushToken") result = { ok: true };
            reply({ t: "res", id: msg.id, status: 200, body: body(result) });
            return;
          }
          if (msg.t === "req") {
            const url = new URL(msg.path, "https://appassets.androidplatform.net");
            let value = {};
            if (url.pathname === "/api/v1/accounts") value = { accounts: [{ id: "fixture", username: "fixture" }] };
            else if (url.pathname === "/api/v1/status") value = { services: [], totals: { items: 0, archived: 0 } };
            else if (url.pathname === "/api/v1/activity") value = { runs: [] };
            else if (url.pathname === "/api/v1/settings") value = { accounts: [{ id: "fixture" }] };
            else if (url.pathname === "/api/v1/sync/state") value = { enabled: false, paused: false };
            else if (url.pathname === "/api/v1/agent/status") value = {
              enabled: true, connected: true, provider: "claude", selected_provider: "claude",
              model: "claude-sonnet-4", claude: true, codex: false,
              credential_state: { claude: "connected", codex: "unconfigured" },
              onboarding: {
                selected_provider: "claude",
                providers: {
                  claude: { state: "ready", steps: [] },
                  codex: { state: "not_started", steps: [] },
                },
              },
              models: { claude: [{ id: "claude-sonnet-4", label: "Claude" }], codex: [] },
            };
            else if (url.pathname === "/api/v1/agent/connectivity/preflight") {
              state.preflight += 1;
              value = state.preflight === 1
                ? { status: "action_required", code: "no_validated_network", retryable: true, settings_hint: "internet_panel" }
                : { status: "ready", code: "ready", retryable: false, settings_hint: "none" };
            } else if (url.pathname === "/api/v1/agent/turn") value = { turn: "turn-fixture" };
            reply({ t: "res", id: msg.id, status: 200, body: body(value) });
            return;
          }
          if (msg.t === "sub") {
            state.streamId = msg.id;
            setTimeout(() => reply({ t: "evt", id: msg.id, ev: { event: "message", data: body({ event: "token", text: "Recovered after retry." }) } }), 20);
            setTimeout(() => reply({ t: "evt", id: msg.id, ev: { event: "message", data: body({ event: "done", reason: "complete" }) } }), 40);
          }
        },
        onmessage: null,
      };
      window.__fixtureCap = cap;
    }, { cap: AGENT_CAP });

    await page.goto(`${origin}/#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="agent-input"]');
    await page.locator('[data-testid="agent-input"]').fill("Run the network diagnostic fixture");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForSelector('[data-agent-connectivity="no_validated_network"]');
    assert(
      report,
      "safe code-specific diagnostic rendered",
      await page.locator('[data-agent-connectivity="no_validated_network"] p').isVisible(),
    );
    assert(report, "retry action rendered", await page.locator('[data-agent-connectivity-retry="1"]').isVisible());
    assert(report, "settings action rendered with closed hint", await page.locator('[data-agent-connectivity-settings="internet_panel"]').isVisible());
    await page.locator('[data-agent-connectivity-settings="internet_panel"]').click();
    await page.locator('[data-agent-connectivity-retry="1"]').click();
    await page.waitForFunction(() => document.body.innerText.includes("Recovered after retry."));
    const state = await page.evaluate(() => window.__networkSmokeState);
    report.preflight_calls = state.preflight;
    report.native_settings_hints = state.settings;
    assert(report, "settings uses only internet_panel", JSON.stringify(state.settings) === '["internet_panel"]');
    assert(report, "retry performs a second preflight", state.preflight === 2);
    assert(report, "turn starts only after ready preflight", (await page.getByText("Recovered after retry.").count()) > 0);
    report.ok = true;
  } catch (error) {
    report.error = String(error && error.stack ? error.stack : error);
  } finally {
    if (browser) await browser.close().catch(() => {});
    await new Promise((resolve) => server.close(resolve));
    fs.writeFileSync(output, JSON.stringify(report, null, 2) + "\n");
  }
  if (!report.ok) console.error(report.error);
  else console.log(`agent-network-ui-smoke: ${report.assertions.length} assertions passed`);
  process.exit(report.ok ? 0 : 1);
}

await main();
