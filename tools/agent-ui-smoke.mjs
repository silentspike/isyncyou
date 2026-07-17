// Deterministic Assistant UI smoke for #622/#639.
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
const outFlag = process.argv.indexOf("--out");
if (outFlag >= 0 && !process.argv[outFlag + 1]) {
  throw new Error("--out requires a directory");
}
const OUT_DIR = outFlag >= 0
  ? path.resolve(REPO, process.argv[outFlag + 1])
  : path.join(REPO, "docs/evidence/artifacts/issue-622");
const AGENT_CAP = "fixture-agent-cap";
const ACCOUNT_CAP = "fixture-account-cap";
const ACCOUNT = "fixture";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const readText = (p) => fs.readFileSync(path.join(REPO, p), "utf8");

function fixtureAppJs() {
  return readText("gui/webui/src/app.js").replace(
    /__([A-Z0-9_]+_CAP_TOKEN)__/g,
    (_m, token) => token === "AGENT_CAP_TOKEN" ? AGENT_CAP
      : token === "ACCOUNT_CAP_TOKEN" ? ACCOUNT_CAP : "",
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

function checkAccountCap(req) {
  return req.headers["x-capability-token"] === ACCOUNT_CAP;
}

async function readJson(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const body = Buffer.concat(chunks);
  if (body.length > 64 * 1024) throw new Error("fixture body too large");
  return JSON.parse(body.toString("utf8"));
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
    sendSseMessage(res, { event: "error", message: "raw-fixture-provider-error" });
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
      preview: "Delete this archived item",
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
  let agentReasoningEffort = "medium";
  let onboardingMode = "not_started";
  let oauthAttemptSeq = 0;
  let turnSeq = 0;
  let selectedSessionId = null;
  let failNextSessionCreate = false;
  const sessions = new Map();
  const turns = new Map();
  const activeTurnStreams = new Map();
  const requestScenarios = new Map();
  const state = {
    accountLoginStarts: [],
    accountLoginCancels: [],
    accountLoginPollState: "pending",
    oauthStarts: [],
    modelPosts: [],
    confirmPosts: [],
    cancelPosts: [],
    turnCancelPosts: [],
    viewHits: [],
    streamScenarios: [],
    requestStatusReads: [],
  };

  // #639 T10: the host onboarding projection the wizard renders (per-provider readiness + steps).
  const onboardingStepKeys = [
    "official_oauth_completed", "credential_encrypted", "retained_envelope_verified",
    "default_harness_removed", "m365_profile_activated", "isyncyou_tool_connected",
    "subscription_identity_set", "ready",
  ];
  const onboardingNode = (provider) => ({
    state: agentConnected && agentProvider === provider ? "ready"
      : provider === agentProvider ? onboardingMode : "not_started",
    steps: onboardingStepKeys.map((key) => ({
      key,
      complete: agentConnected && agentProvider === provider,
    })),
  });
  const statusBody = () => ({
    enabled: true,
    connected: agentConnected,
    provider: agentProvider,
    model: agentModel,
    reasoning_effort: agentProvider === "codex" ? agentReasoningEffort : "",
    reasoning_efforts: [
      { id: "low", label: "Light" },
      { id: "medium", label: "Medium" },
      { id: "high", label: "High" },
      { id: "xhigh", label: "Extra High" },
    ],
    claude: agentConnected && agentProvider === "claude",
    codex: agentConnected && agentProvider === "codex",
    onboarding: {
      selected_provider: agentProvider || "claude",
      selected_state: agentConnected ? "ready" : onboardingMode,
      providers: {
        claude: onboardingNode("claude"),
        codex: onboardingNode("codex"),
      },
    },
    models: {
      claude: [
        { id: "claude-sonnet-4", label: "Claude Sonnet 4" },
        { id: "claude-opus-4", label: "Claude Opus 4" },
      ],
      codex: [
        { id: "gpt-5.6-sol", label: "GPT-5.6 Sol" },
        { id: "gpt-5.6-terra", label: "GPT-5.6 Terra" },
        { id: "gpt-5.6-luna", label: "GPT-5.6 Luna" },
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
        json(res, 200, { accounts: [{
          id: ACCOUNT,
          username: "Fixture account",
          auth: {
            reader: { state: "connected", identity_verified: true },
            writer: { state: "connected", identity_verified: true },
          },
        }] });
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
      } else if (req.method === "POST" && url.pathname === "/api/v1/account/login/start") {
        if (!checkAccountCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        const sequence = state.accountLoginStarts.length + 1;
        const loginId = `fixture-login-${sequence}`;
        const authorizationUri = `http://${req.headers.host}/fixture-account-auth?prompt=select_account&state=${"s".repeat(42)}${sequence}&attempt=${sequence}`;
        state.accountLoginStarts.push({ account: body.account, role: body.role, loginId, authorizationUri });
        json(res, 200, {
          flow: "authorization_code_pkce",
          role: body.role,
          login_id: loginId,
          authorization_uri: authorizationUri,
        });
      } else if (req.method === "POST" && url.pathname === "/api/v1/account/login/poll") {
        if (!checkAccountCap(req)) return json(res, 403, { error: "bad capability" });
        await readJson(req);
        const role = state.accountLoginStarts.at(-1)?.role || "reader";
        json(res, 200, state.accountLoginPollState === "error"
          ? { state: "error", code: "account_authorization_failed", role }
          : { state: state.accountLoginPollState, role });
      } else if (req.method === "POST" && url.pathname === "/api/v1/account/login/cancel") {
        if (!checkAccountCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        state.accountLoginCancels.push({
          matched: state.accountLoginStarts.some((attempt) => attempt.loginId === body.id),
        });
        json(res, 200, { cancelled: true });
      } else if (req.method === "GET" && url.pathname === "/fixture-account-auth") {
        text(res, 200, "<!doctype html><title>Account picker fixture</title>", "text/html; charset=utf-8");
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/connectivity/preflight") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        json(res, 200, { status: "ready", code: "ready", retryable: false, settings_hint: "none" });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/start") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        state.oauthStarts.push(body);
        agentConnected = true;
        agentProvider = body.provider === "codex" ? "codex" : "claude";
        onboardingMode = "ready";
        json(res, 200, {
          authorize_url: `http://${req.headers.host}/fixture-auth-complete`,
          attempt_id: `fixture-attempt-${++oauthAttemptSeq}`,
        });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/oauth/cancel") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        json(res, 200, { cancelled: true });
      } else if (req.method === "GET" && url.pathname === "/fixture-auth-complete") {
        text(
          res,
          200,
          "<!doctype html><meta charset=\"utf-8\"><title>Fixture auth</title><main>Fixture auth complete</main>",
          "text/html; charset=utf-8",
        );
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/model") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        state.modelPosts.push(body);
        agentProvider = body.provider || agentProvider;
        agentModel = body.model || agentModel;
        if (agentProvider === "codex") agentReasoningEffort = body.reasoning_effort || "medium";
        json(res, 200, { ok: true });
      } else if (req.method === "GET" && url.pathname === "/api/v1/agent/session/list") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        json(res, 200, {
          sessions: [...sessions.values()].map(({ records: _records, ...session }) => session),
          selected_session_id: selectedSessionId,
          next_cursor: null,
        });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/session/create") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        await readJson(req);
        if (failNextSessionCreate) {
          failNextSessionCreate = false;
          return json(res, 200, { session: null });
        }
        const sessionId = `session-${sessions.size + 1}`;
        const session = { session_id: sessionId, display_name: "Assistant", archived: false };
        sessions.set(sessionId, { ...session, records: [] });
        selectedSessionId = sessionId;
        json(res, 200, { session });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/session/select") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        if (!sessions.has(body.session_id)) return json(res, 404, { error: "session_not_found" });
        selectedSessionId = body.session_id;
        json(res, 200, { selected: true });
      } else if (req.method === "GET" && url.pathname === "/api/v1/agent/session/history") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const session = sessions.get(url.searchParams.get("session_id"));
        if (!session) return json(res, 404, { error: "session_not_found" });
        json(res, 200, { records: session.records, next_cursor: null });
      } else if (req.method === "GET" && url.pathname === "/api/v1/agent/request/status") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const requestId = url.searchParams.get("request_id") || "";
        const scenario = requestScenarios.get(requestId);
        state.requestStatusReads.push({ matched: Boolean(scenario) });
        if (!scenario) return json(res, 404, { error: "request_not_found" });
        json(res, 200, scenario === "error"
          ? { state: "outcome_unknown", code: "turn_outcome_unknown", terminal: true, resume_allowed: false }
          : { state: "committed", code: "ok", terminal: true, resume_allowed: false });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/turn") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        const prompt = body.prompt || "";
        const turn = `turn-${++turnSeq}`;
        const lower = prompt.toLowerCase();
        const scenario = lower.includes("slow cancellation") ? "slow-cancel"
          : lower.includes("error") ? "error"
          : lower.includes("cancel") ? "pending-cancel"
            : lower.includes("delete") || lower.includes("confirm") ? "pending-confirm"
              : "normal";
        turns.set(turn, scenario);
        requestScenarios.set(body.request_id, scenario);
        json(res, 200, { turn });
      } else if (req.method === "GET" && url.pathname === "/api/v1/agent/stream") {
        const turn = url.searchParams.get("turn") || "";
        const scenario = turns.get(turn) || "normal";
        state.streamScenarios.push({ turn, scenario });
        if (scenario === "slow-cancel") {
          res.writeHead(200, {
            "content-type": "text/event-stream; charset=utf-8",
            "cache-control": "no-cache, no-transform",
            connection: "keep-alive",
          });
          res.write(": ready\n\n");
          activeTurnStreams.set(turn, res);
          req.on("close", () => activeTurnStreams.delete(turn));
        } else {
          await sendStream(res, scenario, turn);
        }
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/turn/cancel") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        const body = await readJson(req);
        state.turnCancelPosts.push(body);
        json(res, 200, { ok: true });
        const stream = activeTurnStreams.get(body.turn_id);
        if (stream) {
          sendSseMessage(stream, { event: "done", reason: "cancelled" });
          stream.end();
          activeTurnStreams.delete(body.turn_id);
        }
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/confirm") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        state.confirmPosts.push(await readJson(req));
        json(res, 200, {
          result: '{"status":"ok","op":"live-write","account":"me","service":"mail","verb":"set_read"}',
        });
      } else if (req.method === "POST" && url.pathname === "/api/v1/agent/pending/cancel") {
        if (!checkAgentCap(req)) return json(res, 403, { error: "bad capability" });
        state.cancelPosts.push(await readJson(req));
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
  return {
    server,
    state,
    setOnboardingMode(mode) {
      onboardingMode = mode;
      agentConnected = mode === "ready";
    },
    setAgent(provider, model, reasoningEffort = "medium") {
      agentConnected = true;
      onboardingMode = "ready";
      agentProvider = provider;
      agentModel = model;
      agentReasoningEffort = reasoningEffort;
    },
    failNextSessionCreate() {
      failNextSessionCreate = true;
    },
    seedSelectedSessionHistory() {
      const session = sessions.get(selectedSessionId);
      if (!session) throw new Error("fixture session is unavailable");
      session.records = [
        {
          kind: {
            kind: "turn_intent",
            user_text: "Persisted fixture question",
          },
        },
        {
          kind: {
            kind: "assistant_result",
            text: "Persisted fixture answer",
            sources: [{
              service: "mail",
              id: "mail-1",
              item_id: "mail-1",
              path: "/Inbox/quarterly-brief.eml",
              name: "Quarterly brief",
              item_type: "message",
            }],
            usage: null,
          },
        },
      ];
    },
  };
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

function redactedRequest(rawUrl, extra = {}) {
  try {
    const url = new URL(String(rawUrl));
    return {
      ...extra,
      path: url.pathname,
      query_keys: [...new Set(url.searchParams.keys())].sort(),
    };
  } catch (_) {
    return { ...extra, path: "invalid_url", query_keys: [] };
  }
}

function evidenceForWrite(evidence, state) {
  const closedProviders = state.oauthStarts
    .map((row) => row.provider)
    .filter((provider) => provider === "claude" || provider === "codex");
  const closedModels = state.modelPosts
    .map((row) => row.model)
    .filter((model) => typeof model === "string" && /^[a-z0-9-]{1,64}$/.test(model));
  const closedEfforts = state.modelPosts
    .map((row) => row.reasoning_effort)
    .filter((effort) => ["low", "medium", "high", "xhigh"].includes(effort));

  return {
    evidence_version: 2,
    ok: evidence.ok,
    generated_at: evidence.generated_at,
    fixture_origin: "loopback-fixture",
    assertions: evidence.assertions.map(({ name, status }) => ({ name, status })),
    console_errors: evidence.console_errors.map(() => "redacted"),
    page_errors: evidence.page_errors.map(() => "redacted"),
    browser_requests: evidence.browser_requests.map((request) => redactedRequest(request.url, {
      method: request.method,
      resource_type: request.resource_type,
    })),
    runtime_transports: evidence.runtime_transports.map((request) => redactedRequest(request.url, {
      kind: request.kind,
    })),
    external_launches: evidence.external_launches.map((launch) => redactedRequest(launch.url, {
      target: launch.target,
    })),
    non_fixture_origin_requests: evidence.non_fixture_origin_requests.map((request) => ({
      method: request.method,
      resource_type: request.resource_type,
      destination: "non_fixture_origin",
    })),
    fixture404: evidence.fixture404.map((request) => ({
      method: request.method,
      path: request.path,
      query_present: Boolean(request.query),
    })),
    fixtureErrors: evidence.fixtureErrors.map(() => "redacted"),
    fixture_state: {
      oauth_start_count: state.oauthStarts.length,
      oauth_providers: closedProviders,
      model_post_count: state.modelPosts.length,
      models: closedModels,
      reasoning_efforts: closedEfforts,
      confirm_post_count: state.confirmPosts.length,
      cancel_post_count: state.cancelPosts.length,
      turn_cancel_post_count: state.turnCancelPosts.length,
      account_login_start_count: state.accountLoginStarts.length,
      account_login_cancel_count: state.accountLoginCancels.length,
      view_hit_count: state.viewHits.length,
      request_status_count: state.requestStatusReads.length,
      stream_scenarios: state.streamScenarios.map(({ scenario }) => scenario),
    },
    screenshots: evidence.screenshots,
    ...(evidence.ok ? {} : { error: "smoke_failed" }),
  };
}

function assertRedactedEvidenceReport(report) {
  const forbiddenKeys = new Set([
    "url", "details", "token", "action_hash", "pending", "prompt", "code", "state",
    "account_id", "email", "redirect",
  ]);
  const forbiddenStrings = [
    /https?:\/\//i,
    /(?:^|\D)127\.0\.0\.1(?:\D|$)/,
    /\blocalhost\b/i,
    /[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}/i,
    /fixture-(?:token|hash)-/i,
    /pending-turn-/i,
    /raw-fixture-provider-error/i,
  ];

  const visit = (value) => {
    if (Array.isArray(value)) {
      value.forEach(visit);
      return;
    }
    if (value && typeof value === "object") {
      for (const [key, child] of Object.entries(value)) {
        if (forbiddenKeys.has(key)) throw new Error("smoke evidence contains a forbidden field");
        visit(child);
      }
      return;
    }
    if (typeof value === "string" && forbiddenStrings.some((pattern) => pattern.test(value))) {
      throw new Error("smoke evidence contains a forbidden value");
    }
  };

  visit(report);
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
        const requestUrl = new URL(String(args[0]), location.href);
        if (window.__agentSmokeFailNextStatus && requestUrl.pathname === "/api/v1/agent/status") {
          window.__agentSmokeFailNextStatus = false;
          return Promise.reject(new Error("fixture status unavailable"));
        }
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
    await page.goto(`${origin}/#/settings`, { waitUntil: "domcontentloaded" });
    await page.getByRole("button", { name: "Choose Microsoft account" }).click();
    const readerRole = page.locator(".acct-role").filter({ hasText: "iSyncYou Reader" });
    const writerRole = page.locator(".acct-role").filter({ hasText: "iSyncYou Writer" });
    assert(evidence, "account menu exposes independent Reader and Writer roles",
      await readerRole.getByText("Connected").isVisible()
      && await writerRole.getByText("Connected").isVisible());
    await readerRole.getByTitle("Reconnect iSyncYou Reader").click();
    await page.locator(".acct-menu .acct-dc-title").waitFor();
    const accountMenuText = await page.locator(".acct-menu").innerText();
    assert(evidence, "account reconnect renders picker flow without device code",
      accountMenuText.includes("Connect iSyncYou Reader")
      && accountMenuText.includes("Open Microsoft account picker")
      && !accountMenuText.includes("enter this code"));
    const accountPickerPopupPromise = page.waitForEvent("popup", { timeout: 2000 }).catch(() => null);
    await page.locator(".acct-menu").getByRole("button", { name: "Open Microsoft account picker" }).click();
    const accountPickerPopup = await accountPickerPopupPromise;
    if (accountPickerPopup) {
      await accountPickerPopup.waitForLoadState("domcontentloaded");
      const pickerUrl = new URL(accountPickerPopup.url());
      assert(evidence, "account picker launch uses select_account without verifier",
        pickerUrl.searchParams.get("prompt") === "select_account"
        && pickerUrl.searchParams.has("state")
        && !pickerUrl.searchParams.has("code_verifier"));
      await accountPickerPopup.close();
    } else {
      assert(evidence, "account picker launch uses select_account without verifier", false);
    }
    await page.locator(".acct-menu").getByRole("button", { name: "Cancel" }).click();
    await page.waitForFunction(() => !document.querySelector(".acct-menu .acct-dc"));
    assert(evidence, "account picker cancel closes exact backend attempt",
      fixture.state.accountLoginStarts.length === 1
      && fixture.state.accountLoginStarts[0].role === "reader"
      && fixture.state.accountLoginCancels.length === 1
      && fixture.state.accountLoginCancels[0].matched === true);
    fixture.state.accountLoginPollState = "error";
    await readerRole.getByTitle("Reconnect iSyncYou Reader").click();
    await page.locator(".acct-menu .acct-dc-title").waitFor();
    const stalePicker = page.locator(".acct-menu").getByRole("button", { name: "Open Microsoft account picker" });
    await page.locator(".acct-menu").getByRole("button", { name: "Start sign-in again" }).waitFor({ timeout: 5000 });
    assert(evidence, "terminal account attempt disables stale authorization url",
      await stalePicker.isDisabled());
    const endedAuthorizationUri = fixture.state.accountLoginStarts.at(-1).authorizationUri;
    fixture.state.accountLoginPollState = "pending";
    await page.locator(".acct-menu").getByRole("button", { name: "Start sign-in again" }).click();
    await page.locator(".acct-menu").getByRole("button", { name: "Open Microsoft account picker" }).waitFor();
    assert(evidence, "account sign-in retry creates a fresh backend attempt before browser launch",
      fixture.state.accountLoginStarts.length === 3
      && fixture.state.accountLoginStarts.at(-1).authorizationUri !== endedAuthorizationUri);
    await page.locator(".acct-menu").getByRole("button", { name: "Cancel" }).click();
    await page.waitForFunction(() => !document.querySelector(".acct-menu .acct-dc"));
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

    await page.setViewportSize({ width: 720, height: 900 });
    const wizardNoOverflow = await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);
    assert(evidence, "720px wizard has no horizontal overflow", wizardNoOverflow);
    evidence.screenshots.wizard_720 = await screenshot(page, "wizard-720.png");

    fixture.setOnboardingMode("reconnect_required");
    await page.goto(`${origin}/?reconnect-smoke=1#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-agent-wizard="reconnect_required"]', { timeout: 10000 });
    assert(evidence, "reconnect uses the short host-driven flow",
      (await page.locator('[data-agent-wizard="reconnect_required"]').innerText()).includes("Reconnect")
      && await page.locator('[data-agent-wizard="reconnect_required"] [data-testid="agent-wizard-steps"]').count() === 0);
    evidence.screenshots.reconnect_720 = await screenshot(page, "reconnect-720.png");

    fixture.setOnboardingMode("not_started");
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(`${origin}/?first-run-smoke=1#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="agent-setup"]', { timeout: 10000 });

    await page.evaluate(() => localStorage.setItem("isy_agent_privacy_consent_v1", JSON.stringify({
      version: 1,
      accepted: true,
      provider: "claude",
      timestamp: "2026-01-01T00:00:00.000Z",
    })));
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="agent-setup"]', { timeout: 10000 });
    assert(evidence, "legacy single-provider consent remains accepted after schema upgrade",
      await page.locator('[data-agent-consent-accept="claude"]').getAttribute("data-agent-consent-state") === "accepted"
      && await page.locator('[data-agent-consent-accept="codex"]').getAttribute("data-agent-consent-state") === "required");
    await page.evaluate(() => localStorage.removeItem("isy_agent_privacy_consent_v1"));
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="agent-setup"]', { timeout: 10000 });

    await page.locator('[data-agent-consent-accept="claude"]').click();
    let consent = await page.evaluate(() => JSON.parse(localStorage.getItem("isy_agent_privacy_consent_v1") || "{}"));
    assert(evidence, "consent localStorage is versioned", consent.version === 2 && consent.providers?.claude?.accepted === true, consent);
    assert(evidence, "consent localStorage contains no secrets",
      !JSON.stringify(consent).match(/"(?:access_token|refresh_token|token|secret|key|code|content)"\s*:/i), consent);
    const acceptedConsent = page.locator('[data-agent-consent-accept="claude"]');
    assert(evidence, "accepted consent is visibly confirmed", await acceptedConsent.getAttribute("data-agent-consent-state") === "accepted"
      && await acceptedConsent.getAttribute("aria-pressed") === "true"
      && (await acceptedConsent.innerText()).includes("Claude allowed"));
    assert(evidence, "accepted consent cannot be submitted repeatedly", await acceptedConsent.isDisabled());
    assert(evidence, "connect is enabled only after consent", !(await page.locator('[data-testid="agent-connect-claude"]').isDisabled()));

    await page.locator('[data-agent-consent-accept="codex"]').click();
    consent = await page.evaluate(() => JSON.parse(localStorage.getItem("isy_agent_privacy_consent_v1") || "{}"));
    assert(evidence, "provider consents persist independently", consent.version === 2
      && consent.providers?.claude?.accepted === true
      && consent.providers?.codex?.accepted === true, consent);
    const acceptedCodexConsent = page.locator('[data-agent-consent-accept="codex"]');
    assert(evidence, "both provider consents remain visibly confirmed",
      await acceptedConsent.getAttribute("data-agent-consent-state") === "accepted"
      && await acceptedCodexConsent.getAttribute("data-agent-consent-state") === "accepted"
      && (await acceptedCodexConsent.innerText()).includes("ChatGPT allowed"));
    assert(evidence, "both provider connect buttons stay enabled after consent",
      !(await page.locator('[data-testid="agent-connect-claude"]').isDisabled())
      && !(await page.locator('[data-testid="agent-connect-codex"]').isDisabled()));

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
    const secretSurfaces = await page.evaluate(() => ({
      html: document.documentElement.outerHTML,
      local: Object.fromEntries(Object.keys(localStorage).map((key) => [key, localStorage.getItem(key)])),
      session: Object.fromEntries(Object.keys(sessionStorage).map((key) => [key, sessionStorage.getItem(key)])),
    }));
    const secretSurfaceText = JSON.stringify({
      ...secretSurfaces,
      console_errors: evidence.console_errors,
    });
    assert(evidence, "handoff DOM console and storage exclude capability and attempt secrets",
      !secretSurfaceText.includes(AGENT_CAP) && !secretSurfaceText.includes("fixture-attempt-"));

    await page.evaluate(() => {
      window.__agentSmokeFailNextStatus = true;
      return renderAssistantView(document.querySelector("#view"));
    });
    await page.waitForSelector('[data-agent-status-unavailable="1"]', { timeout: 10000 });
    assert(evidence, "transient status failure preserves the last verified connected surface",
      await page.locator('[data-testid="agent-transcript"]').isVisible()
      && await page.locator('[data-testid="agent-setup"]').count() === 0);
    await page.locator('[data-agent-status-unavailable="1"]').getByRole("button", { name: "Retry" }).click();
    await page.waitForFunction(() => !document.querySelector('[data-agent-status-unavailable="1"]'));

    fixture.failNextSessionCreate();
    await page.locator('[data-testid="agent-input"]').fill("Check shared session storage");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForFunction(() => document.body.innerText.includes("Shared session storage is unavailable."), null, { timeout: 10000 });
    const sessionTransportText = await pageText(page);
    assert(evidence, "session transport failure renders safe actionable copy",
      sessionTransportText.includes("Check your Microsoft 365 connection and try again.")
      && !sessionTransportText.includes("SQLITE")
      && !sessionTransportText.includes("database_open_failed"));

    await page.locator('[data-testid="agent-input"]').fill("Find the quarterly brief");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForFunction(() => document.body.innerText.includes("Here is "), null, { timeout: 10000 });
    const firstTokenText = await pageText(page);
    assert(evidence, "first token appears incrementally", firstTokenText.includes("Here is ") && !firstTokenText.includes("a source-backed answer."));
    await page.waitForFunction(() => document.body.innerText.includes("a source-backed answer."), null, { timeout: 10000 });
    await page.waitForSelector('[data-agent-citation="view"]', { timeout: 10000 });
    const toolCallText = await page.locator('[data-agent-tool-row="tool_call"]').first().innerText();
    assert(evidence, "tool call UI uses product copy and hides internal operation fields",
      toolCallText === "Searching your Microsoft 365 archive"
      && !toolCallText.includes("archive.search")
      && !toolCallText.includes("quarterly brief")
      && !toolCallText.includes("limit")
      && !toolCallText.includes("Tool call"));
    const toolResultTitle = await page.locator('[data-agent-tool-row="tool_result"] .asst-tool-title').first().innerText();
    const toolResultDetail = await page.locator('[data-agent-tool-row="tool_result"] .asst-tool-detail').first().innerText();
    assert(evidence, "tool result UI exposes only source count and never raw result content",
      toolResultTitle === "Sources checked"
      && toolResultDetail === "1 source"
      && !toolResultDetail.includes("items")
      && !toolResultDetail.includes("mail-1"));
    const citationHref = await page.locator('[data-agent-citation="view"]').first().getAttribute("href");
    assert(evidence, "citation href same-origin path", citationHref && citationHref.startsWith("/api/v1/view?"), { citationHref });
    const popupPromise = page.waitForEvent("popup");
    await page.locator('[data-agent-citation="view"]').first().click();
    const popup = await popupPromise;
    await popup.waitForLoadState("domcontentloaded");
    assert(evidence, "citation click opens fixture view", popup.url().startsWith(`${origin}/api/v1/view?`), { popup: popup.url() });
    await popup.close();
    evidence.screenshots.stream_citations = await screenshot(page, "stream-citations.png");

    fixture.seedSelectedSessionHistory();
    await page.goto(`${origin}/?session-hydration-smoke=1#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForFunction(() => document.body.innerText.includes("Persisted fixture answer"), null, { timeout: 10000 });
    const hydratedTranscript = await page.locator('[data-testid="agent-transcript"]').innerText();
    assert(evidence, "V2 tagged session history rehydrates question answer and source",
      hydratedTranscript.includes("Persisted fixture question")
      && hydratedTranscript.includes("Persisted fixture answer")
      && await page.locator('[data-agent-citation="view"]').count() === 1);

    await page.locator('[data-testid="agent-model-picker"] .mdl-trigger').click();
    await page.locator('[data-agent-model-option="claude|claude-opus-4"]').click();
    await page.waitForFunction(() => document.querySelector('[data-testid="agent-model-picker"]')?.innerText.includes("Claude Opus 4"), null, { timeout: 10000 });
    assert(evidence, "model picker posts model change", fixture.state.modelPosts.length === 1 && fixture.state.modelPosts[0].model === "claude-opus-4", fixture.state.modelPosts);
    assert(evidence, "usage chip shows unavailable state", (await page.locator('[data-testid="agent-usage"]').innerText()).includes("Usage unavailable"));

    await page.locator('[data-testid="agent-input"]').fill("Run slow cancellation fixture");
    await page.locator('[data-testid="agent-send"]').click();
    const stopButton = page.locator('[data-testid="agent-stop"]');
    await stopButton.waitFor({ state: "visible", timeout: 10000 });
    const activeTurnControls = {
      stop_enabled: await stopButton.isEnabled(),
      input_disabled: await page.locator('[data-testid="agent-input"]').isDisabled(),
      send_hidden: await page.locator('[data-testid="agent-send"]').isHidden(),
    };
    assert(evidence, "active turn exposes one stop command and locks the composer",
      activeTurnControls.stop_enabled
      && activeTurnControls.input_disabled
      && activeTurnControls.send_hidden,
      activeTurnControls);
    await stopButton.click();
    await page.waitForFunction(() => document.body.innerText.includes("Cancelled"), null, { timeout: 10000 });
    assert(evidence, "turn stop posts once and restores the composer only after terminal cancellation",
      fixture.state.turnCancelPosts.length === 1
      && fixture.state.turnCancelPosts[0].turn_id?.startsWith("turn-")
      && await stopButton.isHidden()
      && await page.locator('[data-testid="agent-input"]').isEnabled());

    fixture.setAgent("codex", "gpt-5.6-sol", "medium");
    await page.goto(`${origin}/?codex-model-smoke=1#/assistant`, { waitUntil: "domcontentloaded" });
    await page.waitForSelector('[data-testid="agent-transcript"]', { timeout: 10000 });
    await page.locator('[data-testid="agent-model-picker"] .mdl-trigger').click();
    assert(evidence, "ChatGPT picker exposes Sol Terra and Luna",
      await page.locator('[data-agent-model-option="codex|gpt-5.6-sol"]').isVisible()
      && await page.locator('[data-agent-model-option="codex|gpt-5.6-terra"]').isVisible()
      && await page.locator('[data-agent-model-option="codex|gpt-5.6-luna"]').isVisible());
    await page.locator('[data-agent-model-option="codex|gpt-5.6-terra"]').click();
    await page.waitForFunction(() => document.querySelector('[data-testid="agent-model-picker"]')?.innerText.includes("GPT-5.6 Terra"), null, { timeout: 10000 });
    await page.locator('[data-testid="agent-model-picker"] .mdl-trigger').click();
    await page.locator('[data-agent-effort-option="high"]').click();
    await page.waitForFunction(() => document.querySelector('[data-testid="agent-model-picker"]')?.innerText.includes("GPT-5.6 Terra · High"), null, { timeout: 10000 });
    const codexSelection = fixture.state.modelPosts.at(-1);
    assert(evidence, "ChatGPT effort posts with the selected GPT-5.6 model",
      codexSelection?.provider === "codex"
      && codexSelection?.model === "gpt-5.6-terra"
      && codexSelection?.reasoning_effort === "high",
      fixture.state.modelPosts);

    await page.locator('[data-testid="agent-input"]').fill("Please delete the stale fixture item");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForSelector('[data-agent-pending-card="1"]', { timeout: 10000 });
    const confirmCard = page.locator('[data-agent-pending-card="1"]').last();
    assert(evidence, "pending card appears", await confirmCard.isVisible());
    assert(evidence, "no confirm before click", fixture.state.confirmPosts.length === 0);
    const pendingHtml = await confirmCard.evaluate((el) => el.outerHTML);
    assert(evidence, "pending DOM excludes token hash and pending handle",
      !pendingHtml.includes("fixture-token-")
      && !pendingHtml.includes("fixture-hash-")
      && !pendingHtml.includes("pending-turn-2"));
    evidence.screenshots.pending_confirm = await screenshot(page, "pending-confirm.png");
    await confirmCard.getByRole("button", { name: "Confirm" }).click();
    await page.waitForFunction(() => document.body.innerText.includes("Completed successfully."), null, { timeout: 10000 });
    assert(evidence, "confirm posts once with pending token action_hash", fixture.state.confirmPosts.length === 1
      && fixture.state.confirmPosts[0].pending
      && fixture.state.confirmPosts[0].token?.startsWith("fixture-token-")
      && fixture.state.confirmPosts[0].action_hash?.startsWith("fixture-hash-"), fixture.state.confirmPosts);
    assert(evidence, "confirmed action is terminal and exposes no stale controls",
      (await confirmCard.getByRole("button").count()) === 0
      && (await confirmCard.innerText()).includes("Action confirmed")
      && (await confirmCard.innerText()).includes("Completed successfully.")
      && !(await confirmCard.innerText()).includes("live-write")
      && !(await confirmCard.innerText()).includes("set_read")
      && !(await confirmCard.innerText()).includes("account")
      && !(await confirmCard.innerText()).includes("Risk:")
      && !(await confirmCard.innerText()).includes("Expires"));

    await page.locator('[data-testid="agent-input"]').fill("Please cancel the delete fixture item");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForFunction(() => document.querySelectorAll('[data-agent-pending-card="1"]').length >= 2, null, { timeout: 10000 });
    const cancelCard = page.locator('[data-agent-pending-card="1"]').last();
    await cancelCard.getByRole("button", { name: "Cancel" }).click();
    await page.waitForFunction(() => Array.from(document.querySelectorAll('[data-agent-pending-card="1"]')).at(-1)?.classList.contains("cancelled"), null, { timeout: 10000 });
    assert(evidence, "pending cancel posts once for separate turn", fixture.state.cancelPosts.length === 1
      && fixture.state.cancelPosts[0].pending?.startsWith("pending-turn-"), fixture.state.cancelPosts);
    assert(evidence, "cancelled action is terminal and exposes no stale controls",
      (await cancelCard.getByRole("button").count()) === 0
      && (await cancelCard.innerText()).includes("Action cancelled")
      && (await cancelCard.innerText()).includes("No changes were made.")
      && !(await cancelCard.innerText()).includes("Risk:")
      && !(await cancelCard.innerText()).includes("Expires"));
    evidence.screenshots.pending_cancel = await screenshot(page, "pending-cancel.png");

    await page.locator('[data-testid="agent-input"]').fill("trigger error");
    await page.locator('[data-testid="agent-send"]').click();
    await page.waitForFunction(() => document.body.innerText.includes("The turn may have reached the provider"), null, { timeout: 10000 });
    const errorText = await pageText(page);
    assert(evidence, "ambiguous provider outcome is reconciled without automatic replay",
      errorText.includes("The turn may have reached the provider")
      && fixture.state.requestStatusReads.length >= 1
      && fixture.state.requestStatusReads.every((entry) => entry.matched === true)
      && !errorText.includes("raw-fixture-provider-error"));

    const desktopScroll = await page.evaluate(async () => {
      const transcript = document.querySelector("#asst-log");
      const view = document.querySelector("#view");
      const probe = document.createElement("div");
      probe.style.cssText = "height:2000px;flex:none";
      transcript.append(probe);
      transcript.scrollTop = 0;
      view.scrollTop = 0;
      scrollAssistantToEnd();
      await new Promise(requestAnimationFrame);
      await new Promise(requestAnimationFrame);
      const result = {
        transcript_overflow: getComputedStyle(transcript).overflowY,
        transcript_at_end: Math.abs(transcript.scrollHeight - transcript.clientHeight - transcript.scrollTop) <= 2,
      };
      probe.remove();
      return result;
    });
    assert(evidence, "desktop Assistant auto-scrolls its transcript container",
      desktopScroll.transcript_overflow !== "visible" && desktopScroll.transcript_at_end,
      desktopScroll);

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
    const mobileScroll = await page.evaluate(async () => {
      const transcript = document.querySelector("#asst-log");
      const view = document.querySelector("#view");
      const probe = document.createElement("div");
      probe.style.cssText = "height:2000px;flex:none";
      transcript.append(probe);
      transcript.scrollTop = 0;
      view.scrollTop = 0;
      scrollAssistantToEnd();
      await new Promise(requestAnimationFrame);
      await new Promise(requestAnimationFrame);
      const result = {
        transcript_overflow: getComputedStyle(transcript).overflowY,
        view_scrollable: view.scrollHeight > view.clientHeight,
        view_at_end: Math.abs(view.scrollHeight - view.clientHeight - view.scrollTop) <= 2,
      };
      probe.remove();
      return result;
    });
    assert(evidence, "mobile Assistant auto-scrolls the view container",
      mobileScroll.transcript_overflow === "visible"
      && mobileScroll.view_scrollable
      && mobileScroll.view_at_end,
      mobileScroll);
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
    assert(evidence, "normal pending error and cancellation streams exercised", ["normal", "slow-cancel", "pending-confirm", "pending-cancel", "error"].every((s) => fixture.state.streamScenarios.some((row) => row.scenario === s)), fixture.state.streamScenarios);

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
    const report = evidenceForWrite(evidence, evidence.fixture_state);
    assertRedactedEvidenceReport(report);
    fs.writeFileSync(path.join(OUT_DIR, "agent-ui-smoke.json"), JSON.stringify(report, null, 2) + "\n");
  }
  process.exit(evidence.ok ? 0 : 1);
}

await main();
