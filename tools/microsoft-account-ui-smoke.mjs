// Deterministic Android-bridge smoke for Microsoft account-picker guard ownership.
import { chromium } from "playwright";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const ACCOUNT_CAP = "fixture-account-cap";

function asset(relative) {
  return fs.readFileSync(path.join(ROOT, relative));
}

function serve() {
  const index = asset("gui/webui/src/index.html");
  const css = asset("gui/webui/src/app.css");
  const font = asset("gui/webui/src/assets/inter-var.woff2");
  const js = asset("gui/webui/src/app.js").toString("utf8").replace(
    /__([A-Z0-9_]+_CAP_TOKEN)__/g,
    (_match, token) => token === "ACCOUNT_CAP_TOKEN" ? ACCOUNT_CAP : "",
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
  const report = { schema_version: 1, ok: false, assertions: [] };
  const server = serve();
  let browser;
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const origin = `http://127.0.0.1:${server.address().port}`;
    browser = await chromium.launch();
    const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
    await page.addInitScript(() => {
      const state = { events: [], guardAvailable: true, loginSequence: 0 };
      window.__microsoftAccountSmoke = state;
      const reply = (message) => setTimeout(
        () => window.__isyBridge.onmessage({ data: JSON.stringify(message) }), 0,
      );
      window.__isyBridge = {
        postMessage(raw) {
          const msg = JSON.parse(raw);
          if (msg.t === "native") {
            let value = {};
            if (msg.op === "beginNetworkGuard") {
              state.events.push(`native:begin:${msg.payload.reason}`);
              value = state.guardAvailable ? { guard_id: "account-guard" } : {};
            } else if (msg.op === "openExternal") {
              state.events.push(`native:open:${msg.payload.kind}`);
              value = { ok: true };
            } else if (msg.op === "endNetworkGuard") {
              state.events.push("native:end");
              value = { ok: true };
            } else if (msg.op === "pushToken") value = { ok: true };
            reply({ t: "res", id: msg.id, status: 200, body: JSON.stringify(value) });
            return;
          }
          if (msg.t !== "req") return;
          const url = new URL(msg.path, "https://appassets.androidplatform.net");
          let value = {};
          if (url.pathname === "/api/v1/accounts") value = { accounts: [{
            id: "fixture",
            username: "Fixture account",
            auth: {
              reader: { state: "disconnected", identity_verified: false },
              writer: { state: "connected", identity_verified: true },
            },
          }] };
          else if (url.pathname === "/api/v1/settings") value = { accounts: [{ id: "fixture" }] };
          else if (url.pathname === "/api/v1/status") value = { services: [], totals: { items: 0, archived: 0 } };
          else if (url.pathname === "/api/v1/activity") value = { runs: [] };
          else if (url.pathname === "/api/v1/sync/state") value = { enabled: false, paused: false };
          else if (url.pathname === "/api/v1/account/login/start") {
            const body = typeof msg.body === "string" ? JSON.parse(msg.body) : msg.body;
            state.loginSequence += 1;
            state.events.push(`request:start:${body.role}`);
            value = {
              flow: "authorization_code_pkce",
              role: body.role,
              login_id: `fixture-login-${state.loginSequence}`,
              authorization_uri: "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize?prompt=select_account",
            };
          } else if (url.pathname === "/api/v1/account/login/poll") {
            if (state.failNextPoll) {
              state.failNextPoll = false;
              reply({ t: "res", id: msg.id, status: 503, body: JSON.stringify({ error: "unavailable" }) });
              return;
            }
            value = { state: "pending" };
          }
          else if (url.pathname === "/api/v1/account/login/cancel") {
            state.events.push("request:cancel");
            value = { cancelled: true };
            if (state.cancelDelayMs) {
              setTimeout(() => reply({ t: "res", id: msg.id, status: 200, body: JSON.stringify(value) }), state.cancelDelayMs);
              return;
            }
          }
          reply({ t: "res", id: msg.id, status: 200, body: JSON.stringify(value) });
        },
        onmessage: null,
      };
    });

    await page.goto(`${origin}/#/settings`, { waitUntil: "domcontentloaded" });
    await page.getByRole("button", { name: "Choose Microsoft account" }).click();
    const reader = page.locator(".acct-role").filter({ hasText: "iSyncYou Reader" });
    const writer = page.locator(".acct-role").filter({ hasText: "iSyncYou Writer" });
    await reader.getByRole("button", { name: "Connect" }).click();
    await page.getByRole("button", { name: "Open Microsoft account picker" }).click();
    await page.evaluate(() => { window.__microsoftAccountSmoke.failNextPoll = true; });
    await page.getByText("Reconnecting to the sign-in status…").waitFor({ timeout: 5000 });
    await page.getByText("Waiting for sign-in…", { exact: true }).waitFor({ timeout: 5000 });
    assert(report, "one transient bridge poll failure keeps the login attempt resumable",
      await page.getByRole("button", { name: "Open Microsoft account picker" }).isEnabled()
      && await page.getByRole("button", { name: "Start sign-in again" }).count() === 0);
    await page.getByRole("button", { name: "Cancel" }).click();
    await page.waitForFunction(() => window.__microsoftAccountSmoke.events.includes("native:end"));
    const events = await page.evaluate(() => window.__microsoftAccountSmoke.events);
    const begin = events.indexOf("native:begin:oauth");
    const start = events.indexOf("request:start:reader");
    const open = events.indexOf("native:open:account_authorize");
    const cancel = events.indexOf("request:cancel");
    const end = events.indexOf("native:end");
    assert(report, "guard is acquired before backend start and browser launch",
      begin >= 0 && begin < start && start < open, events);
    assert(report, "host attempt is cancelled before guard release",
      cancel > open && end > cancel, events);
    assert(report, "Reader and Writer expose independent connection state",
      await reader.getByText("Not connected").isVisible()
      && await writer.getByText("Connected").isVisible());
    await page.evaluate(() => { window.__microsoftAccountSmoke.cancelDelayMs = 150; });
    await reader.getByRole("button", { name: "Connect" }).click();
    await page.getByRole("button", { name: "Open Microsoft account picker" }).waitFor();
    await page.locator(".acct-menu-wrap > .scrim").click({ position: { x: 5, y: 5 } });
    await page.getByRole("button", { name: "Choose Microsoft account" }).click();
    await reader.getByRole("button", { name: "Connect" }).click();
    await page.getByRole("button", { name: "Open Microsoft account picker" }).waitFor();
    const serializedEvents = await page.evaluate(() => window.__microsoftAccountSmoke.events);
    const finalCancel = serializedEvents.lastIndexOf("request:cancel");
    const finalReaderStart = serializedEvents.lastIndexOf("request:start:reader");
    assert(report, "closing and immediately reopening waits for exact login cancellation",
      finalCancel >= 0 && finalReaderStart > finalCancel, serializedEvents);
    await page.getByRole("button", { name: "Cancel" }).click();
    await page.evaluate(() => { window.__microsoftAccountSmoke.cancelDelayMs = 0; });
    const menuBox = await page.locator(".acct-menu").boundingBox();
    const viewport = page.viewportSize();
    assert(report, "account role controls fit the mobile viewport",
      Boolean(menuBox && viewport
        && menuBox.x >= 0
        && menuBox.y >= 0
        && menuBox.x + menuBox.width <= viewport.width
        && menuBox.y + menuBox.height <= viewport.height));
    assert(report, "account menu has no horizontal overflow",
      await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
    if (process.env.ISY_SMOKE_SCREENSHOT) {
      await page.screenshot({ path: process.env.ISY_SMOKE_SCREENSHOT, fullPage: true });
    }

    const beforeGuardFailure = await page.evaluate(() => ({
      starts: window.__microsoftAccountSmoke.events.filter((event) => event === "request:start:reader").length,
      opens: window.__microsoftAccountSmoke.events.filter((event) => event.startsWith("native:open:")).length,
    }));
    await page.evaluate(() => { window.__microsoftAccountSmoke.guardAvailable = false; });
    await reader.getByRole("button", { name: "Connect" }).click();
    await page.getByText("Sign-in could not be kept active while the browser is open.").waitFor();
    const failedEvents = await page.evaluate(() => window.__microsoftAccountSmoke.events);
    assert(report, "guard failure prevents backend start and browser launch",
      failedEvents.filter((event) => event === "request:start:reader").length === beforeGuardFailure.starts
      && failedEvents.filter((event) => event.startsWith("native:open:")).length === beforeGuardFailure.opens,
      failedEvents);

    // Exercise the actual view with an intentionally held metadata response.
    // The fixture contains no provider data or live account operations.
    const snapshot = await page.evaluate(async () => {
      App.accounts = [{ id: "fixture", provider: "microsoft", auth: { reader: { state: "connected" } } }];
      App.account = "fixture";
      let calls = 0, release, held = false;
      const originalApi = api;
      const item = { service: "mail", item_type: "message", remote_id: "one", name: "Snapshot fixture", body: "body sentinel", preview: { subject: "Snapshot fixture", snippet: "snippet sentinel" } };
      api = async (url) => {
        if (url.startsWith("/api/v1/items?") && url.includes("service=mail")) {
          calls++;
          if (held) await new Promise(resolve => { release = resolve; });
          return { items: [item] };
        }
        return { items: [], runs: [] };
      };
      const wait = () => new Promise(resolve => setTimeout(resolve, 20));
      try {
        go("mail");
        for (let i = 0; i < 100 && !MailSnapshot.entries.size; i++) await wait();
        const stored = cachedMailSnapshot("fixture");
        go("overview"); held = true; go("mail");
        for (let i = 0; i < 100 && !release; i++) await wait();
        const before = { rows: document.querySelectorAll("#mail-list .mail-item:not(.skel-row)").length,
          label: document.querySelector("#mail-cache-provenance")?.textContent,
          searchDisabled: document.querySelector("#mail-search")?.disabled, calls };
        release();
        for (let i = 0; i < 100 && document.querySelector("#mail-search")?.disabled; i++) await wait();
        const after = { rows: document.querySelectorAll("#mail-list .mail-item:not(.skel-row)").length,
          searchDisabled: document.querySelector("#mail-search")?.disabled };
        go("overview"); release = null; held = true; go("mail");
        for (let i = 0; i < 100 && !release; i++) await wait();
        Mail.selected = { remote_id: "one" }; Mail.pendingSelect = "one";
        document.querySelector("#mail-reader").textContent = "private sentinel";
        document.querySelector("#mail-search").value = "query sentinel";
        api = async () => ({ accounts: [{ id: "fixture", provider: "microsoft", auth: { reader: { state: "disconnected" } } }] });
        await reloadAccounts();
        release(); await wait();
        const revoked = { rows: document.querySelectorAll("#mail-list .mail-item").length,
          readerCleared: !document.querySelector("#mail-reader").textContent.includes("private sentinel"),
          query: document.querySelector("#mail-search").value, selected: Mail.selected, pending: Mail.pendingSelect,
          entries: MailSnapshot.entries.size };
        rememberMailSnapshot("a", { items: [item] });
        rememberMailSnapshot("b", { items: [item] });
        rememberMailSnapshot("c", { items: [item] });
        const bounded = !cachedMailSnapshot("a") && MailSnapshot.entries.size === 2;
        rememberMailSnapshot("c", { items: [{ name: "x".repeat(1000001) }] });
        const oversizeCleared = !cachedMailSnapshot("c");
        rememberMailSnapshot("b", { items: [item] });
        MailSnapshot.entries.get("b").expires = Date.now() - 1;
        const expired = !cachedMailSnapshot("b");
        const finishes = [];
        api = () => new Promise(resolve => { finishes.push(resolve); });
        const first = fetchMailSnapshot("fixture", "/one"), second = fetchMailSnapshot("fixture", "/two");
        clearMailSnapshots();
        const third = fetchMailSnapshot("fixture", "/three");
        finishes[0]({ items: [] }); await first;
        const staleSafe = MailSnapshot.inflight.get("fixture") === third && MailSnapshot.entries.size === 0;
        finishes[1]({ items: [] }); await third;
        return { before, after, revoked, bounded, oversizeCleared, expired,
          deduplicated: first === second && first !== third, staleSafe,
          noBody: stored && stored.items[0].body === undefined && stored.items[0].preview.snippet === undefined };
      } finally { api = originalApi; clearMailSnapshots(); }
    });
    assert(report, "cached metadata renders before held refresh and search returns after it",
      snapshot.before.rows === 1 && snapshot.before.calls === 2 && snapshot.before.searchDisabled
      && snapshot.before.label === "Cached metadata · refreshing" && snapshot.after.rows === 1 && !snapshot.after.searchDisabled, snapshot);
    assert(report, "reader loss clears visible and retained mail state",
      snapshot.revoked.rows === 0 && snapshot.revoked.readerCleared && snapshot.revoked.query === ""
      && snapshot.revoked.selected === null && snapshot.revoked.pending === null && snapshot.revoked.entries === 0, snapshot.revoked);
    assert(report, "metadata snapshots are bounded, expire, omit bodies and deduplicate in-flight requests",
      snapshot.bounded && snapshot.oversizeCleared && snapshot.expired && snapshot.deduplicated && snapshot.staleSafe && snapshot.noBody, snapshot);

    const containment = await page.evaluate(async () => {
      if (!CSS.supports("content-visibility", "auto")) return { supported: false };
      App.accounts = [{ id: "fixture", provider: "microsoft", auth: { reader: { state: "connected" } } }]; App.account = "fixture";
      const originalApi = api;
      const items = Array.from({ length: 80 }, (_, i) => ({ service: "mail", item_type: "message",
        remote_id: `fixture-${i}`, name: i === 79 ? "unique-filter-target" : `message-${i}`,
        preview: { subject: i === 79 ? "unique-filter-target" : `message-${i}` } }));
      api = async url => url.startsWith("/api/v1/items?") && url.includes("service=mail") ? { items } : { items: [], runs: [] };
      try {
        go("overview"); await new Promise(r => setTimeout(r, 50));
        go("mail");
        for (let i = 0; i < 300 && document.querySelectorAll("#mail-list .mail-item:not(.skel-row)").length < 80; i++) await new Promise(r => setTimeout(r, 20));
        const list = document.querySelector("#mail-list"), rows = [...list.querySelectorAll(".mail-item:not(.skel-row)")];
        if (rows.length !== 80) return { supported: true, count: rows.length };
        // Control the render scheduler to verify the first viewport gets a
        // rendering opportunity and stale deferred batches cannot append.
        const originalFrame = window.requestAnimationFrame, originalTimeout = window.setTimeout;
        const frames = [], tasks = [];
        let deferred;
        try {
          window.requestAnimationFrame = callback => { frames.push(callback); return frames.length; };
          window.setTimeout = (callback, delay, ...args) => {
            if (delay === 0) { tasks.push(() => callback(...args)); return tasks.length; }
            return originalTimeout(callback, delay, ...args);
          };
          const rowCount = () => list.querySelectorAll(".mail-item:not(.skel-row)").length;
          mailRender(); const initial = rowCount();
          frames.shift()(0); frames.shift()(16); const beforeTask = rowCount();
          const stale = tasks.shift(); mailRender(); stale(); const afterStale = rowCount();
          frames.shift()(32); frames.shift()(48); tasks.shift()(); const afterBatch = rowCount();
          frames.shift()(64); const complete = rowCount();
          deferred = { initial, beforeTask, afterStale, afterBatch, complete };
        } finally { window.requestAnimationFrame = originalFrame; window.setTimeout = originalTimeout; }
        const currentRows = [...list.querySelectorAll(".mail-item:not(.skel-row)")];
        const applied = getComputedStyle(currentRows[0]).contentVisibility === "auto";
        currentRows[0].style.minHeight = "140px"; currentRows[1].style.minHeight = "180px";
        const heights = [currentRows[0].getBoundingClientRect().height, currentRows[1].getBoundingClientRect().height];
        list.scrollTop = list.scrollHeight;
        await new Promise(requestAnimationFrame); await new Promise(requestAnimationFrame);
        const last = currentRows.at(-1), rect = last.getBoundingClientRect(), view = list.getBoundingClientRect();
        const bottomVisible = rect.top < view.bottom && rect.bottom > view.top;
        last.focus({ preventScroll: true }); const focus = document.activeElement === last;
        Mail.q = "unique-filter-target"; mailRender();
        await new Promise(requestAnimationFrame);
        const searchRows = list.querySelectorAll(".mail-item:not(.skel-row)").length;
        return { supported: true, count: rows.length, applied, heights, bottomVisible, focus, searchRows, deferred };
      } finally { api = originalApi; Mail.q = ""; clearMailSnapshots(); }
    });
    assert(report, "mobile mail containment preserves variable-height rows, bottom focus and search",
      !containment.supported || (containment.count === 80 && containment.applied
        && containment.heights[0] >= 140 && containment.heights[1] >= 180
        && containment.bottomVisible && containment.focus && containment.searchRows === 1
        && containment.deferred.initial === 24 && containment.deferred.beforeTask === 24
        && containment.deferred.afterStale === 24 && containment.deferred.afterBatch === 64
        && containment.deferred.complete === 80), containment);
    report.ok = true;
  } catch (error) {
    report.error = String(error && error.stack ? error.stack : error);
  } finally {
    if (browser) await browser.close().catch(() => {});
    await new Promise((resolve) => server.close(resolve));
  }
  if (!report.ok) console.error(report.error);
  else console.log(`microsoft-account-ui-smoke: ${report.assertions.length} assertions passed`);
  process.exit(report.ok ? 0 : 1);
}

await main();
