import { chromium } from "playwright";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const AGENT_CAP = "fixture-agent-cap";
const PROVIDERS = ["claude", "codex"];

function asset(relative) {
  return fs.readFileSync(path.join(ROOT, relative));
}

function onboarding(state) {
  return { state, steps: [] };
}

function lifecycle(overrides = {}) {
  return {
    state: "connected",
    mode: null,
    busy: false,
    retryable: false,
    code: "ok",
    switch_capability: "unavailable",
    revoke_scope_guarantee: "observed_token_session",
    ack_required: false,
    credential_etag: "credential-etag-fixture",
    resume_operation_id: null,
    operation_etag: null,
    ...overrides,
  };
}

function statusFor(scenario) {
  const claude = lifecycle();
  const codex = lifecycle({ switch_capability: "verified_subject", credential_etag: "codex-etag-fixture" });
  if (scenario === "revoke_unknown") Object.assign(claude, {
    state: "revoke_unknown", mode: "disconnect", busy: true, retryable: true,
    code: "retry_revoke", credential_etag: null,
    resume_operation_id: "operation-revoke", operation_etag: "operation-etag-revoke",
  });
  if (scenario === "cleanup_pending") Object.assign(claude, {
    state: "cleanup_pending", mode: "disconnect", busy: true, retryable: true,
    code: "resume_cleanup", credential_etag: null,
    resume_operation_id: "operation-cleanup", operation_etag: "operation-etag-cleanup",
  });
  if (scenario === "exchange_unknown") Object.assign(codex, {
    state: "exchange_outcome_unknown", mode: "switch", busy: true, retryable: true,
    code: "retry_exchange", credential_etag: null,
    resume_operation_id: "operation-exchange", operation_etag: "operation-etag-exchange",
  });
  if (scenario === "reconnect") Object.assign(claude, { state: "reconnect_required" });
  if (scenario === "busy") Object.assign(claude, { busy: true });
  if (scenario === "full_grant") Object.assign(claude, {
    revoke_scope_guarantee: "full_grant", ack_required: true,
  });
  return {
    enabled: true,
    connected: scenario !== "reconnect",
    provider: "claude",
    selected_provider: "claude",
    model: "claude-sonnet-4",
    claude: scenario !== "reconnect",
    codex: true,
    credential_state: { claude: scenario === "reconnect" ? "reconnect_required" : "connected", codex: "connected" },
    account_lifecycle: { claude, codex },
    onboarding: {
      selected_provider: "claude",
      providers: {
        claude: onboarding(scenario === "reconnect" ? "reconnect_required" : "ready"),
        codex: onboarding("ready"),
      },
    },
    models: {
      claude: [{ id: "claude-sonnet-4", label: "Claude" }],
      codex: [{ id: "gpt-5-codex", label: "Codex" }],
    },
  };
}

function serve(scenario) {
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
    if (table[pathname]) {
      res.writeHead(200, { "content-type": table[pathname][0], "cache-control": "no-store" });
      return res.end(table[pathname][1]);
    }
    let value = {};
    if (pathname === "/api/v1/accounts") value = { accounts: [{ id: "fixture", username: "fixture" }] };
    else if (pathname === "/api/v1/status") value = { services: [], totals: { items: 0, archived: 0 } };
    else if (pathname === "/api/v1/activity") value = { runs: [] };
    else if (pathname === "/api/v1/settings") value = { accounts: [{ id: "fixture" }] };
    else if (pathname === "/api/v1/sync/state") value = { enabled: false, paused: false };
    else if (pathname === "/api/v1/agent/status") value = statusFor(scenario);
    else {
      res.writeHead(404, { "content-type": "application/json" });
      return res.end('{"error":"not found"}');
    }
    res.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
    res.end(JSON.stringify(value));
  });
}

function check(report, name, passed, detail = null) {
  report.assertions.push({ name, passed, detail });
  if (!passed) throw new Error(`assertion failed: ${name}`);
}

async function withScenario(browser, scenario, callback) {
  const server = serve(scenario);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
  try {
    await page.addInitScript(() => {
      localStorage.setItem("isy_agent_privacy_consent_v1", JSON.stringify({
        version: 2,
        providers: {
          claude: { accepted: true, timestamp: "fixture" },
          codex: { accepted: true, timestamp: "fixture" },
        },
      }));
    });
    await page.goto(`http://127.0.0.1:${server.address().port}/#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-agent-lifecycle-controls="1"]');
    await callback(page);
  } finally {
    await page.close();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function main() {
  const output = process.argv[2] || "/tmp/isyncyou-645-ui-smoke.json";
  const report = { schema_version: 1, issue: 645, ok: false, assertions: [] };
  let browser;
  try {
    browser = await chromium.launch();
    await withScenario(browser, "connected", async (page) => {
      check(report, "connected Claude exposes Disconnect", await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="disconnect"]').isVisible());
      check(report, "connected Claude exposes Reconnect", await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="reconnect"]').isVisible());
      check(report, "Claude without verified subject has no Switch command", await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="switch"]').count() === 0);
      check(report, "validated Codex exposes Switch", await page.locator('[data-agent-lifecycle-provider="codex"][data-agent-lifecycle-action="switch"]').isVisible());

      await page.evaluate(() => {
        window.__lifecycleOrder = [];
        beginNetworkGuard = async (reason) => { window.__lifecycleOrder.push(`guard:${reason}`); return "revoke-guard"; };
        runConnectivityPreflight = async (_provider, purpose) => { window.__lifecycleOrder.push(`preflight:${purpose}`); return { status: "ready" }; };
        postJson = async (route, _cap, body) => { window.__lifecycleOrder.push(`post:${route}:${body.mode}`); return { state: "completed", mode: body.mode, code: "ok" }; };
        endNetworkGuard = async (guardId) => { window.__lifecycleOrder.push(`end:${guardId}`); };
        renderAssistantView = async () => {};
      });
      await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="disconnect"]').click();
      check(report, "confirmation appears before lifecycle route", await page.locator('[data-agent-lifecycle-dialog="disconnect"]').isVisible());
      await page.locator('[data-agent-lifecycle-confirm="disconnect"]').click();
      await page.waitForFunction(() => window.__lifecycleOrder.length === 4);
      const order = await page.evaluate(() => window.__lifecycleOrder);
      check(report, "revoke guard and preflight precede logout POST", order.join("|") === "guard:credential_revoke|preflight:credential_revoke|post:/api/v1/agent/oauth/logout:disconnect|end:revoke-guard", order);
    });

    await withScenario(browser, "revoke_unknown", async (page) => {
      check(report, "revoke unknown exposes only Retry revoke", await page.locator('[data-agent-lifecycle-resume="retry_revoke"]').isVisible());
      check(report, "revoke unknown does not expose fresh Disconnect", await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="disconnect"]').count() === 0);
    });
    await withScenario(browser, "cleanup_pending", async (page) => {
      check(report, "cleanup pending exposes Resume cleanup", await page.locator('[data-agent-lifecycle-resume="resume_cleanup"]').isVisible());
    });
    await withScenario(browser, "exchange_unknown", async (page) => {
      check(report, "unknown exchange exposes only Retry exchange", await page.locator('[data-agent-lifecycle-resume="retry_exchange"]').isVisible());
      check(report, "unknown exchange does not start another OAuth command", await page.locator('[data-agent-lifecycle-continue-oauth]').count() === 0);
    });
    await withScenario(browser, "reconnect", async (page) => {
      const text = await page.locator('#asst-connect-claude').innerText();
      check(report, "reconnect-required wizard renders Reconnect", /reconnect/i.test(text), text);
    });
    await withScenario(browser, "busy", async (page) => {
      const disconnect = page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="disconnect"]');
      const reconnect = page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="reconnect"]');
      check(report, "busy lifecycle keeps Disconnect visible but disabled", await disconnect.isVisible() && await disconnect.isDisabled());
      check(report, "busy lifecycle keeps Reconnect visible but disabled", await reconnect.isVisible() && await reconnect.isDisabled());
    });
    await withScenario(browser, "full_grant", async (page) => {
      await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="disconnect"]').click();
      const confirm = page.locator('[data-agent-lifecycle-confirm="disconnect"]');
      const ack = page.locator('[data-agent-lifecycle-full-grant-ack="1"]');
      check(report, "full-grant confirmation starts disabled", await confirm.isDisabled());
      await ack.check();
      check(report, "full-grant acknowledgement enables mutation", await confirm.isEnabled());
      check(report, "full-grant warning names other-client impact", /other clients/i.test(await page.locator('[data-agent-lifecycle-dialog="disconnect"]').innerText()));
    });
    await withScenario(browser, "connected", async (page) => {
      await page.evaluate(() => {
        window.__boundReconnect = [];
        beginNetworkGuard = async () => "revoke-guard";
        runConnectivityPreflight = async () => ({ status: "ready" });
        endNetworkGuard = async () => {};
        postJson = async (_route, _cap, body) => ({
          state: "awaiting_oauth_login",
          mode: body.mode,
          operation_id: "server-operation-fixture",
        });
        startAiLogin = async (provider, operationId) => { window.__boundReconnect.push({ provider, operationId }); };
      });
      await page.locator('[data-agent-lifecycle-provider="claude"][data-agent-lifecycle-action="reconnect"]').click();
      await page.locator('[data-agent-lifecycle-confirm="reconnect"]').click();
      await page.waitForFunction(() => window.__boundReconnect.length === 1);
      const calls = await page.evaluate(() => window.__boundReconnect);
      check(report, "Reconnect starts OAuth only with the server lifecycle operation", calls[0].provider === "claude" && calls[0].operationId === "server-operation-fixture", calls);
    });

    const source = asset("gui/webui/src/app.js").toString("utf8");
    const cleanup = source.indexOf('node.state !== "candidate_cleanup"');
    const oauthEnd = source.indexOf("await finishOAuthGuard(provider);", cleanup);
    const revokeResume = source.indexOf('return await resumeAccountLifecycle(provider, node, "retry_revoke")', cleanup);
    check(report, "candidate cleanup ends OAuth guard before fresh revoke resume", cleanup >= 0 && oauthEnd > cleanup && revokeResume > oauthEnd);
    check(report, "candidate cleanup keeps polling after a transient callback lease race", source.includes("if (!result) {") && source.includes("return false;") && revokeResume > oauthEnd);
    check(report, "lifecycle UI contains no token-delete affordance", !/delete\s+token|access_token|refresh_token/i.test(source.slice(source.indexOf("function renderAssistantLifecycleControls"))));
    check(report, "both providers use the same candidate cleanup controller", PROVIDERS.every((provider) => source.includes(`handleCandidateCleanupStatus(s, "${provider}")`) || provider === "claude" && source.includes("handleCandidateCleanupStatus(s, provider)")));
    report.ok = true;
  } catch (error) {
    report.error = String(error && error.stack ? error.stack : error);
  } finally {
    if (browser) await browser.close().catch(() => {});
    fs.writeFileSync(output, JSON.stringify(report, null, 2) + "\n");
  }
  if (report.ok) console.log(`agent-account-lifecycle-ui-smoke: ${report.assertions.length} assertions passed`);
  else console.error(report.error);
  process.exit(report.ok ? 0 : 1);
}

await main();
