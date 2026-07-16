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
          } else if (url.pathname === "/api/v1/account/login/poll") value = { state: "pending" };
          else if (url.pathname === "/api/v1/account/login/cancel") {
            state.events.push("request:cancel");
            value = { cancelled: true };
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

    await page.evaluate(() => { window.__microsoftAccountSmoke.guardAvailable = false; });
    await reader.getByRole("button", { name: "Connect" }).click();
    await page.getByText("Sign-in could not be kept active while the browser is open.").waitFor();
    const failedEvents = await page.evaluate(() => window.__microsoftAccountSmoke.events);
    assert(report, "guard failure prevents backend start and browser launch",
      failedEvents.filter((event) => event === "request:start:reader").length === 1
      && failedEvents.filter((event) => event.startsWith("native:open:")).length === 1,
      failedEvents);
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
