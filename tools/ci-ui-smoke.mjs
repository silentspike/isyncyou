// Token-free headless UI smoke for the staging-e2e CI job.
//
// The CI-runnable port of tools/regression-probe.sh: same DOM invariants, driven
// through the `playwright` npm package + headless Chromium (instead of the local
// `playwright-cli` wrapper, which isn't on CI runners). Drives a RUNNING daemon
// serving the seeded fixture (see crates/connectors/examples/seed_fixture.rs), using
// the app's own window.go()/DOM clicks so it survives ref churn. No tokens.
//
// Usage:  node tools/ci-ui-smoke.mjs [URL]   (default http://127.0.0.1:8869/)
import { chromium } from "playwright";

const URL = (process.argv[2] || "http://127.0.0.1:8869/").replace(/\/+$/, "") + "/";
let pass = 0;
let fail = 0;
const consoleErrors = [];
const check = (name, ok, extra = "") => {
  if (ok) { console.log(`  PASS  ${name}`); pass++; }
  else { console.log(`  FAIL  ${name} ${extra}`); fail++; }
};

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
page.on("console", (m) => { if (m.type() === "error") consoleErrors.push(m.text()); });
page.on("pageerror", (e) => consoleErrors.push(String(e)));
// The app holds a persistent SSE stream open (live updates), so the network never
// goes idle — wait for the DOM, not networkidle.
await page.goto(URL, { waitUntil: "domcontentloaded" });
await page.waitForTimeout(2000);
const nav = async (v) => { await page.evaluate((x) => window.go(x), v); await page.waitForTimeout(1200); };

// --- AC3a: the mail body renders in a sandboxed iframe (#73) ---
await nav("mail");
await page.evaluate(() => { const r = document.querySelector(".mail-item"); if (r) r.click(); });
await page.waitForTimeout(1200);
const sandbox = await page.evaluate(() => {
  const f = document.querySelector(".mail-frame");
  return f ? f.getAttribute("sandbox") : "NO-IFRAME";
});
check("mail body iframe is sandboxed", (sandbox || "").includes("allow-same-origin"), `(got ${sandbox})`);

// --- AC3c: a OneNote page with no archived body shows a native empty card, never a
//     404/JSON iframe (#74 / #89 CC-3) ---
await nav("onenote");
await page.evaluate(() => {
  const ls = [...document.querySelectorAll(".note-leaf")];
  const t = ls[ls.length - 1];
  if (t) t.click();
});
await page.waitForTimeout(1200);
const note = await page.evaluate(() => {
  const r = document.querySelector(".note-reader");
  if (!r) return "NO-READER";
  return "iframe=" + !!r.querySelector("iframe") + ",empty=" + !!r.querySelector(".empty,[class*=empty]");
});
check("onenote no-body -> empty card, no iframe", note === "iframe=false,empty=true", `(got ${note})`);

// --- AC3b: opening an overlay then changing route removes every .scrim/.sheet (#75) ---
await nav("calendar");
await page.evaluate(() => {
  const b = [...document.querySelectorAll("button")].find(
    (x) => /\d\d:\d\d/.test(x.textContent) && x.closest("[class*=cal]"),
  );
  if (b) b.click();
});
await page.waitForTimeout(1000);
const opened = await page.evaluate(() => document.querySelectorAll(".scrim,.sheet").length);
await nav("mail");
const closed = await page.evaluate(() => document.querySelectorAll(".scrim,.sheet").length);
check("overlay closed on route change", closed === 0, `(opened ${opened}, after ${closed})`);

// --- AC4: mobile metrics — no horizontal overflow across 7 views @390x844 ---
await page.setViewportSize({ width: 390, height: 844 });
await page.waitForTimeout(500);
for (const v of ["overview", "mail", "onedrive", "calendar", "contacts", "todo", "onenote"]) {
  await nav(v);
  const ok = await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth);
  check(`${v}: no horizontal overflow`, ok);
}

// --- AC4: smallest interactive target stays tappable (>= 28px tall) ---
await nav("mail");
const minBtn = await page.evaluate(() => {
  const hs = [...document.querySelectorAll(".btn, .nav-subitem, .seg-btn")]
    .map((e) => e.getBoundingClientRect().height)
    .filter((h) => h > 0);
  return hs.length ? Math.round(Math.min(...hs)) : -1;
});
check(`min interactive target >= 28px (measured ${minBtn}px)`, minBtn >= 28);

// Pending self-hosted mail reads are bounded and isolated by account/scope.
// Use synthetic pending requests: no account or provider operation is changed.
const providerRead = await page.evaluate(async () => {
  const original = window.api;
  let started = 0, aborted = 0;
  window.api = (url, opts) => {
    if (!url.startsWith("/synthetic-provider-")) return original(url, opts);
    started++;
    return new Promise((resolve, reject) => {
      opts.signal.addEventListener("abort", () => { aborted++; reject(new Error("aborted")); });
    });
  };
  try {
    const sharedController = new AbortController();
    const a = providerMailRead("a", "", "live", "/synthetic-provider-live");
    const same = providerMailRead("a", "", "live", "/synthetic-provider-live");
    const superseded = same.then(() => false, () => true);
    const newest = providerMailRead("a", "", "live", "/synthetic-provider-live");
    const account = providerMailRead("b", "", "live", "/synthetic-provider-live");
    const shared = providerMailRead("a", "shared", "live", "/synthetic-provider-live", sharedController.signal);
    const sharedAgain = providerMailRead("a", "shared", "live", "/synthetic-provider-live", sharedController.signal);
    const busy = providerMailRead("a", "", "scopes", "/synthetic-provider-scopes").then(() => false, () => true);
    const settled = Promise.allSettled([a, newest, account, shared, sharedAgain]);
    const identity = a !== same && same !== newest && a !== account && shared !== sharedAgain;
    clearProviderMailReads(); sharedController.abort();
    return { identity, started, aborted, busy: await busy, superseded: await superseded,
      rejected: (await settled).every(x => x.status === "rejected"), empty: ProviderMailReads.size === 0 && ProviderSharedReads.size === 0 };
  } finally { window.api = original; }
});
check("pending provider mail reads stay bounded and account-scoped",
  providerRead.identity && providerRead.started === 4 && providerRead.aborted === 4 &&
  providerRead.busy && providerRead.superseded && providerRead.rejected && providerRead.empty);

// A later view waits for an old read but must issue its own authorized request.
const providerBarrier = await page.evaluate(async () => {
  const original = window.api, resolvers = [];
  window.api = (url, opts) => {
    if (url !== "/synthetic-provider-barrier") return original(url, opts);
    return new Promise((resolve, reject) => {
      resolvers.push(resolve);
      opts.signal.addEventListener("abort", () => reject(new Error("aborted")));
    });
  };
  try {
    const old = providerMailRead("a", "", "live", "/synthetic-provider-barrier");
    const next = providerMailRead("a", "", "live", "/synthetic-provider-barrier", undefined, () => true);
    resolvers[0]("old");
    await old;
    for (let i = 0; i < 10 && resolvers.length < 2; i++) await Promise.resolve();
    if (resolvers.length !== 2) return false;
    resolvers[1]("fresh");
    return await next === "fresh";
  } finally { clearProviderMailReads(); window.api = original; }
});
check("later provider view obtains a fresh read", providerBarrier);

// An account refresh revokes pending reads before account identity can change.
const providerRefresh = await page.evaluate(async () => {
  const original = window.api, accounts = App.accounts;
  let resolveAccounts, aborted = 0;
  window.api = (url, opts) => {
    if (url === "/api/v1/accounts") return new Promise(resolve => { resolveAccounts = resolve; });
    if (url === "/synthetic-provider-refresh") return new Promise((resolve, reject) => {
      opts.signal.addEventListener("abort", () => { aborted++; reject(new Error("aborted")); });
    });
    return original(url, opts);
  };
  try {
    const epoch = MailSnapshot.epoch;
    const pending = providerMailRead("a", "", "live", "/synthetic-provider-refresh");
    const settled = Promise.allSettled([pending]);
    const reload = reloadAccounts();
    const blocked = await providerMailRead("a", "", "live", "/synthetic-provider-refresh").then(() => false, () => true);
    resolveAccounts({accounts});
    await reload;
    return { aborted, blocked, epochAdvanced: MailSnapshot.epoch > epoch,
      rejected: (await settled)[0].status === "rejected", refreshCleared: providerAccountsRefreshing === 0,
      empty: ProviderMailReads.size === 0 };
  } finally { window.api = original; }
});
check("account refresh invalidates pending provider reads",
  providerRefresh.aborted === 1 && providerRefresh.blocked && providerRefresh.epochAdvanced &&
  providerRefresh.rejected && providerRefresh.refreshCleared && providerRefresh.empty);

// Reuse shell only for the same primary self-hosted account; never across identity or shared scope.
const shellReuse = await page.evaluate(() => {
  const saved = {accounts:App.accounts, account:App.account, counts:App.counts, route:App.route,
    scope:Mail.activeScope, scopeAccount:Mail.scopeAccount};
  try {
    App.accounts = [{id:"synthetic-a", username:"synthetic-a", provider:"self_hosted"}];
    App.account = "synthetic-a"; App.counts = {}; App.route = "overview";
    Mail.activeScope = ""; Mail.scopeAccount = App.account;
    renderShell();
    const original = document.querySelector(".sidebar");
    App.route = "mail";
    const reused = reuseMailRouteShell("overview");
    const routeUpdated = !!document.querySelector('.nav-item[data-service="mail"].active') &&
      !!document.querySelector("#subnav-mail") && document.querySelector(".topbar .crumbs b")?.textContent === "Mail";
    Mail.activeScope = "synthetic-shared";
    const sharedRejected = !reuseMailRouteShell("overview");
    Mail.activeScope = ""; App.account = "synthetic-b";
    const accountRejected = !reuseMailRouteShell("overview");
    return {reused, same:original === document.querySelector(".sidebar"), routeUpdated, sharedRejected, accountRejected};
  } finally {
    App.accounts = saved.accounts; App.account = saved.account; App.counts = saved.counts; App.route = saved.route;
    Mail.activeScope = saved.scope; Mail.scopeAccount = saved.scopeAccount;
    onRoute();
  }
});
check("primary mail shell reuse stays account and scope bound",
  shellReuse.reused && shellReuse.same && shellReuse.routeUpdated && shellReuse.sharedRejected && shellReuse.accountRejected);

// Older health responses must not overwrite a newer route's status.
await page.setViewportSize({width:1280, height:900});
const healthRace = await page.evaluate(async () => {
  const original = window.api, savedCounts = {...App.counts}, starts = [];
  let statusCalls = 0;
  window.api = (url, opts) => {
    if (url === "/api/v1/sync/state") return new Promise(resolve => starts.push(resolve));
    if (url.startsWith("/api/v1/activity?")) return Promise.resolve({runs:[]});
    if (url.startsWith("/api/v1/status?")) { statusCalls++; return Promise.resolve({services:[{service:"mail",items:42}]}); }
    return original(url, opts);
  };
  try {
    const old = renderSyncWidget(), fresh = renderSyncWidget();
    if (starts.length !== 2) return false;
    starts[1]({enabled:false,paused:false}); await fresh;
    starts[0]({enabled:false,paused:false}); await old;
    return statusCalls === 1 && App.counts.mail === 42;
  } finally { window.api = original; App.counts = savedCounts; }
});
check("stale health responses cannot overwrite newer state", healthRace);

// Provider change notifications refresh the first primary page and preserve shared scopes.
await nav("mail");
const providerLiveRefresh = await page.evaluate(async () => {
  const original = window.api, savedAccounts = App.accounts, savedAccount = App.account, savedMail = {...Mail}, calls = [];
  App.accounts = [{id:"synthetic-live", username:"synthetic-live", provider:"self_hosted"}];
  App.account = "synthetic-live";
  window.api = async (url, opts) => {
    if (!url.includes("live_metadata=1")) return original(url, opts);
    calls.push(url);
    return {items:[{id:"synthetic-delta",subject:"synthetic-delta-visible",mailboxIds:{},keywords:{}}],query_state:"synthetic-state",total:1};
  };
  try {
    Mail.activeScope = ""; Mail.pageCursor = 0; Mail.livePage = true;
    await mailLiveUpdate();
    const primaryVisible = !!document.querySelector("#mail-list")?.textContent.includes("synthetic-delta-visible");
    const primaryNoScope = !!calls[0] && !new URL(calls[0], location.href).searchParams.has("mail_scope");
    Mail.pageCursor = 50; await mailLiveUpdate();
    const laterPageProtected = calls.length === 1;
    Mail.pageCursor = 0; Mail.activeScope = "synthetic-shared"; await mailLiveUpdate();
    const sharedScope = !!calls[1] && new URL(calls[1], location.href).searchParams.get("mail_scope") === "synthetic-shared";
    const scopeCallsCorrect = calls.length === 2;
    const resolvers = [];
    window.api = () => new Promise((resolve, reject) => resolvers.push({resolve, reject}));
    Mail.activeScope = ""; Mail.pageCursor = 0; Mail.livePage = true;
    const older = mailLiveUpdate(), second = mailLiveUpdate(), third = mailLiveUpdate();
    const page = subject => ({items:[{id:subject,subject,mailboxIds:{},keywords:{}}],query_state:subject,total:1});
    const oneInflight = resolvers.length === 1;
    await Promise.all([second, third]);
    resolvers[0].resolve(page("synthetic-oldest")); await older;
    const oneFollowup = resolvers.length === 2;
    if (oneFollowup) resolvers[1].resolve(page("synthetic-newest"));
    for (let i = 0; i < 20 && Mail.liveUpdateRunning; i++) await Promise.resolve();
    const newestWins = document.querySelector("#mail-list")?.textContent.includes("synthetic-newest") &&
      !document.querySelector("#mail-list")?.textContent.includes("synthetic-oldest") && Mail.queryState === "synthetic-newest";
    const late = mailLiveUpdate();
    clearMailSnapshots();
    resolvers[2].resolve(page("synthetic-invalidated")); await late;
    const invalidatedIgnored = document.querySelector("#mail-list")?.textContent.includes("synthetic-newest") &&
      !document.querySelector("#mail-list")?.textContent.includes("synthetic-invalidated");
    Mail.activeScope = "synthetic-shared";
    const staleError = mailLiveUpdate();
    clearMailSnapshots();
    resolvers[3].reject(new Error("synthetic-error")); await staleError;
    const staleErrorIgnored = Mail.all.some(item => item.name === "synthetic-newest");
    let resolveCache;
    window.api = () => new Promise(resolve => { resolveCache = resolve; });
    Mail.activeScope = ""; Mail.livePage = false;
    const cacheRefresh = mailLiveUpdate();
    Mail.livePage = true;
    resolveCache({items:[{item_type:"message",remote_id:"synthetic-old-cache",name:"synthetic-old-cache"}]});
    await cacheRefresh;
    const modeRaceIgnored = Mail.all.some(item => item.name === "synthetic-newest");
    window.api = async () => ({});
    const priorState = Mail.queryState; await mailLiveUpdate();
    const invalidPrimaryPreserved = Mail.all.some(item => item.name === "synthetic-newest") && Mail.queryState === priorState;
    window.api = async () => ({items:[null],query_state:"invalid"});
    await mailLiveUpdate();
    const malformedPrimaryPreserved = Mail.all.some(item => item.name === "synthetic-newest");
    Mail.activeScope = "synthetic-shared"; await mailLiveUpdate();
    const invalidSharedHidden = Mail.all.length === 0;
    return primaryVisible && primaryNoScope && laterPageProtected && sharedScope && scopeCallsCorrect &&
      oneInflight && oneFollowup && newestWins && invalidatedIgnored && staleErrorIgnored && modeRaceIgnored &&
      invalidPrimaryPreserved && malformedPrimaryPreserved && invalidSharedHidden;
  } finally {
    window.api = original; App.accounts = savedAccounts; App.account = savedAccount; Object.assign(Mail, savedMail);
  }
});
check("provider first-page live updates preserve scope and pagination", providerLiveRefresh);

// --- boot health: no uncaught console errors while exercising the UI ---
check("no console errors on boot", consoleErrors.length === 0, `(${consoleErrors.slice(0, 3).join(" | ")})`);

await browser.close();
console.log(`== ci-ui-smoke: ${pass} passed, ${fail} failed ==`);
process.exit(fail === 0 ? 0 : 1);
