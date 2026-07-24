// #639 T10: focused first-run handoff-wizard smoke.
//
// The full #622 assistant smoke (tools/agent-ui-smoke.mjs) drives the OAuth
// redirect -> chat transition, which needs a browser flow that is not available
// in every headless sandbox. This harness verifies ONLY the #639 wizard states
// (first-run ordered steps, reconnect short flow, and secret-free DOM/storage)
// against a mocked host status, so the wizard is verifiable without any OAuth
// transition. It writes a JSON evidence report.
import { chromium } from "playwright";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const REPO = path.resolve(path.dirname(__filename), "..");
const outFlag = process.argv.indexOf("--out");
if (outFlag >= 0 && !process.argv[outFlag + 1]) throw new Error("--out requires a directory");
const OUT_DIR = outFlag >= 0
  ? path.resolve(REPO, process.argv[outFlag + 1])
  : process.env.ISY_WIZARD_OUT || path.join(REPO, "docs/evidence/artifacts/issue-639");
const AGENT_CAP = "fixture-agent-cap";
const readText = (p) => fs.readFileSync(path.join(REPO, p), "utf8");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const STEP_KEYS = [
  "official_oauth_completed", "credential_encrypted", "retained_envelope_verified",
  "default_harness_removed", "m365_profile_activated", "isyncyou_tool_connected",
  "subscription_identity_set", "ready",
];

function onboardingNode(state, complete) {
  return { state, steps: STEP_KEYS.map((key) => ({ key, complete })) };
}

// A per-scenario host status: first_run (nothing connected) or reconnect_required.
function statusFor(scenario) {
  const claude = scenario === "reconnect"
    ? onboardingNode("reconnect_required", false)
    : onboardingNode("not_started", false);
  return {
    enabled: true,
    connected: false,
    provider: "claude",
    selected_provider: "claude",
    model: "",
    claude: false,
    codex: false,
    credential_state: { claude: scenario === "reconnect" ? "reconnect_required" : "unconfigured", codex: "unconfigured" },
    onboarding: {
      selected_provider: "claude",
      selected_state: scenario === "reconnect" ? "reconnect_required" : "not_started",
      providers: { claude, codex: onboardingNode("not_started", false) },
    },
    models: { claude: [{ id: "claude-opus-4", label: "Claude Opus 4" }], codex: [{ id: "gpt-5-codex", label: "GPT-5 Codex" }] },
  };
}

function text(res, status, body, ct) {
  const data = Buffer.from(body);
  res.writeHead(status, { "content-type": ct, "content-length": String(data.length), "cache-control": "no-store" });
  res.end(data);
}
function json(res, status, body) {
  text(res, status, JSON.stringify(body), "application/json; charset=utf-8");
}

function startServer(scenario) {
  const appJs = readText("gui/webui/src/app.js").replace(
    /__([A-Z0-9_]+_CAP_TOKEN)__/g,
    (_m, token) => (token === "AGENT_CAP_TOKEN" ? AGENT_CAP : ""),
  );
  const indexHtml = readText("gui/webui/src/index.html");
  const appCss = readText("gui/webui/src/app.css");
  const requests = [];
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    if (req.method === "GET" && url.pathname === "/") return text(res, 200, indexHtml, "text/html; charset=utf-8");
    if (req.method === "GET" && url.pathname === "/app.css") return text(res, 200, appCss, "text/css; charset=utf-8");
    if (req.method === "GET" && url.pathname === "/app.js") return text(res, 200, appJs, "text/javascript; charset=utf-8");
    if (url.pathname === "/api/v1/agent/status") return json(res, 200, statusFor(scenario));
    if (url.pathname === "/api/v1/agent/connectivity/preflight") return json(res, 200, { status: "ready", code: "ready", retryable: false, settings_hint: "none" });
    if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/start") {
      if (scenario === "invalid_oauth_start") {
        return json(res, 200, { attempt_id: "attempt-invalid-response" });
      }
      return json(res, 200, {
        attempt_id: "attempt-fixture",
        authorize_url: `${url.origin}/fixture-auth`,
      });
    }
    if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/complete") {
      let body = "";
      for await (const chunk of req) body += chunk;
      let parsed = null;
      try { parsed = JSON.parse(body); } catch (_) {}
      requests.push({
        route: "oauth_complete",
        json: !!parsed,
        provider: parsed && parsed.provider,
        attempt_id_present: !!(parsed && parsed.attempt_id),
        pasted_code_present: !!(parsed && parsed.pasted_code),
        query_empty: url.search === "",
      });
      return json(res, 200, { connected: true });
    }
    if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/cancel") {
      return json(res, 200, { cancelled: true });
    }
    return json(res, 404, { error: "not found" });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port, requests })));
}

async function openAssistant(page, origin) {
  await page.goto(`${origin}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.nav-item[data-service="assistant"]', { timeout: 10000 });
  await page.locator('.nav-item[data-service="assistant"]').first().click();
  await page.waitForSelector('[data-testid="agent-setup"]', { timeout: 10000 });
}

async function main() {
  fs.mkdirSync(OUT_DIR, { recursive: true });
  const evidence = { issue: 639, task: "T10", assertions: [] };
  const record = (name, ok, details) => { evidence.assertions.push({ name, ok, details }); if (!ok) console.log(`FAIL: ${name}`, JSON.stringify(details || {})); else console.log(`PASS: ${name}`); };
  const browser = await chromium.launch();
  try {
    // --- AC1: first-run wizard renders the ordered 8 steps.
    {
      const { server, port } = await startServer("first_run");
      const origin = `http://127.0.0.1:${port}`;
      const page = await browser.newPage();
      await openAssistant(page, origin);
      const count = await page.locator('[data-testid="agent-wizard-steps"] [data-agent-wizard-step]').count();
      record("first-run wizard renders 8 steps", count === 8, { count });
      const order = await page.evaluate(() => Array.from(document.querySelectorAll('[data-testid="agent-wizard-steps"] [data-agent-wizard-step]')).map((n) => n.getAttribute("data-agent-wizard-step")));
      record("wizard steps are the ordered handoff sequence", order.join(",") === STEP_KEYS_STR, order);
      record("wizard chat surface absent on first run", (await page.locator('[data-testid="agent-transcript"]').count()) === 0);
      await page.close();
      server.close();
    }
    // --- AC2: reconnect uses the short flow (no full step list; reconnect affordance).
    {
      const { server, port } = await startServer("reconnect");
      const origin = `http://127.0.0.1:${port}`;
      const page = await browser.newPage();
      await openAssistant(page, origin);
      const stepsCount = await page.locator('[data-testid="agent-wizard-steps"] [data-agent-wizard-step]').count();
      record("reconnect flow condenses the full step list", stepsCount === 0, { stepsCount });
      const wizardState = await page.locator('[data-agent-wizard]').first().getAttribute("data-agent-wizard");
      record("reconnect wizard state is reconnect_required", wizardState === "reconnect_required", { wizardState });
      const connectLabel = await page.locator('#asst-connect-claude').innerText();
      record("reconnect surfaces a reconnect affordance", /reconnect/i.test(connectLabel), { connectLabel });
      await page.close();
      server.close();
    }
    // --- AC3: DOM, storage, and console carry no secret after a simulated code paste.
    {
      const { server, port, requests } = await startServer("first_run");
      const origin = `http://127.0.0.1:${port}`;
      const page = await browser.newPage();
      const consoleText = [];
      page.on("console", (m) => consoleText.push(m.text()));
      await openAssistant(page, origin);
      // Render the manual code step and paste a distinctive secret into the password input.
      const SECRET = "SECRET-PASTED-CODE-abc123#state-xyz";
      await page.evaluate(() => { OAUTH_ATTEMPTS.set("claude", "attempt-fixture"); showCodeStep(); });
      await page.waitForSelector('#asst-code', { timeout: 5000 });
      const inputType = await page.locator('#asst-code').getAttribute("type");
      record("manual code input is type=password", inputType === "password", { inputType });
      await page.locator('#asst-code').fill(SECRET);
      const domHtml = await page.evaluate(() => document.documentElement.outerHTML);
      record("pasted code is not serialized into the DOM", !domHtml.includes(SECRET));
      const storage = await page.evaluate(() => JSON.stringify({ local: { ...localStorage }, session: { ...sessionStorage } }));
      record("pasted code is not in localStorage/sessionStorage", !storage.includes(SECRET), { keys: Object.keys(JSON.parse(storage).local) });
      record("no secret in console output", !consoleText.join("\n").includes(SECRET));
      await page.getByRole("button", { name: "Finish connecting" }).click();
      await page.waitForFunction(() => !document.getElementById("asst-code") || document.getElementById("asst-code").value === "");
      record("completion path clears the code input", true);
      record("completion posts one strict JSON body with no query secret",
        requests.length === 1
        && requests[0].route === "oauth_complete"
        && requests[0].json
        && requests[0].provider === "claude"
        && requests[0].attempt_id_present
        && requests[0].pasted_code_present
        && requests[0].query_empty,
        { request_count: requests.length, contract: requests[0] || null });
      await page.close();
      server.close();
    }
    // --- AC4: an incomplete OAuth-start response cancels its attempt and releases its guard.
    {
      const { server, port } = await startServer("invalid_oauth_start");
      const origin = `http://127.0.0.1:${port}`;
      const page = await browser.newPage();
      await openAssistant(page, origin);
      await page.evaluate(() => {
        localStorage.setItem("isy_agent_privacy_consent_v1", JSON.stringify({
          version: 1,
          accepted: true,
          provider: "claude",
        }));
        window.__wizardGuardEvents = [];
        beginNetworkGuard = async () => {
          window.__wizardGuardEvents.push("begin");
          return "guard-invalid-response";
        };
        endNetworkGuard = async (guardId) => {
          window.__wizardGuardEvents.push(`end:${guardId}`);
        };
        runConnectivityPreflight = async () => ({ status: "ready" });
      });
      await page.evaluate(() => startAiLogin("claude"));
      const cleanup = await page.evaluate(() => ({
        events: window.__wizardGuardEvents,
        attempt_retained: OAUTH_ATTEMPTS.has("claude"),
        guard_retained: AGENT_GUARD_ID !== null,
      }));
      record("invalid OAuth start response releases the exact guard",
        cleanup.events.join(",") === "begin,end:guard-invalid-response", cleanup);
      record("invalid OAuth start response clears the server attempt",
        cleanup.attempt_retained === false, cleanup);
      record("invalid OAuth start response clears local guard ownership",
        cleanup.guard_retained === false, cleanup);
      await page.close();
      server.close();
    }
  } finally {
    await browser.close();
  }
  fs.writeFileSync(path.join(OUT_DIR, "wizard-smoke.json"), JSON.stringify(evidence, null, 2));
  const failed = evidence.assertions.filter((a) => !a.ok);
  console.log(`\n${evidence.assertions.length - failed.length}/${evidence.assertions.length} wizard assertions passed`);
  if (failed.length) process.exit(1);
}

const STEP_KEYS_STR = STEP_KEYS.join(",");
await sleep(0);
await main();
