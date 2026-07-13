// Deterministic Assistant UI smoke for #622.
//
// Starts a local fixture server, serves the real app assets, drives the Assistant
// tab through Playwright, and writes screenshots plus a JSON evidence report.
// No daemon, provider account, or external network is required.
import { chromium } from "playwright";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const REPO = path.resolve(path.dirname(__filename), "..");
const OUT_DIR = path.join(REPO, "docs/evidence/artifacts/issue-622");
const AGENT_CAP = "fixture-agent-cap";
const ACCOUNT = "fixture";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const readText = (p) => fs.readFileSync(path.join(REPO, p), "utf8");

function fixtureAppJs() {
  return readText("gui/webui/src/app.js").replace(
    /__([A-Z0-9_]+_CAP_TOKEN)__/g,
    (_m, token) => (token === "AGENT_CAP_TOKEN" ? AGENT_CAP : ""),
  );
}

function json(res, status, body) {
  const data = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": String(data.length),
    "cache-control": "no-store",
  });
  res.end(data);
}

function text(res, status, body, contentType) {
  const data = Buffer.from(body);
  res.writeHead(status, {
    "content-type": contentType,
    "content-length": String(data.length),
    "cache-control": "no-store",
  });
  res.end(data);
}

function checkAgentCap(req) {
  return req.headers["x-capability-token"] === AGENT_CAP;
}

function sendSseMessage(res, obj) {
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

async function sendStream(res, scenario, turn) {
  res.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-cache, no-transform",
    connection: "keep-alive",
  });
  if (scenario === "normal") {
    sendSseMessage(res, { event: "token", text: "Here is " });
    await sleep(80);
    sendSseMessage(res, {
      event: "tool_call",
      name: "archive.search",
      input: { query: "quarterly brief", limit: 3 },
    });
    await sleep(30);
    sendSseMessage(res, {
      event: "tool_result",
      content: JSON.stringify({
        items: [{
          source: {
            service: "mail",
            id: "mail-1",
            path: "/Inbox/quarterly-brief.eml",
            name: "Quarterly brief",
          },
        }],
      }),
      untrusted: true,
    });
    await sleep(30);
    sendSseMessage(res, {
      event: "partial_result",
      items: [{
        service: "mail",
        id: "mail-1",
        remote_id: "mail-1",
        path: "/Inbox/quarterly-brief.eml",
        name: "Quarterly brief",
        item_type: "message",
        snippet: "Source-backed fixture result.",
      }],
    });
    await sleep(80);
    sendSseMessage(res, { event: "token", text: "a source-backed answer." });
    await sleep(30);
    sendSseMessage(res, { event: "done", reason: "complete" });
  } else if (scenario === "error") {
    sendSseMessage(res, { event: "error", message: "Fixture stream failure" });
    await sleep(30);
    sendSseMessage(res, { event: "done", reason: "error" });
  } else {
    sendSseMessage(res, { event: "token", text: "This action needs review." });
    await sleep(60);
    sendSseMessage(res, {
      event: "confirmation_required",
      pending_id: `pending-${turn}`,
      token: `fixture-token-${turn}`,
      action_hash: `fixture-hash-${turn}`,
      preview: "Delete archived fixture item",
      risk: "destructive",
      expires_at_ms: Date.now() + 600000,
    });
    await sleep(30);
    sendSseMessage(res, { event: "done", reason: "pending_confirmation" });
  }
  await sleep(50);
  res.end();
}

function makeFixtureServer(evidence) {
  const indexHtml = readText("gui/webui/src/index.html");
  const appCss = readText("gui/webui/src/app.css");
  const appJs = fixtureAppJs();
  const appFont = fs.readFileSync(path.join(REPO, "gui/webui/src/assets/inter-var.woff2"));
  let agentConnected = false;
  let agentProvider = "claude";
  let agentModel = "claude-sonnet-4";
  let turnSeq = 0;
  const turns = new Map();
  const state = {
    oauthStarts: [],
    modelPosts: [],
    confirmPosts: [],
    cancelPosts: [],
    viewHits: [],
    streamScenarios: [],
  };

  // #639 T10: the host onboarding projection the wizard renders (per-provider readiness + steps).
  const onboardingStepKeys = [
    "official_oauth_completed", "credential_encrypted", "retained_envelope_verified",
    "default_harness_removed", "m365_profile_activated", "isyncyou_tool_connected",
    "subscription_identity_set", "ready",
  ];
  const onboardingNode = (ready) => ({
    state: ready ? "ready" : "not_started",
    steps: onboardingStepKeys.map((key) => ({ key, complete: ready })),
  });
  const statusBody = () => ({
    enabled: true,
    connected: agentConnected,
    provider: agentProvider,
    model: agentModel,
    claude: agentConnected,
    codex: false,
    onboarding: {
      selected_provider: agentProvider || "claude",
      selected_state: agentConnected ? "ready" : "not_started",
      providers: {
        claude: onboardingNode(agentConnected),
        codex: onboardingNode(false),
      },
    },
    models: {
      claude: [
        { id: "claude-sonnet-4", label: "Claude Sonnet 4" },
        { id: "claude-opus-4", label: "Claude Opus 4" },
      ],
      codex: [
        { id: "gpt-5-codex", label: "GPT-5 Codex" },
      ],
    },
  });

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    try {
      if (req.method === "GET" && url.pathname === "/") {
        text(res, 200, indexHtml, "text/html; charset=utf-8");
      } else if (req.method === "GET" && url.pathname === "/app.css") {
        text(res, 200, appCss, "text/css; charset=utf-8");
      } else if (req.method === "GET" && url.pathname === "/app.js") {
        text(res, 200, appJs, "application/javascript; charset=utf-8");
      } else if (req.method === "GET" && url.pathname === "/app.woff2") {
        res.writeHead(200, {
          "content-type": "font/woff2",
          "content-length": String(appFont.length),
          "cache-control": "no-store",
        });
        res.end(appFont);
      } else if (req.method === "GET" && url.pathname === "/favicon.ico") {
        res.writeHead(204);
        res.end();
      } else if (req.method === "GET" && url.pathname === "/api/v1/accounts") {
        json(res, 200, { accounts: [{ id: ACCOUNT, username: "fixture@example.test" }] });
      } else if (req.method === "GET" && url.pathname === "/api/v1/status") {
        json(res, 200, {
          services: [
            { service: "mail", items: 1 },
            { service: "onedrive", items: 0 },
            { service: "calendar", items: 0 },
            { service: "contacts", items: 0 },
            { service: "todo", items: 0 },
            { service: "onenote", items: 0 },
          ],
          totals: { items: 1, archived: 1 },
        });
      } else if (req.method === "GET" && url.pathname === "/api/v1/activity") {
        json(res, 200, { runs: [] });
      } else if (req.method === "GET" && url.pathname === "/api/v1/settings") {
        json(res, 200, { accounts: [{ id: ACCOUNT }] });
      } else if (req.method === "GET" && url.pathname === "/api/v1/sync/state") {
        json(res, 200, { enabled: false, paused: false });
      } else if (req.method === "GET" && url.pathname === "/api/v1/events") {
        res.writeHead(200, {
          "content-type": "text/event-stream; charset=utf-8",
          "cache-control": "no-cache, no-transform",
          connection: "keep-alive",
        });
        res.write(": fixture-ready\n\n");
      } else if (req.method === "GET" && url.pathname === "/api/v1/agent/status") {
        json(res, 200, statusBody());
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/start") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        state.oauthStarts.push(Object.fromEntries(url.searchParams.entries()));
        agentConnected = true;
        agentProvider = url.searchParams.get("provider") === "codex" ? "codex" : "claude";
        json(res, 200, { authorize_url: `http://${req.headers.host}/fixture-auth-complete` });
      } else if (req.method === "GET" && url.pathname === "/fixture-auth-complete") {
        text(
          res,
          200,
          "<!doctype html><meta charset=\"utf-8\"><title>Fixture auth</title><main>Fixture auth complete</main>",
          "text/html; charset=utf-8",
        );
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/model") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        state.modelPosts.push(Object.fromEntries(url.searchParams.entries()));
        agentProvider = url.searchParams.get("provider") || agentProvider;
        agentModel = url.searchParams.get("model") || agentModel;
        json(res, 200, { ok: true });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/turn") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const prompt = url.searchParams.get("prompt") || "";
        const turn = `turn-${++turnSeq}`;
        const lower = prompt.toLowerCase();
        const scenario = lower.includes("error") ? "error"
          : lower.includes("cancel") ? "pending-cancel"
            : lower.includes("delete") || lower.includes("confirm") ? "pending-confirm"
              : "normal";
        turns.set(turn, scenario);
        json(res, 200, { turn });
      } else if (req.method === "GET" && url.pathname === "/api/v1/agent/stream") {
        const turn = url.searchParams.get("turn") || "";
        const scenario = turns.get(turn) || "normal";
        state.streamScenarios.push({ turn, scenario });
        await sendStream(res, scenario, turn);
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/confirm") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        state.confirmPosts.push(Object.fromEntries(url.searchParams.entries()));
        json(res, 200, { result: "Confirmed by fixture" });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/cancel") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        state.cancelPosts.push(Object.fromEntries(url.searchParams.entries()));
        json(res, 200, { ok: true });
      } else if (req.method === "GET" && url.pathname === "/api/v1/view") {
        state.viewHits.push(Object.fromEntries(url.searchParams.entries()));
        text(
          res,
          200,
          "<!doctype html><meta charset=\"utf-8\"><title>Fixture view</title><main>Quarterly brief fixture body</main>",
          "text/html; charset=utf-8",
        );
      } else {
        evidence.fixture404.push({ method: req.method, path: url.pathname, query: url.search });
        json(res, 404, { error: "fixture route not found", path: url.pathname });
      }
    } catch (err) {
      evidence.fixtureErrors.push(String(err && err.stack ? err.stack : err));
      if (!res.headersSent) json(res, 500, { error: "fixture error" });
      else res.end();
    }
  });
  return { server, state };
}

function assert(evidence, name, ok, details = {}) {
  const row = { name, status: ok ? "pass" : "fail", details };
  evidence.assertions.push(row);
  if (!ok) throw new Error(`${name}: ${JSON.stringify(details)}`);
}

async function screenshot(page, name) {
  const out = path.join(OUT_DIR, name);
  await page.screenshot({ path: out, fullPage: true });
  return path.relative(REPO, out);
}

async function pageText(page) {
  return page.locator("body").innerText({ timeout: 5000 });
}

async function main() {
  fs.mkdirSync(OUT_DIR, { recursive: true });
  const evidence = {
    ok: false,
    generated_at: new Date().toISOString(),
    fixture_origin: null,
    assertions: [],
    console_errors: [],
    page_errors: [],
    browser_requests: [],
    runtime_transports: [],
    external_launches: [],
    non_fixture_origin_requests: [],
    fixture404: [],
    fixtureErrors: [],
    fixture_state: null,
    screenshots: {},
  };

  const fixture = makeFixtureServer(evidence);
  let browser;
  try {
    await new Promise((resolve) => fixture.server.listen(0, "127.0.0.1", resolve));
    const address = fixture.server.address();
    const origin = `http://127.0.0.1:${address.port}`;
    evidence.fixture_origin = origin;

    browser = await chromium.launch();
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    page.on("console", (msg) => {
      if (msg.type() === "error") evidence.console_errors.push(msg.text());
    });
    page.on("pageerror", (err) => evidence.page_errors.push(String(err)));
    page.on("request", (req) => {
      evidence.browser_requests.push({
        method: req.method(),
        url: req.url(),
        resource_type: req.resourceType(),
      });
    });
    await page.addInitScript(() => {
      window.__agentSmokeTransport = [];
      window.__agentSmokeExternalLaunches = [];
      const record = (kind, url) => {
        try {
          window.__agentSmokeTransport.push({ kind, url: new URL(String(url), location.href).href });
        } catch (_) {
          window.__agentSmokeTransport.push({ kind, url: String(url) });
        }
      };
      const nativeFetch = window.fetch;
      window.fetch = (...args) => {
        record("fetch", args[0]);
        return nativeFetch(...args);
      };
      const nativeOpen = XMLHttpRequest.prototype.open;
      XMLHttpRequest.prototype.open = function(method, url, ...rest) {
        record("XMLHttpRequest", url);
        return nativeOpen.call(this, method, url, ...rest);
      };
      const NativeEventSource = window.EventSource;
      if (NativeEventSource) {
        function RecordingEventSource(url, config) {
          record("EventSource", url);
          return new NativeEventSource(url, config);
        }
        RecordingEventSource.prototype = NativeEventSource.prototype;
        Object.setPrototypeOf(RecordingEventSource, NativeEventSource);
        window.EventSource = RecordingEventSource;
      }
      const NativeWebSocket = window.WebSocket;
      if (NativeWebSocket) {
        function RecordingWebSocket(url, protocols) {
          record("WebSocket", url);
          return protocols === undefined ? new NativeWebSocket(url) : new NativeWebSocket(url, protocols);
        }
        RecordingWebSocket.prototype = NativeWebSocket.prototype;
        Object.setPrototypeOf(RecordingWebSocket, NativeWebSocket);
        window.WebSocket = RecordingWebSocket;
      }
    });

    await page.goto(`${origin}/`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('.nav-item[data-service="assistant"]', { timeout: 10000 });
    const assistantNavVisible = await page.locator('.nav-item[data-service="assistant"]').first().isVisible();
    assert(evidence, "desktop Assistant nav visible", assistantNavVisible);
    await page.locator('.nav-item[data-service="assistant"]').first().click();
    await page.waitForSelector('[data-testid="agent-setup"]', { timeout: 10000 });
    assert(evidence, "setup consent panel visible", await page.locator('[data-testid="agent-consent"]').isVisible());
    assert(evidence, "connect disabled before consent", await page.locator('[data-testid="agent-connect-codex"]').isDisabled());
    assert(evidence, "BYO key row absent", await page.locator('[data-agent-byo-key="unavailable"]').count() === 0);
    assert(evidence, "no editable secret input in setup", await page.locator('input[type="password"], input[name*="key" i], textarea[name*="key" i]').count() === 0);
    // #639 T10: the first-run handoff wizard renders the ordered official-sign-in -> ready steps.
    const wizardStepCount = await page.locator('[data-testid="agent-wizard-steps"] [data-agent-wizard-step]').count();
    assert(evidence, "handoff wizard renders 8 ordered steps", wizardStepCount === 8, { wizardStepCount });
    const wizardStepOrder = await page.evaluate(() =>
      Array.from(document.querySelectorAll('[data-testid="agent-wizard-steps"] [data-agent-wizard-step]'))
        .map((node) => node.getAttribute("data-agent-wizard-step")));
    assert(evidence, "wizard steps are the ordered handoff sequence",
      wizardStepOrder.join(",") === "official_oauth_completed,credential_encrypted,retained_envelope_verified,default_harness_removed,m365_profile_activated,isyncyou_tool_connected,subscription_identity_set,ready",
      wizardStepOrder);
    evidence.screenshots.setup_consent = await screenshot(page, "setup-consent.png");
    evidence.screenshots.desktop_assistant = await screenshot(page, "desktop-assistant.png");

    await page.locator('[data-agent-consent-accept="claude"]').click();
    const consent = await page.evaluate(() => JSON.parse(localStorage.getItem("isy_agent_privacy_consent_v1") || "{}"));
    assert(evidence, "consent localStorage is versioned", consent.version === 1 && consent.accepted === true && consent.provider === "claude", consent);
    assert(evidence, "consent localStorage contains no secrets", !JSON.stringify(consent).match(/token|secret|key|code|content|refresh|access/i), consent);

    const authPopupPromise = page.waitForEvent("popup", { timeout: 2000 }).catch(() => null);
    await page.locator('#asst-connect-claude').click();
    const authPopup = await authPopupPromise;
    if (authPopup) {
      await authPopup.waitForLoadState("domcontentloaded");
      evidence.external_launches.push({ url: authPopup.url(), target: "popup" });
      await authPopup.close();
    }
    await page.waitForTimeout(500);
    if (page.url().startsWith(`${origin}/fixture-auth-complete`)) {
      evidence.external_launches.push({ url: page.url(), target: "same-tab" });
      await page.goto(`${origin}/#/assistant`, { waitUntil: "domcontentloaded" });
    }
    await page.waitForSelector('[data-testid="agent-transcript"]', { timeout: 10000 });
    assert(evidence, "oauth start posted once", fixture.state.oauthStarts.length === 1, fixture.state.oauthStarts);
    assert(evidence, "oauth start used same-origin fixture route", evidence.browser_requests.some((r) => r.method === "POST" && r.url.startsWith(`${origin}/api/v1/agent/oauth/start`)));
    assert(evidence, "auth launch stayed on fixture origin", evidence.external_launches.every((row) => row.url.startsWith(origin)), evidence.external_launches);

    await page.locator('[data-testid="agent-input"]').fill("Find the quarterly brief");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForFunction(() => document.body.innerText.includes("Here is "), null, { timeout: 10000 });
    const firstTokenText = await pageText(page);
    assert(evidence, "first token appears incrementally", firstTokenText.includes("Here is ") && !firstTokenText.includes("a source-backed answer."));
    await page.waitForFunction(() => document.body.innerText.includes("a source-backed answer."), null, { timeout: 10000 });
    await page.waitForSelector('[data-agent-citation="view"]', { timeout: 10000 });
    const citationHref = await page.locator('[data-agent-citation="view"]').first().getAttribute("href");
    assert(evidence, "citation href same-origin path", citationHref && citationHref.startsWith("/api/v1/view?"), { citationHref });
    const popupPromise = page.waitForEvent("popup");
    await page.locator('[data-agent-citation="view"]').first().click();
    const popup = await popupPromise;
    await popup.waitForLoadState("domcontentloaded");
    assert(evidence, "citation click opens fixture view", popup.url().startsWith(`${origin}/api/v1/view?`), { popup: popup.url() });
    await popup.close();
    evidence.screenshots.stream_citations = await screenshot(page, "stream-citations.png");

    await page.locator('[data-testid="agent-model-picker"] .mdl-trigger').click();
    await page.locator('[data-agent-model-option="claude|claude-opus-4"]').click();
    await page.waitForFunction(() => document.querySelector('[data-testid="agent-model-picker"]')?.innerText.includes("Claude Opus 4"), null, { timeout: 10000 });
    assert(evidence, "model picker posts model change", fixture.state.modelPosts.length === 1 && fixture.state.modelPosts[0].model === "claude-opus-4", fixture.state.modelPosts);
    assert(evidence, "usage chip shows unavailable state", (await page.locator('[data-testid="agent-usage"]').innerText()).includes("Usage unavailable"));

    await page.locator('[data-testid="agent-input"]').fill("Please delete the stale fixture item");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForSelector('[data-agent-pending-card="pending-turn-2"]', { timeout: 10000 });
    assert(evidence, "pending card appears", await page.locator('[data-agent-pending-card="pending-turn-2"]').isVisible());
    assert(evidence, "no confirm before click", fixture.state.confirmPosts.length === 0);
    const pendingHtml = await page.locator('[data-agent-pending-card="pending-turn-2"]').evaluate((el) => el.outerHTML);
    assert(evidence, "pending DOM excludes token and hash", !pendingHtml.includes("fixture-token-") && !pendingHtml.includes("fixture-hash-"));
    evidence.screenshots.pending_confirm = await screenshot(page, "pending-confirm.png");
    await page.locator('[data-agent-pending-card="pending-turn-2"]').getByRole("button", { name: "Confirm" }).click();
    await page.waitForFunction(() => document.body.innerText.includes("Confirmed by fixture"), null, { timeout: 10000 });
    assert(evidence, "confirm posts once with pending token action_hash", fixture.state.confirmPosts.length === 1
      && fixture.state.confirmPosts[0].pending
      && fixture.state.confirmPosts[0].token?.startsWith("fixture-token-")
      && fixture.state.confirmPosts[0].action_hash?.startsWith("fixture-hash-"), fixture.state.confirmPosts);

    await page.locator('[data-testid="agent-input"]').fill("Please cancel the delete fixture item");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForSelector('[data-agent-pending-card="pending-turn-3"]', { timeout: 10000 });
    await page.locator('[data-agent-pending-card="pending-turn-3"]').getByRole("button", { name: "Cancel" }).click();
    await page.waitForFunction(() => document.querySelector('[data-agent-pending-card="pending-turn-3"]')?.classList.contains("cancelled"), null, { timeout: 10000 });
    assert(evidence, "cancel posts once for separate turn", fixture.state.cancelPosts.length === 1 && fixture.state.cancelPosts[0].turn, fixture.state.cancelPosts);
    evidence.screenshots.pending_cancel = await screenshot(page, "pending-cancel.png");

    await page.locator('[data-testid="agent-input"]').fill("trigger error");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForFunction(() => document.body.innerText.includes("Fixture stream failure"), null, { timeout: 10000 });
    assert(evidence, "error stream renders inline", (await pageText(page)).includes("Fixture stream failure"));

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`${origin}/?mobile-smoke=1#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="agent-transcript"]', { timeout: 10000 });
    await page.waitForTimeout(300);
    const mobileNavVisible = await page.locator('.nav-item[data-service="assistant"]').first().isVisible();
    const noOverflow = await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);
    assert(evidence, "mobile Assistant nav visible", mobileNavVisible);
    assert(evidence, "mobile Assistant has no horizontal overflow", noOverflow, await page.evaluate(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      clientWidth: document.documentElement.clientWidth,
    })));
    evidence.screenshots.mobile_assistant = await screenshot(page, "mobile-assistant.png");

    evidence.runtime_transports = await page.evaluate(() => window.__agentSmokeTransport || []);
    const seenOrigins = new Set();
    for (const req of evidence.browser_requests) {
      try {
        const u = new URL(req.url);
        seenOrigins.add(u.origin);
        if (u.origin !== origin) evidence.non_fixture_origin_requests.push(req);
      } catch (_) {}
    }
    for (const req of evidence.runtime_transports) {
      try {
        const u = new URL(req.url);
        seenOrigins.add(u.origin);
        if (u.origin !== origin) evidence.non_fixture_origin_requests.push({ method: req.kind, url: req.url, resource_type: "runtime" });
      } catch (_) {}
    }
    assert(evidence, "no console errors", evidence.console_errors.length === 0, evidence.console_errors);
    assert(evidence, "no page errors", evidence.page_errors.length === 0, evidence.page_errors);
    assert(evidence, "no non-fixture-origin WebView requests", evidence.non_fixture_origin_requests.length === 0, evidence.non_fixture_origin_requests);
    assert(evidence, "fixture routes complete", evidence.fixture404.length === 0 && evidence.fixtureErrors.length === 0, { fixture404: evidence.fixture404, fixtureErrors: evidence.fixtureErrors });
    assert(evidence, "normal pending error streams exercised", ["normal", "pending-confirm", "pending-cancel", "error"].every((s) => fixture.state.streamScenarios.some((row) => row.scenario === s)), fixture.state.streamScenarios);

    evidence.fixture_state = fixture.state;
    evidence.ok = true;
    console.log(`agent-ui-smoke: ${evidence.assertions.length} assertions passed`);
  } catch (err) {
    evidence.ok = false;
    evidence.error = String(err && err.stack ? err.stack : err);
    console.error(evidence.error);
  } finally {
    if (browser) await browser.close().catch(() => {});
    await new Promise((resolve) => fixture.server.close(resolve));
    evidence.fixture_state = evidence.fixture_state || fixture.state;
    fs.writeFileSync(path.join(OUT_DIR, "agent-ui-smoke.json"), JSON.stringify(evidence, null, 2) + "\n");
  }
  process.exit(evidence.ok ? 0 : 1);
}

await main();
