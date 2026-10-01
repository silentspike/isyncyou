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
  let completionAccepted = false;
  let statusFailureSent = false;
  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    if (req.method === "GET" && url.pathname === "/") return text(res, 200, indexHtml, "text/html; charset=utf-8");
    if (req.method === "GET" && url.pathname === "/app.css") return text(res, 200, appCss, "text/css; charset=utf-8");
    if (req.method === "GET" && url.pathname === "/app.js") return text(res, 200, appJs, "text/javascript; charset=utf-8");
    if (url.pathname === "/api/v1/agent/status") {
      if (completionAccepted && scenario === "completion_status_failure" && !statusFailureSent) {
        statusFailureSent = true;
        return json(res, 503, { error: "status_unavailable" });
      }
      const status = statusFor(scenario);
      if (completionAccepted && ["completion_success", "completion_status_failure"].includes(scenario)) {
        status.connected = true;
        status.claude = true;
        status.credential_state.claude = "ready";
        status.onboarding.providers.claude = onboardingNode("ready", true);
        status.onboarding.selected_state = "ready";
      }
      return json(res, 200, status);
    }
    if (url.pathname === "/api/v1/agent/connectivity/preflight") return json(res, 200, { status: "ready", code: "ready", retryable: false, settings_hint: "none" });
    if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/start") {
      requests.push({ route: "oauth_start" });
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
      if (scenario === "completion_rejected") return json(res, 400, { error: "oauth complete failed" });
      completionAccepted = true;
      return json(res, 200, { connected: true });
    }
    if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/cancel") {
      requests.push({ route: "oauth_cancel" });
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
    for (const scenario of ["completion_success", "completion_rejected", "completion_status_failure", "missing_attempt"]) {
      const { server, port, requests } = await startServer(scenario);
      const page = await browser.newPage();
      try {
        await openAssistant(page, `http://127.0.0.1:${port}`);
        await page.evaluate(scenario => {
          window.__completionMessages = [];
          toast = message => window.__completionMessages.push(message);
          window.__completionGuards = [];
          AGENT_GUARD_ID = "guard-completion";
          endNetworkGuard = async id => window.__completionGuards.push(id);
          if (scenario !== "missing_attempt") OAUTH_ATTEMPTS.set("claude", "attempt-fixture");
          showCodeStep();
          const original = postJson;
          postJson = async (path, cap, body) => {
            if (path === "/api/v1/agent/oauth/complete") {
              await new Promise(resolve => { window.__releaseCompletion = resolve; });
            }
            return original(path, cap, body);
          };
        }, scenario);
        await page.locator("#asst-code").fill("fixture-code#fixture-state");
        await page.getByRole("button", { name: "Finish connecting" }).click();
        if (scenario !== "missing_attempt") {
          await page.waitForFunction(() => !!window.__releaseCompletion);
          record(`${scenario} completion shows immediate progress and disables code controls`,
            await page.evaluate(() => document.querySelector("[data-agent-oauth-opening]")?.textContent === "Finishing sign-in…"
              && [...document.querySelectorAll("#asst-connect-card input, #asst-connect-card button")].every(node => node.disabled)
              && document.getElementById("asst-code").value === ""));
          await page.evaluate(async () => {
            await completeAiLogin();
            await renderAssistantView(document.getElementById("view"));
          });
          record(`${scenario} completion stays locked through rerender`,
            await page.evaluate(() => document.querySelector("[data-agent-oauth-opening]")?.textContent === "Finishing sign-in…"
              && [...document.querySelectorAll("#asst-connect-card input, #asst-connect-card button")].every(node => node.disabled)));
          await page.evaluate(() => window.__releaseCompletion());
        }
        await page.waitForFunction(() => AssistantState.oauthOpening === null && !document.querySelector("#asst-code"));
        record(`${scenario} no obsolete code form or attempt remains`,
          await page.evaluate(() => !OAUTH_ATTEMPTS.has("claude") && !document.querySelector("[data-agent-oauth-opening]")));
        record(`${scenario} completion is sent at most once`,
          requests.filter(r => r.route === "oauth_complete").length === (scenario === "missing_attempt" ? 0 : 1));
        if (scenario === "completion_status_failure") {
          record("accepted completion with status failure is not cancelled or mislabeled as a failed login",
            !requests.some(r => r.route === "oauth_cancel")
              && await page.evaluate(() => window.__completionMessages.some(m => m.startsWith("Sign-in completed."))
                && !window.__completionMessages.some(m => m.includes("Couldn't connect"))));
        }
        if (scenario === "completion_rejected" || scenario === "missing_attempt") {
          record(`${scenario} clears its attempt and releases the exact guard`,
            requests.filter(r => r.route === "oauth_cancel").length === (scenario === "missing_attempt" ? 0 : 1)
              && await page.evaluate(() => AGENT_GUARD_ID === null && window.__completionGuards.length === 1));
        }
      } finally {
        await page.close();
        await new Promise(resolve => server.close(resolve));
      }
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
    // Delayed boundaries prove feedback before any network/native work completes.
    for (const provider of ["claude", "codex"]) {
      for (const width of [390, 1280]) {
        const { server, port, requests } = await startServer("first_run");
        const page = await browser.newPage({ viewport: { width, height: 844 } });
        try {
          await openAssistant(page, `http://127.0.0.1:${port}`);
          await page.evaluate(async () => {
            localStorage.setItem(AGENT_PRIVACY_CONSENT_KEY, JSON.stringify({
              version: AGENT_PRIVACY_CONSENT_VERSION,
              providers: { claude: { accepted: true }, codex: { accepted: true } },
            }));
            window.__openingTest = { guards: 0, browsers: 0 };
            beginNetworkGuard = () => {
              window.__openingTest.guards++;
              return new Promise(resolve => { window.__openingTest.guard = resolve; });
            };
            runConnectivityPreflight = () => new Promise(resolve => {
              window.__openingTest.preflight = resolve;
            });
            openExternalAuth = () => {
              window.__openingTest.browsers++;
              return new Promise(resolve => { window.__openingTest.browser = resolve; });
            };
            await renderAssistantView(document.getElementById("view"));
          });
          const label = `${provider}/${width}`;
          await page.locator(`#asst-connect-${provider}`).click();
          const immediate = await page.evaluate(() => ({
            status: document.querySelector("[data-agent-oauth-opening]")?.textContent,
            disabled: ["claude", "codex"].every(p => document.getElementById(`asst-connect-${p}`).disabled),
            busy: document.querySelector('[aria-busy="true"]') !== null,
            guards: window.__openingTest.guards,
          }));
          record(`${label} feedback precedes guard completion`,
            immediate.status === "Preparing sign-in…" && immediate.disabled
              && immediate.busy && immediate.guards === 1, immediate);
          await page.evaluate(async () => {
            await connectAgentProvider("codex");
            await startAiLogin("claude");
            await renderAssistantView(document.getElementById("view"));
          });
          record(`${label} duplicate start is ignored across rerender`,
            await page.evaluate(() => window.__openingTest.guards === 1
              && ["claude", "codex"].every(p => document.getElementById(`asst-connect-${p}`).disabled)
              && document.querySelector("[data-agent-oauth-opening]")?.textContent === "Preparing sign-in…"));
          await page.evaluate(() => window.__openingTest.guard(null));
          await page.waitForFunction(() => !!window.__openingTest.preflight);
          record(`${label} network phase is visible`,
            await page.locator("[data-agent-oauth-opening]").innerText() === "Checking connection…");
          await page.evaluate(() => window.__openingTest.preflight({ status: "ready" }));
          await page.waitForFunction(() => !!window.__openingTest.browser);
          record(`${label} browser phase is visible`,
            await page.locator("[data-agent-oauth-opening]").innerText() === "Opening browser…");
          const bounds = await page.locator("[data-agent-oauth-opening]").evaluate(node => {
            const r = node.getBoundingClientRect();
            return r.width > 0 && r.left >= 0 && r.right <= innerWidth && node.scrollWidth <= node.clientWidth;
          });
          record(`${label} progress fits viewport`, bounds);
          await page.screenshot({ path: path.join(OUT_DIR, `oauth-opening-${provider}-${width}.png`), fullPage: true });
          await page.evaluate(() => window.__openingTest.browser());
          await page.waitForFunction(() => AssistantState.oauthOpening === null);
          record(`${label} handoff clears opening state`,
            await page.locator("[data-agent-oauth-opening]").count() === 0);
          record(`${label} one start and one browser handoff`,
            requests.filter(r => r.route === "oauth_start").length === 1
              && await page.evaluate(() => window.__openingTest.browsers === 1));
        } finally {
          await page.close();
          await new Promise(resolve => server.close(resolve));
        }
      }
    }
    for (const boundary of ["guard", "preflight", "oauth_response", "browser"]) {
      const { server, port } = await startServer(boundary === "oauth_response" ? "invalid_oauth_start" : "first_run");
      const page = await browser.newPage();
      try {
        await openAssistant(page, `http://127.0.0.1:${port}`);
        await page.evaluate(async boundary => {
          localStorage.setItem(AGENT_PRIVACY_CONSENT_KEY, JSON.stringify({
            version: AGENT_PRIVACY_CONSENT_VERSION,
            providers: { claude: { accepted: true }, codex: { accepted: true } },
          }));
          beginNetworkGuard = async () => {
            if (boundary === "guard") throw new Error("network_guard_unavailable");
            return null;
          };
          runConnectivityPreflight = async () => {
            if (boundary === "preflight") throw Object.assign(new Error("connect_failed"), {
              connectivity: { code: "connect_failed", retryable: true, settings_hint: "none" },
            });
          };
          openExternalAuth = async () => { throw new Error("external_launch_failed"); };
          await renderAssistantView(document.getElementById("view"));
        }, boundary);
        await page.locator("#asst-connect-claude").click();
        await page.waitForFunction(() => AssistantState.oauthOpening === null
          && document.getElementById("asst-connect-claude") && !document.getElementById("asst-connect-claude").disabled);
        record(`${boundary} failure restores both connect controls`,
          await page.evaluate(() => ["claude", "codex"].every(p => !document.getElementById(`asst-connect-${p}`).disabled)
            && !document.querySelector("[data-agent-oauth-opening]")
            && OAUTH_ATTEMPTS.size === 0));
      } finally {
        await page.close();
        await new Promise(resolve => server.close(resolve));
      }
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
