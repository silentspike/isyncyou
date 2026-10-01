// Deterministic Living Agent UI smoke for issue #644.
//
// Reuses the real WebUI assets and the established loopback fixture server. All
// streamed values are synthetic public projections; no account or provider is used.
// The default `all` scenario remains the evidence gate. `functional` skips only
// host-sensitive performance assertions and screenshots, while `containment`
// exercises the stale/rejected event boundary for focused iteration.
import { chromium } from "playwright";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { makeFixtureServer, sendSseMessage, sleep } from "./agent-ui-smoke.mjs";

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const outFlag = process.argv.indexOf("--out");
if (outFlag >= 0 && !process.argv[outFlag + 1]) throw new Error("--out requires a directory");
const OUT = outFlag >= 0
  ? path.resolve(REPO, process.argv[outFlag + 1])
  : path.join(REPO, "docs/evidence/artifacts/issue-644");
const scenarioFlag = process.argv.indexOf("--scenario");
if (scenarioFlag >= 0 && !process.argv[scenarioFlag + 1]) {
  throw new Error("--scenario requires all, functional, or containment");
}
const SCENARIO = scenarioFlag >= 0 ? process.argv[scenarioFlag + 1] : "all";
if (!["all", "functional", "containment"].includes(SCENARIO)) {
  throw new Error("--scenario requires all, functional, or containment");
}
if (SCENARIO !== "all" && outFlag < 0) {
  throw new Error("focused scenarios require an explicit --out directory");
}
const CAPTURE_SCREENSHOTS = SCENARIO === "all";
const ASSERT_PERFORMANCE = SCENARIO === "all";
const ACTIVITY_ID = "abcdefghijklmnopqrstuv";
const LONG_PUBLIC_NAME = "<img src=x onerror=window.__livingInjected=true> " + "Long public label ".repeat(7);
const FINAL_TEXT = ("A source-backed fixture summary remains exact. ".repeat(16)).slice(0, 500);
const FIXTURE_TERMINAL_TURNS = new Set();

function startSse(res) {
  res.writeHead(200, {
    "content-type": "text/event-stream; charset=utf-8",
    "cache-control": "no-store",
    connection: "keep-alive",
  });
}

function stage(stageName, status, scanned, total, hits, currentItem = null) {
  return {
    event: "stage_progress",
    schema_version: 1,
    activity_id: ACTIVITY_ID,
    activity_kind: "archive_search",
    stage: stageName,
    status,
    scanned,
    total,
    hits,
    current_item: currentItem,
    coverage_complete: ["complete", "failed", "skipped", "cancelled"].includes(status)
      ? status === "complete" : null,
    budget_reached: ["complete", "failed", "skipped", "cancelled"].includes(status)
      ? false : null,
    continuation_available: ["complete", "failed", "skipped", "cancelled"].includes(status)
      ? false : null,
  };
}

function publicResult(index, change = "add") {
  const key = `r${index.toString(36).padStart(21, "0")}`;
  const name = index === 0 && change === "add" ? LONG_PUBLIC_NAME
    : change === "enrich" ? "Enriched public result" : `Public result ${index + 1}`;
  return {
    result_key: key,
    change,
    service: index % 2 === 0 ? "mail" : "onedrive",
    item_id: `fixture-item-${index}`,
    name,
    item_type: index % 2 === 0 ? "message" : "file",
    display_path: `Fixture/Public result ${index + 1}`,
    sender: null,
    body_available: true,
    source: {
      service: index % 2 === 0 ? "mail" : "onedrive",
      item_id: `fixture-item-${index}`,
      label: name,
    },
  };
}

function partial(sequence, items) {
  return {
    event: "partial_result",
    schema_version: 1,
    activity_id: ACTIVITY_ID,
    stage: "names",
    sequence,
    items,
  };
}

async function searchStream(res, turn) {
  startSse(res);
  await sleep(180);
  for (const event of [
    stage("names", "queued", 0, null, 0),
    stage("bodies", "queued", 0, null, 0),
    stage("deep", "queued", 0, null, 0),
    stage("names", "running", 0, 100, 0, "Preparing public metadata"),
  ]) sendSseMessage(res, event);

  for (let index = 1; index <= 96; index += 1) {
    sendSseMessage(res, stage("names", "running", index, 100, Math.min(index * 2, 200), `Public item ${index}`));
    if (index % 16 === 0) await sleep(2);
  }
  for (let batch = 0; batch < 10; batch += 1) {
    sendSseMessage(res, partial(batch,
      Array.from({ length: 20 }, (_, offset) => publicResult(batch * 20 + offset))));
  }
  sendSseMessage(res, partial(9, Array.from({ length: 20 }, (_, offset) => publicResult(180 + offset))));
  sendSseMessage(res, partial(10, [publicResult(0, "enrich")]));
  sendSseMessage(res, stage("names", "complete", 100, 100, 200));
  await sleep(180);
  sendSseMessage(res, stage("bodies", "running", 80, 200, 35, "Checking public availability"));
  sendSseMessage(res, stage("bodies", "complete", 200, 200, 80));
  sendSseMessage(res, stage("deep", "running", 3, 8, 3, "Selecting public source 3"));
  sendSseMessage(res, stage("deep", "complete", 8, 8, 4));

  for (let index = 0; index < FINAL_TEXT.length; index += 1) {
    sendSseMessage(res, { event: "token", text: FINAL_TEXT[index] });
    if (index % 20 === 19) await sleep(40);
  }
  FIXTURE_TERMINAL_TURNS.add(turn);
  sendSseMessage(res, { event: "done", reason: "complete" });
  await sleep(20);
  res.end();
}

async function directStream(res, turn) {
  startSse(res);
  await sleep(100);
  sendSseMessage(res, { event: "token", text: "Direct " });
  sendSseMessage(res, { event: "token", text: "answer." });
  FIXTURE_TERMINAL_TURNS.add(turn);
  sendSseMessage(res, { event: "done", reason: "complete" });
  res.end();
}

async function backupStream(res, turn) {
  startSse(res);
  await sleep(80);
  sendSseMessage(res, { event: "tool_call", name: "isyncyou", input: { op: "backup" } });
  sendSseMessage(res, {
    event: "confirmation_required",
    pending_id: `pending-${turn}`,
    token: `fixture-token-${turn}`,
    action_hash: `fixture-hash-${turn}`,
    preview: "Back up selected Microsoft 365 data",
    risk: "writes cloud backup data",
    expires_at_ms: Date.now() + 300000,
  });
  FIXTURE_TERMINAL_TURNS.add(turn);
  sendSseMessage(res, { event: "done", reason: "pending_confirmation" });
  res.end();
}

async function authorizationContainmentStream(res, turn, scenario) {
  startSse(res);
  await sleep(80);
  const actions = [
    ["search", "mail"],
    ["deep-search", "mail"],
    ["read", "onedrive"],
    ["list", "calendar"],
    ["export", "contacts"],
    ["restore-local", "onenote"],
    ["backup", "mail"],
    ["restore-cloud", "mail"],
    ["live-write", "mail"],
    ["share", "onedrive"],
  ];
  for (const [op, service] of actions) {
    sendSseMessage(res, {
      event: "tool_call",
      name: "isyncyou",
      input: {
        op,
        service,
        account: "private-account-alias",
        id: "private-item-id",
        recipient: "private@example.invalid",
        change: { body: "private-body" },
      },
    });
  }
  const previews = {
    "authorization-backup": ["0", "Back up selected Microsoft 365 data"],
    "authorization-restore": ["1", "Restore selected Microsoft 365 data"],
    "authorization-live-write": ["2", "Update one Microsoft 365 item"],
    "authorization-share": ["3", "Share one OneDrive item"],
  };
  const [index, preview] = previews[scenario] || previews["authorization-backup"];
  sendSseMessage(res, {
    event: "confirmation_required",
    pending_id: `policy-pending-${index}-${turn}`,
    token: `policy-token-${index}-${turn}`,
    action_hash: index.repeat(64),
    preview,
    risk: "Requires your approval",
    expires_at_ms: Date.now() + 300000,
  });
  FIXTURE_TERMINAL_TURNS.add(turn);
  sendSseMessage(res, { event: "done", reason: "pending_confirmation" });
  await sleep(20);
  res.end();
}

async function failureStream(res, turn) {
  startSse(res);
  await sleep(80);
  for (const event of [
    stage("names", "queued", 0, null, 0),
    stage("bodies", "queued", 0, null, 0),
    stage("deep", "queued", 0, null, 0),
    stage("names", "running", 3, null, 1, "Public metadata"),
    stage("names", "failed", 3, null, 1),
    stage("bodies", "skipped", 0, null, 0),
    stage("deep", "skipped", 0, null, 0),
  ]) sendSseMessage(res, event);
  sendSseMessage(res, { event: "error", message: "provider_request_failed" });
  FIXTURE_TERMINAL_TURNS.add(turn);
  sendSseMessage(res, { event: "done", reason: "error" });
  res.end();
}

async function invalidProgressStream(res, turn) {
  startSse(res);
  await sleep(80);
  const invalidStage = { ...stage("names", "queued", 0, null, 0), schema_version: 99 };
  sendSseMessage(res, invalidStage);
  sendSseMessage(res, invalidStage);
  sendSseMessage(res, invalidStage);
  sendSseMessage(res, {
    ...partial(0, [publicResult(0)]),
    schema_version: 99,
  });
  sendSseMessage(res, { event: "token", text: "Invalid progress was contained." });
  FIXTURE_TERMINAL_TURNS.add(turn);
  sendSseMessage(res, { event: "done", reason: "complete" });
  res.end();
}

async function sendLivingStream(res, scenario, turn) {
  if (scenario === "search") return searchStream(res, turn);
  if (scenario === "backup") return backupStream(res, turn);
  if (scenario.startsWith("authorization-")) {
    return authorizationContainmentStream(res, turn, scenario);
  }
  if (scenario === "error") return failureStream(res, turn);
  if (scenario === "invalid-progress") return invalidProgressStream(res, turn);
  return directStream(res, turn);
}

function scenarioForPrompt(prompt) {
  const value = String(prompt).toLowerCase();
  if (value.includes("authorization restore")) return "authorization-restore";
  if (value.includes("authorization live write")) return "authorization-live-write";
  if (value.includes("authorization share")) return "authorization-share";
  if (value.includes("authorization containment")) return "authorization-backup";
  if (value.includes("living search") || value.includes("living reduced")) return "search";
  if (value.includes("living backup")) return "backup";
  if (value.includes("living failure")) return "error";
  if (value.includes("living invalid progress")) return "invalid-progress";
  if (value.includes("slow cancellation")) return "slow-cancel";
  return "direct";
}

function check(report, name, condition) {
  report.assertions.push({ name, status: condition ? "pass" : "fail" });
  if (!condition) throw new Error(name);
}

async function sendPrompt(page, prompt, during = null) {
  const messages = page.locator('[data-agent-message="assistant"]');
  const before = await messages.count();
  await page.locator('[data-testid="agent-input"]').fill(prompt);
  await page.locator('[data-testid="agent-send"]').click();
  await page.waitForFunction((count) => document.querySelectorAll('[data-agent-message="assistant"]').length > count, before);
  const message = messages.nth(before);
  if (during) {
    await page.locator('[data-testid="agent-stop"]').waitFor({ state: "visible" });
    await during(message);
  }
  await page.waitForFunction(() => {
    const terminal = AssistantState.transcript.findLast(entry => entry.role === "assistant")?.doneReason;
    return Boolean(terminal) && document.querySelector('[data-testid="agent-stop"]')?.hidden === true;
  }, null, { timeout: 20000 });
  return message;
}

async function reducerBoundaryProbe(page) {
  return page.evaluate(async () => {
    const identity = { session_id: "s", turn_request_id: "r", turn_id: "t", stream_id: "x" };
    const stageValue = (activity_id, status = "queued") => ({
      event: "stage_progress", schema_version: 1, activity_id, activity_kind: "archive_search",
      stage: "names", status, scanned: 0, total: null, hits: 0, current_item: null,
      coverage_complete: null, budget_reached: null, continuation_available: null,
    });
    const stageEvent = (activity_id, status = "queued") => ({
      kind: "stage", identity, value: stageValue(activity_id, status),
    });

    let stageState = createAssistantActivityState(identity);
    for (let index = 0; index < AGENT_PROGRESS_MAX_STAGE_UPDATES; index += 1) {
      stageState = reduceAssistantActivity(stageState, stageEvent("aaaaaaaaaaaaaaaaaaaaaa"));
    }
    const stageOver = reduceAssistantActivity(stageState, stageEvent("aaaaaaaaaaaaaaaaaaaaaa"));

    let activityState = createAssistantActivityState(identity);
    for (const activityId of [
      "aaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbb",
      "cccccccccccccccccccccc", "dddddddddddddddddddddd",
    ]) activityState = reduceAssistantActivity(activityState, stageEvent(activityId));
    const activityOver = reduceAssistantActivity(activityState, stageEvent("eeeeeeeeeeeeeeeeeeeeee"));

    let resultState = createAssistantActivityState(identity);
    resultState = reduceAssistantActivity(resultState, stageEvent("aaaaaaaaaaaaaaaaaaaaaa"));
    resultState = reduceAssistantActivity(resultState, stageEvent("aaaaaaaaaaaaaaaaaaaaaa", "running"));
    for (let sequence = 0; sequence < 10; sequence += 1) {
      const items = Array.from({ length: 20 }, (_, offset) => {
        const index = sequence * 20 + offset;
        return {
          result_key: `r${index.toString(36).padStart(21, "0")}`, change: "add", service: "mail",
          item_id: `i${index}`, name: `Result ${index}`, item_type: "message",
          display_path: null, sender: null, body_available: true,
          source: { service: "mail", item_id: `i${index}`, label: `Result ${index}` },
        };
      });
      resultState = reduceAssistantActivity(resultState, {
        kind: "partial", identity, digest: `d${sequence}`,
        value: { event: "partial_result", schema_version: 1, activity_id: "aaaaaaaaaaaaaaaaaaaaaa", stage: "names", sequence, items },
      });
    }
    const resultOver = reduceAssistantActivity(resultState, {
      kind: "partial", identity, digest: "over",
      value: {
        event: "partial_result", schema_version: 1, activity_id: "aaaaaaaaaaaaaaaaaaaaaa", stage: "names", sequence: 10,
        items: [{
          result_key: "rzzzzzzzzzzzzzzzzzzzzz", change: "add", service: "mail", item_id: "over",
          name: "Over", item_type: "message", display_path: null, sender: null, body_available: true,
          source: { service: "mail", item_id: "over", label: "Over" },
        }],
      },
    });
    const terminal = finishAssistantActivityState(resultState, identity, "complete");
    const stale = reduceAssistantActivity(terminal, stageEvent("aaaaaaaaaaaaaaaaaaaaaa"));

    let rejectedCitations = 0;
    await handleAgentEvent({ event: "partial_result" }, {
      isCurrent: () => true,
      flushTokens() {}, stopTokenCaret() {},
      async onPartialResult() { return null; },
      addCitations() { rejectedCitations += 1; },
    });
    const pendingBefore = AssistantState.pendingCardsById.size;
    await handleAgentEvent({
      event: "confirmation_required", pending_id: "stale-pending",
      token: "stale-token", action_hash: "stale-hash",
    }, { isCurrent: () => false });
    const stalePendingDelta = AssistantState.pendingCardsById.size - pendingBefore;

    let terminalCounterState = createAssistantActivityState(identity);
    terminalCounterState = reduceAssistantActivity(terminalCounterState, stageEvent("aaaaaaaaaaaaaaaaaaaaaa"));
    terminalCounterState = reduceAssistantActivity(terminalCounterState, {
      kind: "stage", identity, value: { ...stageValue("aaaaaaaaaaaaaaaaaaaaaa", "running"), scanned: 50, hits: 4 },
    });
    const runningRegression = reduceAssistantActivity(terminalCounterState, {
      kind: "stage", identity, value: { ...stageValue("aaaaaaaaaaaaaaaaaaaaaa", "running"), scanned: 1, hits: 1 },
    });
    const terminalReconciled = reduceAssistantActivity(terminalCounterState, {
      kind: "stage", identity, value: {
        ...stageValue("aaaaaaaaaaaaaaaaaaaaaa", "complete"), scanned: 1, total: 1, hits: 1,
      },
    });
    const reconciledStage = terminalReconciled.activities.get("aaaaaaaaaaaaaaaaaaaaaa").stages.get("names");
    return {
      stage_limit: stageState.acceptedStageUpdates,
      stage_over: stageOver.lastDisposition,
      activities: activityState.activities.size,
      activity_over: activityOver.lastDisposition,
      results: resultState.results.size,
      result_over: resultOver.lastDisposition,
      terminal_results: terminal.results.size,
      stale: stale.lastDisposition,
      post_terminal: stale.diagnostics.post_terminal,
      rejected_citations: rejectedCitations,
      stale_pending_delta: stalePendingDelta,
      running_regression: runningRegression.lastDisposition,
      terminal_reconciled: terminalReconciled.lastDisposition,
      reconciled_status: reconciledStage.status,
      reconciled_scanned: reconciledStage.scanned,
      reconciled_hits: reconciledStage.hits,
      reconciled_total: reconciledStage.total,
      reconciled_diagnostic: terminalReconciled.diagnostics.counter_regression,
    };
  });
}

async function noOverlap(page) {
  return page.evaluate(() => [...document.querySelectorAll(".asst-stage")].every((row) => {
    const label = row.querySelector(".asst-stage-copy, .asst-stage-label")?.getBoundingClientRect();
    const counter = row.querySelector(".asst-stage-n")?.getBoundingClientRect();
    if (!label || !counter || !label.width || !counter.width) return true;
    return label.right <= counter.left + 1 || label.bottom <= counter.top + 1 || counter.bottom <= label.top + 1;
  }));
}

async function main() {
  fs.mkdirSync(OUT, { recursive: true });
  const report = {
    evidence_version: 1,
    scenario: SCENARIO,
    ok: false,
    assertions: [],
    assertion_count: 0,
    console_error_count: 0,
    page_error_count: 0,
    csp_violation_count: 0,
    failed_request_count: 0,
    non_self_request_count: 0,
    performance: {},
    reducer_bounds: {},
    screenshots: CAPTURE_SCREENSHOTS
      ? ["desktop-running.png", "desktop-terminal.png", "mobile-terminal.png", "reduced-motion.png"]
      : [],
  };
  const fixtureEvidence = {
    fixture404: [], fixtureErrors: [], console_errors: [], page_errors: [], browser_requests: [],
    runtime_transports: [], external_launches: [], non_fixture_origin_requests: [], assertions: [], screenshots: {},
  };
  const fixture = makeFixtureServer(fixtureEvidence, {
    scenarioForPrompt,
    sendStream: sendLivingStream,
    requireAgentStreamCapability: true,
  });
  fixture.setAgent("claude", "claude-sonnet-4");
  let browser;
  try {
    await new Promise(resolve => fixture.server.listen(0, "127.0.0.1", resolve));
    const address = fixture.server.address();
    const origin = `http://127.0.0.1:${address.port}`;
    browser = await chromium.launch();
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, reducedMotion: "no-preference" });
    await context.addInitScript(() => {
      localStorage.setItem("isy_agent_privacy_consent_v1", JSON.stringify({
        version: 2, providers: { claude: { accepted: true, timestamp: "fixture" } },
      }));
      window.__livingPerf = { long: [], cls: 0, csp: 0, marks: {} };
      try {
        new PerformanceObserver(list => list.getEntries().forEach(entry => window.__livingPerf.long.push({
          start: entry.startTime, duration: entry.duration,
        })))
          .observe({ type: "longtask", buffered: true });
        new PerformanceObserver(list => list.getEntries().forEach(entry => {
          if (!entry.hadRecentInput) window.__livingPerf.cls += entry.value;
        })).observe({ type: "layout-shift", buffered: true });
      } catch (_) {}
      addEventListener("securitypolicyviolation", () => { window.__livingPerf.csp += 1; });
    });
    const page = await context.newPage();
    const consoleErrors = [];
    const pageErrors = [];
    const failedRequests = [];
    const expectedCancelledTurns = new Set();
    const expectedCancelledRequests = [];
    const expectedTerminalRequests = [];
    const requests = [];
    page.on("console", message => { if (message.type() === "error") consoleErrors.push("redacted"); });
    page.on("pageerror", () => pageErrors.push("redacted"));
    page.on("requestfailed", request => {
      try {
        const failed = new URL(request.url());
        const turn = failed.searchParams.get("turn");
        if (failed.pathname === "/api/v1/agent/stream" && expectedCancelledTurns.has(turn)) {
          expectedCancelledRequests.push("agent_stream_cancelled");
          return;
        }
        if (failed.pathname === "/api/v1/agent/stream" && FIXTURE_TERMINAL_TURNS.has(turn)) {
          expectedTerminalRequests.push("agent_stream_terminal_close");
          return;
        }
      } catch (_) {}
      try {
        const failed = new URL(request.url());
        failedRequests.push({
          method: request.method(),
          path: failed.pathname,
          query_keys: [...failed.searchParams.keys()].sort(),
        });
      } catch (_) {
        failedRequests.push({ method: request.method(), path: "invalid_url", query_keys: [] });
      }
    });
    page.on("request", request => requests.push(request.url()));

    await page.goto(`${origin}/#/assistant`, { waitUntil: "domcontentloaded" });
    await page.locator('[data-testid="agent-input"]').waitFor();
    await page.evaluate(() => { window.__livingPerf.long = []; window.__livingPerf.cls = 0; });

    const accountLabels = await page.evaluate(() => {
      const previousAccounts = App.accounts;
      const previousAccount = App.account;
      const host = document.createElement("div");
      App.accounts = [
        { id: "controlled", username: "controlled" },
        { id: "me", username: "me" },
        { id: "slot-3", username: "person@example.invalid" },
      ];
      App.account = "controlled";
      renderAccountMenu(host);
      const rendered = host.textContent || "";
      const labels = App.accounts.map(accountDisplayLabel);
      App.accounts = previousAccounts;
      App.account = previousAccount;
      return { labels, rendered };
    });
    check(report, "internal account aliases use a user-facing fallback",
      accountLabels.labels[0] === "Microsoft 365 account"
      && accountLabels.labels[1] === "Microsoft 365 account");
    check(report, "configured Microsoft account name remains visible",
      accountLabels.labels[2] === "person@example.invalid");
    check(report, "account switcher never renders internal account aliases",
      !accountLabels.rendered.includes("controlled") && !accountLabels.rendered.includes("me"));

    let lifecycle;
    if (SCENARIO === "containment") {
      const unauthorizedPath = `${origin}/api/v1/agent/stream?turn=unauthorized-probe`;
      const [missingStreamCap, wrongStreamCap] = await Promise.all([
        context.request.get(unauthorizedPath),
        context.request.get(unauthorizedPath, {
          headers: { "X-Capability-Token": "wrong" },
        }),
      ]);
      const unauthorizedStreams = [missingStreamCap.status(), wrongStreamCap.status()];
      check(report, "agent stream rejects missing and wrong capability before handler open",
        unauthorizedStreams.every(status => status === 403)
        && fixture.state.agentStreamAuthRejects.length === 2
        && fixture.state.streamScenarios.length === 0);

      const authorization = await sendPrompt(page, "authorization containment");
      const authorizationText = await authorization.innerText();
      report.authorization_projection = {
        tool_row_count: await authorization.locator('[data-agent-tool-row="tool_call"]').count(),
        pending_card_count: await authorization.locator('[data-agent-pending-card="1"]').count(),
      };
      check(report, "all ten action projections use closed user-facing tool rows",
        report.authorization_projection.tool_row_count === 10);
      check(report, "tool rows omit raw account item recipient and change fields",
        !authorizationText.includes("private-account-alias")
        && !authorizationText.includes("private-item-id")
        && !authorizationText.includes("private@example.invalid")
        && !authorizationText.includes("private-body"));
      check(report, "backup projection creates one bounded pending card",
        await authorization.locator('[data-agent-pending-card="1"]').count() === 1
        && await authorization.locator('[data-agent-pending-confirm="1"]').count() === 1
        && await authorization.locator('[data-agent-pending-cancel="1"]').count() === 1);

      await page.evaluate(() => {
        const originalPostJson = postJson;
        window.__issue642ConfirmRequests = [];
        window.__issue642RestorePostJson = () => { postJson = originalPostJson; };
        postJson = async (path, capToken, value) => {
          if (path !== "/api/v1/agent/confirm") return originalPostJson(path, capToken, value);
          window.__issue642ConfirmRequests.push(structuredClone(value));
          if (window.__issue642ConfirmRequests.length === 1) {
            const error = new Error("confirmation_retryable");
            error.code = "confirmation_retryable";
            error.responseReceived = true;
            throw error;
          }
          return { result: "Completed successfully." };
        };
      });
      const firstCard = authorization.locator('[data-agent-pending-card="1"]').first();
      await firstCard.locator('[data-agent-pending-confirm="1"]').click();
      await firstCard.locator('[data-agent-pending-confirm="1"]', { hasText: "Retry" }).waitFor();
      await firstCard.locator('[data-agent-pending-confirm="1"]', { hasText: "Retry" }).click();
      await firstCard.getByText("Completed successfully.").waitFor();
      const confirmRequests = await page.evaluate(() => {
        const captured = window.__issue642ConfirmRequests.map(value => structuredClone(value));
        window.__issue642RestorePostJson();
        delete window.__issue642RestorePostJson;
        delete window.__issue642ConfirmRequests;
        return captured;
      });
      const [firstRequest, secondRequest] = confirmRequests;
      check(report, "ambiguous confirmation retry reuses one exact request and owner proof",
        confirmRequests.length === 2
        && firstRequest.request_id === secondRequest.request_id
        && firstRequest.session_id === secondRequest.session_id
        && firstRequest.turn_request_id === secondRequest.turn_request_id
        && firstRequest.turn_id === secondRequest.turn_id
        && firstRequest.pending === secondRequest.pending
        && firstRequest.action_hash === secondRequest.action_hash
        && firstRequest.token === secondRequest.token);
      check(report, "confirmed card removes authority controls and exposes fixed success only",
        await firstCard.locator(".asst-pending-actions").count() === 0
        && (await firstCard.innerText()).includes("Completed successfully."));

      const replaySafety = await page.evaluate(async () => {
        const original = postJson;
        const makeRecord = (id) => ({pending_id: id, token: "synthetic-token",
          action_hash: "synthetic-hash", session_id: "synthetic-session",
          turn_request_id: "synthetic-request", turn_id: "synthetic-turn", status: "pending"});
        const record = makeRecord("replayed-unknown");
        AssistantState.pendingCardsById.set(record.pending_id, record);
        try {
          postJson = async () => {
            const error = new Error("request_replayed");
            error.code = "request_replayed";
            error.responseReceived = true;
            throw error;
          };
          await confirmAgentPending(record.pending_id);
          const replayUnknown = record.status === "outcome_unknown" && !record.token
            && !AssistantState.confirmAttemptsByPendingId.has(record.pending_id);
          const rotated = makeRecord("rotated-authority");
          AssistantState.pendingCardsById.set(rotated.pending_id, rotated);
          const attempt = getOrCreateAgentConfirmAttempt(rotated);
          attempt.phase = "retryable";
          updateAgentPendingStatus(rotated, "retryable");
          const turn = {flushTokens() {}, stopTokenCaret() {},
            ownerProof: {session_id: rotated.session_id, turn_request_id: rotated.turn_request_id,
              turn_id: rotated.turn_id}, onOperationConfirmation() {}, setPending() {}};
          await handleAgentEvent({event: "confirmation_required", pending_id: rotated.pending_id,
            token: rotated.token, action_hash: rotated.action_hash}, turn);
          const sameAttempt = getOrCreateAgentConfirmAttempt(rotated) === attempt
            && pendingRecord(rotated.pending_id) === rotated && rotated.status === "retryable";
          await handleAgentEvent({event: "confirmation_required", pending_id: rotated.pending_id,
            token: "rotated-token", action_hash: rotated.action_hash}, turn);
          const rotationClosed = rotated.status === "outcome_unknown" && !rotated.token
            && !AssistantState.confirmAttemptsByPendingId.has(rotated.pending_id);
          return {replayUnknown, sameAttempt, rotationClosed};
        } finally {
          postJson = original;
          for (const id of ["replayed-unknown", "rotated-authority"]) {
            clearAgentPendingAuthority(pendingRecord(id));
            AssistantState.pendingCardsById.delete(id);
          }
        }
      });
      check(report, "error receipt replay preserves an unknown effect and erases authority",
        replaySafety.replayUnknown);
      check(report, "same authority event preserves the exact ambiguous confirmation attempt",
        replaySafety.sameAttempt);
      check(report, "rotated authority cannot replace an ambiguous confirmation attempt",
        replaySafety.rotationClosed);

      for (const [prompt, preview] of [
        ["authorization restore", "Restore selected Microsoft 365 data"],
        ["authorization live write", "Update one Microsoft 365 item"],
        ["authorization share", "Share one OneDrive item"],
      ]) {
        const message = await sendPrompt(page, prompt);
        const card = message.locator('[data-agent-pending-card="1"]');
        check(report, `${prompt} uses its closed pending preview`,
          await card.count() === 1 && (await card.innerText()).includes(preview));
        await card.locator('[data-agent-pending-cancel="1"]').click();
        await card.getByText("No changes were made.").waitFor();
        check(report, `${prompt} cancellation removes authority controls`,
          await card.locator(".asst-pending-actions").count() === 0);
      }
      const authorityState = await page.evaluate(() => ({
        attempts: AssistantState.confirmAttemptsByPendingId.size,
        live_authority: [...AssistantState.pendingCardsById.values()].filter(record =>
          record.token || record.action_hash || record.session_id
          || record.turn_request_id || record.turn_id).length,
        persisted: JSON.stringify({
          local: Object.fromEntries(Object.entries(localStorage)),
          session: Object.fromEntries(Object.entries(sessionStorage)),
        }),
      }));
      check(report, "terminal cards erase in-memory confirmation authority",
        authorityState.attempts === 0 && authorityState.live_authority === 0
        && await page.locator(".asst-pending-actions").count() === 0);
      check(report, "browser storage contains no pending token hash owner or native authority",
        !/policy-(pending|token)|action_hash|turn_request_id|native.*handle/i
          .test(authorityState.persisted));

      const invalidProgress = await sendPrompt(page, "living invalid progress");
      check(report, "repeated invalid progress produces one bounded warning",
        await invalidProgress.locator('[data-agent-stream-error="1"]').count() === 1);
      check(report, "rejected partial progress produces no citations",
        await invalidProgress.locator('[data-agent-citation]').count() === 0);

      report.reducer_bounds = await reducerBoundaryProbe(page);
      check(report, "terminal teardown erases reducer results and rejects stale events",
        report.reducer_bounds.terminal_results === 0
        && report.reducer_bounds.stale === "stale"
        && report.reducer_bounds.post_terminal === 1);
      check(report, "rejected partial results cannot add citations",
        report.reducer_bounds.rejected_citations === 0);
      check(report, "stale queued events cannot register pending actions",
        report.reducer_bounds.stale_pending_delta === 0);
      check(report, "running counter regression remains rejected",
        report.reducer_bounds.running_regression === "invalid");

      lifecycle = await page.evaluate(() => ({
        activity_cleanup: AssistantState.activeActivityCleanup === null,
        token_cleanup: AssistantState.activeTokenCleanup === null,
        active_stream: AssistantState.activeStream === null,
        visible_carets: document.querySelectorAll(".asst-token-caret:not([hidden])").length,
        perf: window.__livingPerf,
      }));
      check(report, "terminal paths release stream renderer and token resources",
        lifecycle.activity_cleanup && lifecycle.token_cleanup
        && lifecycle.active_stream && lifecycle.visible_carets === 0);
    } else {
    const searchMessage = await sendPrompt(page, "living search", async message => {
      await message.locator(".asst-stage.running").waitFor();
      if (CAPTURE_SCREENSHOTS) {
        await page.screenshot({ path: path.join(OUT, "desktop-running.png"), fullPage: true });
      }
      await page.evaluate(() => {
        window.__livingPerf.long = [];
        window.__livingPerf.cls = 0;
        window.__livingPerf.marks.after_running_screenshot = performance.now();
      });
      await message.locator(".asst-result").nth(49).waitFor({ timeout: 10000 });
      const stagger = await message.locator(".asst-result").evaluateAll(nodes => nodes.slice(0, 3)
        .map(node => getComputedStyle(node).animationDelay));
      check(report, "search results use bounded stagger delays",
        stagger.length === 3 && stagger[0] !== stagger[1] && stagger[1] !== stagger[2]);
      await message.locator(".asst-stage.complete .asst-stage-ic").first().waitFor();
      const stageAnimation = await message.locator(".asst-stage.complete .asst-stage-ic").first()
        .evaluate(node => getComputedStyle(node).animationName);
      check(report, "stage completion uses a checkmark transition", stageAnimation === "asstStageDone");
      await page.evaluate(() => { window.__livingPerf.marks.result_50 = performance.now(); });
      await page.waitForFunction(() => {
        const scroller = assistantCurrentScroller();
        return scroller && scroller.scrollHeight - scroller.clientHeight > 144;
      });
      await page.evaluate(() => new Promise(resolve => {
        requestAnimationFrame(() => requestAnimationFrame(resolve));
      }));
      const scrollState = await page.evaluate(() => {
        const scroller = assistantCurrentScroller();
        scroller.dispatchEvent(new WheelEvent("wheel", { deltaY: -100, bubbles: true }));
        scroller.scrollTop = 0;
        scroller.dispatchEvent(new Event("scroll"));
        scrollAssistantToEnd();
        return {
          top: scroller.scrollTop,
          height: scroller.scrollHeight,
          client_height: scroller.clientHeight,
          follow: AssistantState.followMode,
        };
      });
      report.scroll_probe = scrollState;
      check(report, "search content creates a scrollable reading position",
        scrollState.height > scrollState.client_height);
      check(report, "reader movement disables follow mode", scrollState.follow === false);
      await page.locator('[data-agent-jump-latest="1"]').waitFor({ state: "visible" });
      const beforeJump = await page.evaluate(() => assistantCurrentScroller().scrollTop);
      check(report, "reader scroll position remains unchanged while updates arrive", beforeJump === 0);
      const jump = page.locator('[data-agent-jump-latest="1"]');
      await jump.focus();
      const jumpState = await page.evaluate(() => {
        const button = document.querySelector('[data-agent-jump-latest="1"]');
        button.click();
        const scroller = assistantCurrentScroller();
        return {
          at_end: scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop <= 72,
          follow: AssistantState.followMode,
          focused: document.activeElement === button,
        };
      });
      check(report, "jump to latest restores follow mode", jumpState.follow);
      check(report, "jump to latest does not steal focus elsewhere", jumpState.focused);
      await page.locator('[data-testid="agent-input"]').focus();
    });
    const rendererPerf = await page.evaluate(() => ({
      long: window.__livingPerf.long.slice(),
      cls: window.__livingPerf.cls,
      marks: { ...window.__livingPerf.marks, terminal: performance.now() },
    }));
    check(report, "follow mode reaches the terminal bottom", await page.evaluate(() => {
      const scroller = assistantCurrentScroller();
      return scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop <= 72;
    }));

    check(report, "search renders the complete three-stage plan", await searchMessage.locator(".asst-stage").count() === 3);
    check(report, "search stages finish truthfully", await searchMessage.locator(".asst-stage.complete").count() === 3);
    check(report, "search dedupes and enriches 200 results", await searchMessage.locator(".asst-result").count() === 200);
    check(report, "hostile public labels remain text", (await searchMessage.innerText()).includes("<img src=x")
      && await searchMessage.locator(".asst-result img").count() === 0
      && !await page.evaluate(() => Boolean(window.__livingInjected)));
    const tokenMetrics = await searchMessage.locator(".asst-text").evaluate(node => ({
      text: node.textContent || "",
      events: Number(node.dataset.tokenEvents), writes: Number(node.dataset.tokenWrites),
      caret_hidden: node.querySelector(".asst-token-caret")?.hidden === true,
    }));
    report.primary_stream = {
      text_length: tokenMetrics.text.length,
      expected_text_length: FINAL_TEXT.length,
      token_events: tokenMetrics.events,
      token_dom_writes: tokenMetrics.writes,
    };
    check(report, "frame-batched token output remains exact", tokenMetrics.text === FINAL_TEXT);
    check(report, "500 token events use materially fewer DOM writes", tokenMetrics.events === 500 && tokenMetrics.writes < 100);
    check(report, "token caret stops at terminal", tokenMetrics.caret_hidden);
    check(report, "public UI contains no body excerpt fields", !(await searchMessage.innerText()).includes("body_excerpt"));
    check(report, "desktop labels and counters do not overlap", await noOverlap(page));
    if (CAPTURE_SCREENSHOTS) {
      await page.screenshot({ path: path.join(OUT, "desktop-terminal.png"), fullPage: true });
    }

    const direct = await sendPrompt(page, "living direct");
    check(report, "direct answer has no activity plan", await direct.locator(".asst-activity-host").count() === 0);
    check(report, "direct answer preserves streamed text", await direct.locator(".asst-text").innerText() === "Direct answer.");

    const backup = await sendPrompt(page, "living backup");
    check(report, "backup uses the closed display-only three-stage plan", await backup.locator('[data-agent-operation-stage]').count() === 3);
    check(report, "backup reaches PendingAction without implicit confirmation", await backup.locator('[data-agent-pending-card="1"] [data-agent-pending-confirm="1"]').isVisible());
    await backup.locator('[data-agent-pending-cancel="1"]').click();
    await backup.getByText("No changes were made.").waitFor();
    check(report, "pending cancellation removes both authority controls", await backup.locator(".asst-pending-actions").count() === 0);

    const failed = await sendPrompt(page, "living failure");
    check(report, "failed search closes remaining stages as skipped", await failed.locator(".asst-stage.failed").count() === 1
      && await failed.locator(".asst-stage.skipped").count() === 2);
    check(report, "provider error is rendered through closed copy", !(await failed.innerText()).includes("provider_request_failed"));

    const invalidProgress = await sendPrompt(page, "living invalid progress");
    check(report, "repeated invalid progress produces one bounded warning",
      await invalidProgress.locator('[data-agent-stream-error="1"]').count() === 1);
    check(report, "rejected partial progress produces no citations",
      await invalidProgress.locator('[data-agent-citation]').count() === 0);

    const cancelled = await sendPrompt(page, "living slow cancellation", async () => {
      const turn = await page.evaluate(() => AssistantState.activeTurnId);
      expectedCancelledTurns.add(turn);
      await page.locator('[data-testid="agent-stop"]').click();
    });
    check(report, "cancelled turn leaves no active caret", await cancelled.locator(".asst-token-caret:not([hidden])").count() === 0);

    report.reducer_bounds = await reducerBoundaryProbe(page);
    check(report, "activity cap accepts four and limits one over", report.reducer_bounds.activities === 4 && report.reducer_bounds.activity_over === "limited");
    check(report, "stage cap accepts 256 and limits one over", report.reducer_bounds.stage_limit === 256 && report.reducer_bounds.stage_over === "limited");
    check(report, "result cap accepts 200 and limits one over", report.reducer_bounds.results === 200 && report.reducer_bounds.result_over === "limited");
    check(report, "terminal teardown erases reducer results and rejects stale events", report.reducer_bounds.terminal_results === 0
      && report.reducer_bounds.stale === "stale" && report.reducer_bounds.post_terminal === 1);
    check(report, "rejected partial results cannot add citations", report.reducer_bounds.rejected_citations === 0);
    check(report, "stale queued events cannot register pending actions", report.reducer_bounds.stale_pending_delta === 0);
    check(report, "running counter regression remains rejected", report.reducer_bounds.running_regression === "invalid");
    check(report, "terminal counter regression closes with monotonic observed counters",
      report.reducer_bounds.terminal_reconciled === "accept"
      && report.reducer_bounds.reconciled_status === "complete"
      && report.reducer_bounds.reconciled_scanned === 50
      && report.reducer_bounds.reconciled_hits === 4
      && report.reducer_bounds.reconciled_total === null
      && report.reducer_bounds.reconciled_diagnostic === 1);

    await page.emulateMedia({ reducedMotion: "reduce" });
    const reduced = await sendPrompt(page, "living reduced");
    check(report, "reduced motion preserves the full search plan", await reduced.locator(".asst-stage").count() === 3);
    check(report, "reduced motion preserves all search results", await reduced.locator(".asst-result").count() === 200);
    const reducedTokenMetrics = await reduced.locator(".asst-text").evaluate(node => ({
      text: node.textContent || "",
      events: Number(node.dataset.tokenEvents),
      writes: Number(node.dataset.tokenWrites),
    }));
    report.reduced_motion = {
      text_length: reducedTokenMetrics.text.length,
      expected_text_length: FINAL_TEXT.length,
      token_events: reducedTokenMetrics.events,
      token_dom_writes: reducedTokenMetrics.writes,
    };
    check(report, "reduced motion preserves exact final text", reducedTokenMetrics.text === FINAL_TEXT);
    const reducedAnimations = await page.evaluate(() => {
      const probe = document.createElement("div");
      probe.className = "asst-result is-new";
      document.body.append(probe);
      const animation = getComputedStyle(probe).animationName;
      probe.remove();
      return animation;
    });
    check(report, "reduced motion disables decorative result animation", reducedAnimations === "none");
    if (CAPTURE_SCREENSHOTS) {
      await page.screenshot({ path: path.join(OUT, "reduced-motion.png"), fullPage: true });
    }

    await page.setViewportSize({ width: 390, height: 844 });
    check(report, "compact layout has no horizontal page overflow",
      await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth));
    check(report, "compact labels and counters do not overlap", await noOverlap(page));
    if (CAPTURE_SCREENSHOTS) {
      await page.screenshot({ path: path.join(OUT, "mobile-terminal.png"), fullPage: true });
    }

    lifecycle = await page.evaluate(() => ({
      activity_cleanup: AssistantState.activeActivityCleanup === null,
      token_cleanup: AssistantState.activeTokenCleanup === null,
      active_stream: AssistantState.activeStream === null,
      visible_carets: document.querySelectorAll(".asst-token-caret:not([hidden])").length,
      perf: window.__livingPerf,
    }));
    check(report, "terminal paths release stream renderer and token resources", lifecycle.activity_cleanup
      && lifecycle.token_cleanup && lifecycle.active_stream && lifecycle.visible_carets === 0);
    report.performance = {
      token_events: tokenMetrics.events,
      token_dom_writes: tokenMetrics.writes,
      long_task_count: rendererPerf.long.length,
      max_long_task_ms: Math.round(Math.max(0, ...rendererPerf.long.map(entry => entry.duration)) * 100) / 100,
      longest_task_start_ms: Math.round((rendererPerf.long
        .reduce((longest, entry) => entry.duration > longest.duration ? entry : longest, { duration: 0, start: 0 }).start) * 100) / 100,
      long_tasks: rendererPerf.long.map(entry => ({
        start_ms: Math.round(entry.start * 100) / 100,
        duration_ms: Math.round(entry.duration * 100) / 100,
      })),
      marks_ms: Object.fromEntries(Object.entries(rendererPerf.marks)
        .map(([key, value]) => [key, Math.round(value * 100) / 100])),
      cumulative_layout_shift: Math.round(rendererPerf.cls * 100000) / 100000,
    };
    if (ASSERT_PERFORMANCE) {
      check(report, "controlled renderer has no long task above 100 ms",
        report.performance.max_long_task_ms <= 100);
      check(report, "controlled renderer cumulative layout shift stays below 0.1",
        report.performance.cumulative_layout_shift < 0.1);
    }
    }

    const nonSelf = requests.filter(raw => {
      try { return new URL(raw).origin !== origin; } catch (_) { return true; }
    });
    report.console_error_count = consoleErrors.length;
    report.page_error_count = pageErrors.length;
    report.csp_violation_count = lifecycle.perf.csp;
    report.failed_request_count = failedRequests.length;
    report.failed_request_routes = failedRequests;
    report.expected_cancelled_request_count = expectedCancelledRequests.length;
    report.expected_terminal_close_count = expectedTerminalRequests.length;
    report.non_self_request_count = nonSelf.length;
    check(report, "browser emits no console or page errors", !consoleErrors.length && !pageErrors.length);
    check(report, "browser emits no CSP violations", lifecycle.perf.csp === 0);
    check(report, "browser has no unexpected failed or non-self request",
      !failedRequests.length && expectedCancelledRequests.length <= 1 && !nonSelf.length);
    check(report, "fixture has no missing routes or internal errors", !fixtureEvidence.fixture404.length && !fixtureEvidence.fixtureErrors.length);
    report.ok = true;
  } catch (error) {
    report.ok = false;
    report.failure = "smoke_failed";
    console.error(error instanceof Error ? error.stack : String(error));
  } finally {
    if (browser) await browser.close().catch(() => {});
    await new Promise(resolve => fixture.server.close(resolve));
    report.assertion_count = report.assertions.filter(row => row.status === "pass").length;
    fs.writeFileSync(path.join(OUT, "ui-smoke.json"), JSON.stringify(report, null, 2) + "\n");
  }
  if (!report.ok) process.exitCode = 1;
  else console.log(`agent-living-ui-smoke(${SCENARIO}): ${report.assertion_count} assertions passed`);
}

await main();
