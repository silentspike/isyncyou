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

// --- boot health: no uncaught console errors while exercising the UI ---
check("no console errors on boot", consoleErrors.length === 0, `(${consoleErrors.slice(0, 3).join(" | ")})`);

await browser.close();
console.log(`== ci-ui-smoke: ${pass} passed, ${fail} failed ==`);
process.exit(fail === 0 ? 0 : 1);
