// iSyncYou web UI — single-page app (vanilla, no framework, no build).
//
// Served from /app.js (embedded in the binary); the page CSP is `script-src
// 'self'`, so this is the only script. Pure consumer of the JSON API in lib.rs.
//
// SECURITY: untrusted data (item names, mail headers, archived JSON values) is
// ONLY ever inserted via DOM text nodes / .textContent — never innerHTML. The
// one untrusted-HTML surface, a mail body, is shown in a sandboxed
// <iframe src="/api/v1/view"> that the server locks down with MAIL_CSP.
//
// This PR (shell + parity): a full app shell (sidebar, command palette, sync
// widget) plus a generic-but-styled item view that keeps every service working.
// Later PRs replace each service's view with a bespoke one in this same system.
"use strict";

/* ---------------------------------------------------------------- dom helpers */
function el(tag, props, ...kids) {
  const n = document.createElement(tag);
  if (props) for (const [k, v] of Object.entries(props)) {
    if (v == null) continue;
    if (k === "class") n.className = v;
    else if (k === "text") n.textContent = v;            // safe: text node
    else if (k === "html") n.innerHTML = v;              // ONLY trusted, in-code SVG
    else if (k.startsWith("on") && typeof v === "function") n.addEventListener(k.slice(2), v);
    else if (k === "dataset") Object.assign(n.dataset, v);
    else n.setAttribute(k, v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    n.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return n;
}
const $ = (sel, root = document) => root.querySelector(sel);
const clear = (n) => { while (n.firstChild) n.removeChild(n.firstChild); return n; };

/* ---------------------------------------------------------------- lucide icons (inline) */
// Minimal Lucide subset (ISC). Stroke paths only; colored via currentColor.
const ICONS = {
  "layout-dashboard": "M3 3h8v9H3zM13 3h8v5h-8zM13 12h8v9h-8zM3 16h8v5H3z",
  mail: "M22 7l-10 6L2 7M2 5h20v14H2z",
  "hard-drive": "M22 12H2M5.5 17h.01M11 17h.01M2 12l3.5-7h13L22 12v5a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z",
  calendar: "M8 2v4M16 2v4M3 8h18M3 5h18v16H3zM3 5a0 0 0 0 1 0 0",
  users: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8M22 21v-2a4 4 0 0 0-3-3.87M16 3.13A4 4 0 0 1 16 11",
  "check-square": "M9 11l3 3L22 4M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11",
  notebook: "M2 6h4M2 10h4M2 14h4M2 18h4M6 3h13a1 1 0 0 1 1 1v16a1 1 0 0 1-1 1H6a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z",
  share2: "M18 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6M6 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6M18 22a3 3 0 1 0 0-6 3 3 0 0 0 0 6M8.6 13.5l6.8 3.9M15.4 6.6L8.6 10.5",
  search: "M21 21l-4.3-4.3M11 19a8 8 0 1 0 0-16 8 8 0 0 0 0 16",
  folder: "M4 4h6l2 3h8v12H4z",
  file: "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6",
  download: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M7 10l5 5 5-5M12 15V3",
  "rotate-ccw": "M3 2v6h6M3.5 8a9 9 0 1 0 2.1-3.4L3 8",
  play: "M5 3l14 9-14 9z", pause: "M6 4h4v16H6zM14 4h4v16h-4z",
  "refresh-cw": "M21 2v6h-6M3 12a9 9 0 0 1 15-6.7L21 8M3 22v-6h6M21 12a9 9 0 0 1-15 6.7L3 16",
  x: "M18 6L6 18M6 6l12 12", "chevron-right": "M9 6l6 6-6 6", "chevron-left": "M15 6l-6 6 6 6",
  paperclip: "M21.4 11.05l-9.19 9.19a5 5 0 0 1-7.07-7.07l9.19-9.19a3.5 3.5 0 0 1 4.95 4.95l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48",
  "external-link": "M15 3h6v6M10 14L21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
  clock: "M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20M12 6v6l4 2",
};
function icon(name, cls = "icon") {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 24 24"); svg.setAttribute("class", cls); svg.setAttribute("aria-hidden", "true");
  const p = document.createElementNS(ns, "path");
  p.setAttribute("d", ICONS[name] || ICONS.file);
  svg.append(p); return svg;
}
function logoGlyph(size = 30) {
  // Own brand glyph: a sync-arc / cloud motif in the accent gradient.
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 48 48"); svg.setAttribute("width", size); svg.setAttribute("height", size);
  svg.innerHTML =
    '<defs><linearGradient id="lg" x1="0" y1="0" x2="1" y2="1">' +
    '<stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs>' +
    '<rect x="3" y="3" width="42" height="42" rx="12" fill="url(#lg)" opacity="0.16"/>' +
    '<path d="M16 20a9 9 0 0 1 16-3" fill="none" stroke="url(#lg)" stroke-width="3.2" stroke-linecap="round"/>' +
    '<path d="M32 14v5h-5" fill="none" stroke="url(#lg)" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"/>' +
    '<path d="M32 28a9 9 0 0 1-16 3" fill="none" stroke="url(#lg)" stroke-width="3.2" stroke-linecap="round"/>' +
    '<path d="M16 34v-5h5" fill="none" stroke="url(#lg)" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"/>';
  return svg; // trusted, in-code SVG only
}

/* ---------------------------------------------------------------- charts (pure SVG, no lib) */
const SVGNS = "http://www.w3.org/2000/svg";
function svg(tag, attrs) {
  const n = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) n.setAttribute(k, v);
  return n;
}
const reduceMotion = () => window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
// animate a number node 0 -> target (easeOutCubic)
function countUp(node, target, dur = 900) {
  const n = Number(target);
  if (!isFinite(n) || reduceMotion()) { node.textContent = String(target); return; }
  const start = performance.now();
  (function tick(t) {
    const p = Math.min(1, (t - start) / dur), e = 1 - Math.pow(1 - p, 3);
    node.textContent = String(Math.round(n * e));
    if (p < 1) requestAnimationFrame(tick);
  })(performance.now());
}
// donut ring from segments [{value, color}] with a center total
function donutChart(segments, centerSub) {
  const total = segments.reduce((s, x) => s + x.value, 0) || 1;
  const R = 54, C = 2 * Math.PI * R, W = 16;
  const s = svg("svg", { viewBox: "0 0 140 140", class: "chart", style: "max-width:180px" });
  s.append(svg("circle", { cx: 70, cy: 70, r: R, fill: "none", stroke: "var(--bg-3)", "stroke-width": W }));
  let off = 0;
  segments.filter(x => x.value > 0).forEach(seg => {
    const len = (seg.value / total) * C;
    s.append(svg("circle", {
      cx: 70, cy: 70, r: R, fill: "none", stroke: seg.color, "stroke-width": W,
      "stroke-dasharray": `${len.toFixed(2)} ${(C - len).toFixed(2)}`,
      "stroke-dashoffset": (-off).toFixed(2), transform: "rotate(-90 70 70)", class: "donut-seg",
    }));
    off += len;
  });
  return el("div", { style: "position:relative;display:grid;place-items:center" }, s,
    el("div", { style: "position:absolute;text-align:center;pointer-events:none" },
      el("div", { class: "num tnum", style: "font-size:24px", text: String(total) }),
      el("div", { class: "dim", style: "font-size:11px", text: centerSub || "" })));
}
// horizontal bars from rows [{label, value, color}]
function barChart(rows) {
  const max = Math.max(1, ...rows.map(r => r.value));
  const box = el("div", { style: "display:flex;flex-direction:column;gap:10px" });
  rows.forEach(r => box.append(el("div", { style: "display:flex;align-items:center;gap:10px" },
    el("span", { class: "dim", style: "width:84px;font-size:12px;text-transform:capitalize;text-align:right" }, r.label),
    el("div", { style: "flex:1;height:10px;background:var(--bg-3);border-radius:999px;overflow:hidden" },
      el("div", { class: "bar-fill", style: `height:100%;width:${Math.round((r.value / max) * 100)}%;background:${r.color}` })),
    el("span", { class: "tnum", style: "width:42px;text-align:right;font-size:12px" }, String(r.value)))));
  return box;
}
// sparkline (area + line) from numeric points
function sparkline(points, h = 60) {
  const w = 320;
  let pts = points.slice();
  if (pts.length < 2) pts = [0, ...pts, 0];
  const max = Math.max(1, ...pts), min = Math.min(0, ...pts), span = (max - min) || 1;
  const step = w / (pts.length - 1);
  const xy = pts.map((p, i) => [i * step, h - 4 - ((p - min) / span) * (h - 12)]);
  const line = xy.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
  const s = svg("svg", { viewBox: `0 0 ${w} ${h}`, preserveAspectRatio: "none", class: "chart", style: `width:100%;height:${h}px` });
  s.append(svg("polygon", { points: `0,${h} ${line} ${w},${h}`, class: "spark-area" }));
  s.append(svg("polyline", { points: line, class: "spark-line" }));
  return s;
}

/* ---------------------------------------------------------------- api + util */
const CAP = {
  restore: "__RESTORE_CAP_TOKEN__",
  sync: "__SYNC_CAP_TOKEN__",
  share: "__SHARE_CAP_TOKEN__",
};
async function api(path) { const r = await fetch(path); if (!r.ok) throw new Error((await r.json().catch(() => ({}))).error || r.status); return r.json(); }
async function post(path, capToken) {
  const r = await fetch(path, { method: "POST", headers: capToken ? { "X-Capability-Token": capToken } : {} });
  const d = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(d.error || r.status);
  return d;
}
const qs = (o) => Object.entries(o).map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join("&");
const initials = (s) => (s || "?").trim().split(/[\s@.]+/).filter(Boolean).slice(0, 2).map(x => x[0].toUpperCase()).join("") || "?";
function fmtDate(s) {
  if (!s) return "";
  const d = new Date(s); if (isNaN(d)) return s;
  const now = Date.now(), diff = now - d.getTime();
  if (diff < 864e5 && d.getDate() === new Date().getDate()) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (diff < 6048e5) return d.toLocaleDateString([], { weekday: "short" });
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}
function fmtFullDate(s) {
  if (!s) return "";
  const d = new Date(s); if (isNaN(d)) return s;
  return d.toLocaleString([], { weekday: "short", year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}
// Parse an RFC 5322 address ("Name <user@host>" or bare "user@host").
function parseAddr(s) {
  if (!s) return { name: "", email: "" };
  const m = String(s).match(/^\s*"?([^"<]*?)"?\s*<([^>]+)>\s*$/);
  if (m) return { name: m[1].trim(), email: m[2].trim() };
  return { name: "", email: String(s).trim() };
}
const addrLabel = (s) => { const a = parseAddr(s); return a.name || a.email || ""; };

/* ---------------------------------------------------------------- services */
const SERVICES = [
  { id: "overview", label: "Overview", icon: "layout-dashboard" },
  { id: "mail", label: "Mail", icon: "mail" },
  { id: "onedrive", label: "OneDrive", icon: "hard-drive" },
  { id: "calendar", label: "Calendar", icon: "calendar" },
  { id: "contacts", label: "Contacts", icon: "users" },
  { id: "todo", label: "ToDo", icon: "check-square" },
  { id: "onenote", label: "OneNote", icon: "notebook" },
];
const RESTORABLE = new Set(["mail", "calendar", "contacts", "todo", "onenote"]);
const SHAREABLE = new Set(["onedrive"]);

/* ---------------------------------------------------------------- global state */
const App = { account: null, accounts: [], route: "overview", counts: {} };

/* ---------------------------------------------------------------- toasts */
function toast(msg, kind = "ok") {
  const box = $("#toasts");
  const t = el("div", { class: `toast ${kind}` }, icon(kind === "ok" ? "check-square" : "x"), el("span", { text: msg }));
  box.append(t);
  setTimeout(() => t.remove(), 3800);
}

/* ---------------------------------------------------------------- shell render */
function renderShell() {
  const acc = App.accounts.find(a => a.id === App.account) || {};
  const nav = el("nav", { class: "nav" },
    SERVICES.map(s => {
      const item = el("button", {
        class: "nav-item" + (App.route === s.id ? " active" : ""),
        style: `--svc: var(--svc-${s.id})`,
        dataset: { service: s.id },
        onclick: () => go(s.id),
      },
        icon(s.icon),
        el("span", { class: "label", text: s.label }),
        s.id !== "overview" ? el("span", { class: "count", text: App.counts[s.id] != null ? String(App.counts[s.id]) : "·" }) : null,
      );
      return item;
    }),
  );
  const sidebar = el("aside", { class: "sidebar" },
    el("div", { class: "brand" }, logoGlyph(30), el("div", { class: "wordmark", html: "iSync<b>You</b>" })),
    el("button", { class: "sb-account", onclick: openAccountSwitcher, title: "Switch account" },
      el("span", { class: "avatar", text: initials(acc.username) }),
      el("span", { class: "who" }, el("b", { text: acc.username || "no account" }), el("span", { class: "dim", text: "Microsoft 365" })),
    ),
    el("div", { class: "sb-section", text: "Library" }),
    nav,
    el("div", { class: "spacer" }),
    el("div", { id: "sync-widget", class: "sync-widget" }),
  );
  const topbar = el("header", { class: "topbar" },
    el("div", { class: "crumbs" }, el("b", { text: (SERVICES.find(s => s.id === App.route) || {}).label || "iSyncYou" })),
    el("div", { class: "spacer" }),
    el("button", { class: "search-trigger", onclick: openPalette },
      icon("search", "icon-sm"), el("span", { class: "label-text", text: "Search everything" }), el("span", { class: "kbd", text: "⌘K" })),
  );
  const main = el("main", { class: "main" }, topbar, el("div", { id: "view", class: "view" }));
  const app = clear($("#app"));
  app.append(sidebar, main);
  renderSyncWidget();
}

async function renderSyncWidget() {
  const box = $("#sync-widget"); if (!box) return;
  let st = { enabled: false, paused: false };
  try { st = await api("/api/v1/sync/state"); } catch {}
  clear(box);
  if (!st.enabled || !CAP.sync) {
    box.append(el("div", { class: "row" }, el("span", { class: "pill info" }, el("span", { class: "dot" }), "ready")));
    return;
  }
  const pill = st.paused
    ? el("span", { class: "pill warn" }, el("span", { class: "dot" }), "paused")
    : el("span", { class: "pill ok" }, el("span", { class: "dot" }), "syncing");
  box.append(
    el("div", { class: "row" }, pill),
    el("div", { class: "actions" },
      el("button", { onclick: () => syncCmd("now"), title: "Sync now" }, icon("refresh-cw", "icon-sm")),
      st.paused
        ? el("button", { onclick: () => syncCmd("resume"), title: "Resume" }, icon("play", "icon-sm"))
        : el("button", { onclick: () => syncCmd("pause"), title: "Pause" }, icon("pause", "icon-sm")),
    ),
  );
}
async function syncCmd(cmd) {
  try { await post(`/api/v1/sync/${cmd}`, CAP.sync); toast(`sync ${cmd}`); renderSyncWidget(); }
  catch (e) { toast("sync " + cmd + " failed: " + e.message, "err"); }
}
// Update sidebar nav count badges in place (rebuilding the shell would wipe the view).
function updateNavCounts() {
  for (const [svc, n] of Object.entries(App.counts)) {
    const c = document.querySelector(`.nav-item[data-service="${svc}"] .count`);
    if (c) c.textContent = String(n);
  }
}

/* ---------------------------------------------------------------- router */
function go(route) { location.hash = "#/" + route; }
function onRoute() {
  App.route = (location.hash.replace(/^#\//, "") || "overview").split("?")[0];
  if (!SERVICES.find(s => s.id === App.route)) App.route = "overview";
  renderShell();
  const view = $("#view");
  if (App.route === "overview") renderOverview(view);
  else if (App.route === "mail") renderMailView(view);
  else renderServiceView(view, App.route);
}

/* ---------------------------------------------------------------- overview (dashboard showpiece) */
// concrete hues for SVG charts (SVG presentation attributes don't take CSS vars)
const SVC_COLOR = {
  overview: "#6366f1", mail: "#6366f1", onedrive: "#38bdf8", calendar: "#fb7185",
  contacts: "#34d399", todo: "#fbbf24", onenote: "#a855f7", shared: "#94a3b8",
};
// bucket runs into per-day counts for the activity sparkline
function activityBuckets(runs, days = 14) {
  const now = Date.now(), b = new Array(days).fill(0);
  runs.forEach(r => {
    const t = Date.parse(r.finished_at || r.started_at);
    if (isNaN(t)) return;
    const diff = Math.floor((now - t) / 864e5);
    if (diff >= 0 && diff < days) b[days - 1 - diff]++;
  });
  return b;
}
async function renderOverview(view) {
  const acc0 = App.accounts.find(a => a.id === App.account) || {};
  clear(view).append(
    el("h1", { class: "view-title" }, "Welcome back" + (acc0.username ? ", " : ""),
      acc0.username ? el("span", { style: "background:var(--grad-accent);-webkit-background-clip:text;background-clip:text;color:transparent" }, acc0.username.split("@")[0]) : ""),
    el("p", { class: "view-sub", text: "Your Microsoft 365 archive at a glance." }),
  );
  const grid = el("div", { class: "grid cols-auto stagger" });
  view.append(grid);
  for (let i = 0; i < 3; i++) grid.append(el("div", { class: "card stat-tile" }, el("div", { class: "skel", style: "width:50%;height:44px" })));
  if (!App.account) { clear(grid).append(el("div", { class: "empty" }, el("h3", { text: "No account configured" }))); return; }
  try {
    const [st, cfg, act] = await Promise.all([
      api("/api/v1/status?" + qs({ account: App.account })),
      api("/api/v1/settings").catch(() => ({})),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 200 })).catch(() => ({ runs: [] })),
    ]);
    const services = st.services || [];
    services.forEach(s => { App.counts[s.service] = s.items; });
    updateNavCounts();
    // stat tiles with animated count-ups
    clear(grid);
    [
      ["layout-dashboard", st.totals?.items ?? 0, "Total items", true],
      ["download", st.totals?.archived ?? 0, "Archived bodies", true],
      ["hard-drive", st.onedrive_cursor ? "Live" : "—", "OneDrive delta", false],
    ].forEach(([ic, n, l, anim]) => {
      const num = el("div", { class: "num tnum", text: anim ? "0" : String(n) });
      grid.append(el("div", { class: "card stat-tile rise" }, el("div", { class: "ico" }, icon(ic)), num, el("div", { class: "lbl", text: l })));
      if (anim) countUp(num, n);
    });
    // charts
    if (services.length) {
      const charts = el("div", { class: "grid stagger", style: "grid-template-columns:repeat(auto-fit,minmax(300px,1fr));margin-top:24px" });
      view.append(el("h3", { class: "sb-section", text: "Library breakdown" }), charts);
      // donut + legend (items per service)
      const legend = el("div", { style: "display:flex;flex-direction:column;gap:7px;min-width:0" });
      services.forEach(s => legend.append(el("button", {
        class: "list-row", style: "padding:4px 6px;border:0;background:transparent;gap:8px", onclick: () => go(s.service),
      },
        el("span", { style: `width:10px;height:10px;border-radius:3px;flex:none;background:${SVC_COLOR[s.service] || "#888"}` }),
        el("span", { class: "grow truncate", style: "text-transform:capitalize", text: s.service }),
        el("span", { class: "tnum dim", text: String(s.items) }))));
      charts.append(el("div", { class: "card rise", style: "display:flex;gap:20px;align-items:center" },
        donutChart(services.map(s => ({ value: s.items, color: SVC_COLOR[s.service] || "#888" })), "items"), legend));
      // bar: items + archived per service
      charts.append(el("div", { class: "card rise" },
        el("div", { class: "dim", style: "font-size:12px;margin-bottom:14px" }, "Items per service"),
        barChart(services.map(s => ({ label: s.service, value: s.items, color: SVC_COLOR[s.service] || "#888" })))));
      // sparkline: sync activity per day
      charts.append(el("div", { class: "card rise" },
        el("div", { class: "dim", style: "font-size:12px;margin-bottom:8px" }, "Sync activity (14 days)"),
        sparkline(activityBuckets(act.runs || []))));
    }
    // recent activity timeline
    const runs = (act.runs || []).slice(0, 8);
    const actBox = el("div", { class: "card", style: "margin-top:24px;padding:6px 4px" });
    if (runs.length) runs.forEach(r => actBox.append(
      el("div", { class: "list-row", style: "cursor:default" },
        el("span", { class: "pill " + (r.status === "ok" ? "ok" : r.status === "error" ? "err" : "info") }, el("span", { class: "dot" })),
        el("div", { class: "grow" }, el("div", { class: "truncate", text: r.summary || r.kind }), el("div", { class: "dim", style: "font-size:12px", text: r.kind })),
        el("span", { class: "dim tnum", style: "font-size:12px", text: fmtDate(r.finished_at) })),
    ));
    else actBox.append(el("div", { class: "muted", style: "padding:12px", text: "No runs recorded yet." }));
    view.append(el("h3", { class: "sb-section", text: "Recent activity" }), actBox);
    // settings summary
    const sy = cfg.sync || {}, acc = (cfg.accounts || []).find(a => a.id === App.account) || {};
    const dl = el("dl", { class: "kv" });
    const kv = (k, v) => { dl.append(el("dt", { text: k }), el("dd", { text: v == null ? "—" : String(v) })); };
    kv("Account", acc.username || App.account); kv("Sync root", acc.sync_root); kv("Archive root", acc.archive_root);
    kv("Trash retention", (sy.trash_retention_days ?? "?") + " days"); kv("Body index (FTS)", sy.body_index ? "on" : "off"); kv("Change source", sy.change_source);
    view.append(el("h3", { class: "sb-section", text: "Settings" }), el("div", { class: "card" }, dl));
  } catch (e) { clear(grid).append(el("div", { class: "empty" }, el("h3", { text: "Could not load overview" }), el("p", { text: e.message }))); }
}

/* ---------------------------------------------------------------- generic service view (parity; bespoke per PR) */
const PAGE = 60;
async function renderServiceView(view, service) {
  const meta = SERVICES.find(s => s.id === service);
  clear(view).append(
    el("h1", { class: "view-title", text: meta.label }),
    el("div", { class: "view-sub" },
      el("input", { id: "svc-search", class: "input", style: "max-width:420px", placeholder: `Search ${meta.label}…`,
        onkeydown: (e) => { if (e.key === "Enter") doServiceSearch(service); } })),
  );
  const list = el("div", { id: "svc-list", class: "card", style: "padding:0;overflow:hidden" });
  const more = el("div", { id: "svc-more", style: "display:none;padding:14px;text-align:center" },
    el("button", { class: "btn ghost", onclick: () => loadMore(service) }, "Load more"));
  view.append(list, more);
  view._offset = 0; view._total = 0;
  await loadPage(service, true);
}
function itemRow(it) {
  const q = { account: App.account, service: it.service, id: it.remote_id };
  const row = el("div", { class: "list-row" },
    el("span", { class: "avatar", style: `--svc:var(--svc-${it.service});background:color-mix(in oklab,var(--svc-${it.service}) 30%,var(--bg-3))`, text: initials(it.name) }),
    el("div", { class: "grow" },
      el("div", { class: "truncate", text: it.name || "(no name)" }),
      el("div", { class: "dim truncate", style: "font-size:12px", text: `${it.item_type}${it.size ? " · " + fmtSize(it.size) : ""}` })),
    el("span", { class: "dim tnum", style: "font-size:12px", text: fmtDate(it.remote_mtime) }),
  );
  const actions = el("div", { style: "display:flex;gap:4px;align-items:center" });
  if (it.has_body) actions.append(el("a", { class: "btn ghost sm", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener", title: "Open" }, icon("external-link", "icon-sm")));
  if (CAP.restore && it.has_body && RESTORABLE.has(it.service))
    actions.append(el("button", { class: "btn ghost sm", title: "Restore to cloud", onclick: (e) => { e.stopPropagation(); doRestore(it, e.currentTarget); } }, icon("rotate-ccw", "icon-sm")));
  if (CAP.share && SHAREABLE.has(it.service))
    actions.append(el("button", { class: "btn ghost sm", title: "Share", onclick: (e) => { e.stopPropagation(); doShare(it, e.currentTarget); } }, icon("share2", "icon-sm")));
  row.append(actions);
  row.addEventListener("click", () => { if (it.has_body) window.open(`/api/v1/view?${qs(q)}`, "_blank", "noopener"); });
  return row;
}
function fmtSize(n) { if (n == null) return ""; const u = ["B", "KB", "MB", "GB"]; let i = 0; while (n >= 1024 && i < 3) { n /= 1024; i++; } return `${n.toFixed(i ? 1 : 0)} ${u[i]}`; }
async function loadPage(service, reset) {
  const view = $("#view"), list = $("#svc-list");
  if (reset) { clear(list); for (let i = 0; i < 6; i++) list.append(el("div", { class: "list-row" }, el("div", { class: "skel grow", style: "height:30px" }))); }
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service, limit: PAGE, offset: view._offset }));
    const items = d.items || [];
    if (reset) clear(list);
    if (reset && !items.length) { list.append(el("div", { class: "empty" }, icon((SERVICES.find(s => s.id === service) || {}).icon, "icon-lg"), el("h3", { text: `No ${service} items` }), el("p", { text: "Run a backup to populate this view." }))); return; }
    const frag = document.createDocumentFragment();
    items.forEach(it => frag.append(itemRow(it)));
    list.append(frag);
    view._offset += items.length; view._total = d.total ?? view._offset;
    $("#svc-more").style.display = view._offset < view._total ? "block" : "none";
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Could not load" }), el("p", { text: e.message }))); }
}
function loadMore(service) { loadPage(service, false); }
async function doServiceSearch(service) {
  const q = $("#svc-search").value.trim(); const view = $("#view");
  if (!q) { view._offset = 0; return loadPage(service, true); }
  const list = $("#svc-list"); clear(list);
  try {
    const d = await api("/api/v1/search?" + qs({ account: App.account, q }));
    const hits = (d.hits || []).filter(h => h.service === service);
    if (!hits.length) { list.append(el("div", { class: "empty" }, el("h3", { text: "No matches" }))); }
    else hits.forEach(it => list.append(itemRow(it)));
    $("#svc-more").style.display = "none";
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Search failed" }), el("p", { text: e.message }))); }
}

/* ---------------------------------------------------------------- mail (bespoke 3-pane client) */
// `offset` walks the raw /items page (which also contains mailbox folder rows);
// `folders` accumulates the folder items we skip so the displayed count reflects
// real messages (folders sort before messages, so the message total is exact).
const Mail = { offset: 0, msgTotal: 0, folders: 0, selected: null };
const MAIL_PAGE = 50;
async function renderMailView(view) {
  Mail.offset = 0; Mail.msgTotal = 0; Mail.folders = 0; Mail.selected = null;
  const rail = el("aside", { class: "mail-rail" },
    el("div", { class: "mail-rail-head" }, icon("mail"), el("b", { text: "Mailbox" })),
    el("input", { id: "mail-search", class: "input", placeholder: "Search mail…",
      onkeydown: (e) => { if (e.key === "Enter") mailSearch(); } }),
    el("nav", { class: "mail-folders" },
      el("button", { class: "mail-folder active", onclick: () => { $("#mail-search").value = ""; mailReload(); } },
        icon("mail", "icon-sm"), el("span", { class: "grow", text: "All messages" }),
        el("span", { id: "mail-total", class: "count tnum", text: App.counts.mail != null ? String(App.counts.mail) : "·" }))),
    el("div", { class: "spacer" }),
    el("p", { class: "mail-rail-foot dim", text: "Archived & read-only. Restore re-creates a copy in your mailbox." }),
  );
  const list = el("div", { id: "mail-list", class: "mail-list" });
  const reader = el("div", { id: "mail-reader", class: "mail-reader" });
  const layout = el("div", { id: "mail-layout", class: "mail-layout" }, rail, list, reader);
  clear(view).append(layout);
  renderMailReader(null);
  await mailLoadPage(true);
}
function mailReload() { Mail.offset = 0; Mail.msgTotal = 0; Mail.folders = 0; Mail.selected = null; renderMailReader(null); $("#mail-layout")?.classList.remove("reading"); mailLoadPage(true); }
async function mailLoadPage(reset) {
  const list = $("#mail-list"); if (!list) return;
  const old = list.querySelector(".mail-more"); if (old) old.remove();
  if (reset) { clear(list); for (let i = 0; i < 8; i++) list.append(el("div", { class: "mail-item skel-row" }, el("div", { class: "skel grow", style: "height:40px" }))); }
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "mail", limit: MAIL_PAGE, offset: Mail.offset }));
    const all = d.items || [];
    const msgs = all.filter(it => it.item_type === "message");      // hide mailbox folder rows
    Mail.folders += all.length - msgs.length;
    Mail.offset += all.length;                                      // advance over the raw page
    const apiTotal = d.total ?? 0;
    Mail.msgTotal = Math.max(0, apiTotal - Mail.folders);           // exact: folders sort first
    if (reset) clear(list);
    const t = $("#mail-total"); if (t) t.textContent = String(Mail.msgTotal);
    if (reset && !msgs.length) {
      if (Mail.offset < apiTotal) return mailLoadPage(false);       // page held only folders → keep going
      list.append(el("div", { class: "empty" }, icon("mail", "icon-lg"), el("h3", { text: "No mail archived" }), el("p", { text: "Run a backup to populate your mailbox." })));
      return;
    }
    const frag = document.createDocumentFragment();
    msgs.forEach(it => frag.append(mailRow(it)));
    list.append(frag);
    if (Mail.offset < apiTotal)
      list.append(el("div", { class: "mail-more" }, el("button", { class: "btn ghost sm", onclick: () => mailLoadPage(false) }, "Load more")));
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Could not load mail" }), el("p", { text: e.message }))); }
}
function mailRow(it) {
  const p = it.preview || {};
  const from = addrLabel(p.from);
  const subject = p.subject || it.name || "(no subject)";
  const when = p.date || it.remote_mtime;
  return el("button", {
    class: "mail-item" + (Mail.selected && Mail.selected.remote_id === it.remote_id ? " active" : ""),
    dataset: { id: it.remote_id }, onclick: () => mailSelect(it),
  },
    el("span", { class: "avatar mail-av", text: initials(from || subject) }),
    el("div", { class: "grow" },
      el("div", { class: "mi-top" },
        el("span", { class: "mi-from truncate", text: from || "(unknown sender)" }),
        el("span", { class: "mi-date dim tnum", text: fmtDate(when) })),
      el("div", { class: "mi-subject truncate" }, subject,
        p.has_html ? el("span", { class: "mi-flag", title: "Rich HTML message" }, icon("paperclip", "icon-sm")) : null),
      el("div", { class: "mi-snippet truncate dim", text: p.snippet || "" })),
  );
}
function mailSelect(it) {
  Mail.selected = it;
  document.querySelectorAll(".mail-item").forEach(r => r.classList.toggle("active", r.dataset.id === it.remote_id));
  $("#mail-layout")?.classList.add("reading");
  renderMailReader(it);
}
function mailBack() { Mail.selected = null; $("#mail-layout")?.classList.remove("reading"); document.querySelectorAll(".mail-item.active").forEach(r => r.classList.remove("active")); renderMailReader(null); }
function renderMailReader(it) {
  const box = $("#mail-reader"); if (!box) return;
  clear(box);
  if (!it) {
    box.append(el("div", { class: "empty mail-reader-empty" }, logoGlyph(64),
      el("h3", { text: "Select a message" }), el("p", { text: "Choose a message from the list to read it here." })));
    return;
  }
  const p = it.preview || {};
  const from = parseAddr(p.from);
  const subject = p.subject || it.name || "(no subject)";
  const when = p.date || it.remote_mtime;
  const q = { account: App.account, service: "mail", id: it.remote_id };
  const actions = el("div", { class: "mr-actions" },
    el("a", { class: "btn ghost sm", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener", title: "Open in new tab" }, icon("external-link", "icon-sm")));
  if (CAP.restore)
    actions.append(el("button", { class: "btn ghost sm", title: "Restore to cloud", onclick: (e) => doRestore(it, e.currentTarget) }, icon("rotate-ccw", "icon-sm"), "Restore"));
  const head = el("header", { class: "mail-reader-head" },
    el("button", { class: "mail-back btn ghost sm", title: "Back to list", onclick: mailBack }, icon("chevron-left", "icon-sm")),
    el("div", { class: "grow", style: "min-width:0" },
      el("h2", { class: "mr-subject", text: subject }),
      el("div", { class: "mr-meta" },
        el("span", { class: "avatar mail-av", text: initials(from.name || from.email || subject) }),
        el("div", { class: "grow", style: "min-width:0" },
          el("div", { class: "mr-from truncate" }, el("b", { text: from.name || from.email || "(unknown sender)" }),
            from.name && from.email ? el("span", { class: "dim", text: " <" + from.email + ">" }) : null),
          (p.to && p.to.length) ? el("div", { class: "mr-to dim truncate", text: "To: " + p.to.join(", ") }) : null),
        el("span", { class: "mr-date dim tnum", text: fmtFullDate(when) }))),
    actions);
  // The body is untrusted HTML; it stays isolated in a same-origin iframe that the
  // server locks down with MAIL_CSP (no scripts, no remote fetch, data: images only).
  const frame = el("iframe", { class: "mail-frame", src: `/api/v1/view?${qs(q)}`, title: "Message body", loading: "lazy" });
  box.append(head, frame);
}
async function mailSearch() {
  const q = $("#mail-search").value.trim();
  if (!q) return mailReload();
  const list = $("#mail-list"); clear(list);
  Mail.selected = null; renderMailReader(null); $("#mail-layout")?.classList.remove("reading");
  try {
    const d = await api("/api/v1/search?" + qs({ account: App.account, q }));
    const hits = (d.hits || []).filter(h => h.service === "mail");
    if (!hits.length) { list.append(el("div", { class: "empty" }, el("h3", { text: "No matches" }), el("p", { text: `Nothing in mail matches “${q}”.` }))); return; }
    hits.forEach(it => list.append(mailRow(it)));
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Search failed" }), el("p", { text: e.message }))); }
}

/* ---------------------------------------------------------------- actions */
async function doRestore(it, btn) {
  if (!confirm(`Restore this ${it.service} item to the cloud as a new copy?`)) return;
  btn.disabled = true;
  try { const d = await post("/api/v1/restore?" + qs({ account: App.account, service: it.service, id: it.remote_id }), CAP.restore); toast(`Restored (new id ${String(d.new_id).slice(0, 8)}…)`); }
  catch (e) { toast("Restore failed: " + e.message, "err"); } finally { btn.disabled = false; }
}
async function doShare(it, btn) {
  btn.disabled = true;
  try {
    const d = await post("/api/v1/share?" + qs({ account: App.account, service: it.service, id: it.remote_id, type: "view", scope: "anonymous" }), CAP.share);
    if (d.webUrl) { try { await navigator.clipboard.writeText(d.webUrl); } catch {} toast("Share link copied to clipboard"); }
  } catch (e) { toast("Share failed: " + e.message, "err"); } finally { btn.disabled = false; }
}

/* ---------------------------------------------------------------- account switcher */
function openAccountSwitcher() {
  if (App.accounts.length < 2) return;
  const i = App.accounts.findIndex(a => a.id === App.account);
  App.account = App.accounts[(i + 1) % App.accounts.length].id;
  toast("Switched to " + (App.accounts.find(a => a.id === App.account) || {}).username);
  onRoute();
}

/* ---------------------------------------------------------------- command palette */
let palette = null;
function openPalette() {
  if (palette) return;
  const input = el("input", { placeholder: "Search mail, files, events… or jump to a view", autofocus: "" });
  const results = el("div", { class: "results" });
  const scrim = el("div", { class: "palette-scrim", onclick: closePalette });
  const box = el("div", { class: "palette" }, input, results);
  palette = el("div", {}, scrim, box);
  document.body.append(palette);
  input.focus();
  let sel = 0, rows = [];
  const renderRes = (items) => {
    clear(results); rows = items;
    items.forEach((it, idx) => {
      const r = el("div", { class: "res" + (idx === sel ? " sel" : ""), onclick: it.run },
        icon(it.icon || "chevron-right", "icon-sm"), el("span", { text: it.label }),
        it.badge ? el("span", { class: "badge", text: it.badge }) : null);
      results.append(r);
    });
  };
  const jumps = SERVICES.map(s => ({ label: "Go to " + s.label, icon: s.icon, run: () => { closePalette(); go(s.id); } }));
  renderRes(jumps);
  let timer;
  input.addEventListener("input", () => {
    const q = input.value.trim(); clearTimeout(timer);
    if (!q) { sel = 0; return renderRes(jumps); }
    timer = setTimeout(async () => {
      const local = jumps.filter(j => j.label.toLowerCase().includes(q.toLowerCase()));
      let hits = [];
      try { const d = await api("/api/v1/search?" + qs({ account: App.account, q })); hits = (d.hits || []).slice(0, 8).map(h => ({ label: h.name || "(no name)", icon: (SERVICES.find(s => s.id === h.service) || {}).icon, badge: h.service, run: () => { closePalette(); if (h.has_body) window.open(`/api/v1/view?${qs({ account: App.account, service: h.service, id: h.remote_id })}`, "_blank", "noopener"); else go(h.service); } })); } catch {}
      sel = 0; renderRes([...local, ...hits]);
    }, 180);
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") return closePalette();
    if (e.key === "ArrowDown") { sel = Math.min(sel + 1, rows.length - 1); e.preventDefault(); }
    else if (e.key === "ArrowUp") { sel = Math.max(sel - 1, 0); e.preventDefault(); }
    else if (e.key === "Enter") { rows[sel]?.run(); return; }
    else return;
    [...results.children].forEach((c, i) => c.classList.toggle("sel", i === sel));
    results.children[sel]?.scrollIntoView({ block: "nearest" });
  });
}
function closePalette() { if (palette) { palette.remove(); palette = null; } }

/* ---------------------------------------------------------------- init */
async function init() {
  document.body.append(el("div", { id: "toasts", class: "toasts" }));
  window.addEventListener("hashchange", onRoute);
  window.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); openPalette(); }
  });
  try {
    const d = await api("/api/v1/accounts");
    App.accounts = d.accounts || [];
    if (App.accounts.length) App.account = App.accounts[0].id;
  } catch {}
  onRoute();
}
init();
