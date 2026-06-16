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
  list: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  image: "M19 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V5a2 2 0 0 0-2-2zM8.5 10a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3M21 15l-5-5L5 21",
  "file-text": "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6M16 13H8M16 17H8M10 9H8",
  table: "M3 3h18v18H3zM3 9h18M3 15h18M9 3v18M15 3v18",
  music: "M9 18V5l12-2v13M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0M21 16a3 3 0 1 1-6 0 3 3 0 0 1 6 0",
  film: "M3 3h18v18H3zM7 3v18M17 3v18M3 7h4M3 12h18M3 17h4M17 7h4M17 17h4",
  archive: "M21 8v13H3V8M1 3h22v5H1zM10 12h4",
  code: "M16 18l6-6-6-6M8 6l-6 6 6 6",
  "map-pin": "M21 10c0 7-9 13-9 13s-9-6-9-13a9 9 0 0 1 18 0zM12 13a3 3 0 1 0 0-6 3 3 0 0 0 0 6",
  phone: "M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72c.13.96.36 1.9.7 2.81a2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45c.91.34 1.85.57 2.81.7A2 2 0 0 1 22 16.92z",
  building: "M6 22V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v18M6 22H2M18 22h4M9 6h.01M15 6h.01M9 10h.01M15 10h.01M9 14h.01M15 14h.01M10 22v-4h4v4",
  flag: "M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1zM4 22v-7",
  circle: "M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20",
  check: "M20 6L9 17l-5-5",
  settings: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z",
  shield: "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z",
  "shield-check": "M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10zM9 12l2 2 4-4",
  inbox: "M22 12h-6l-2 3h-4l-2-3H2M5.5 5h13l3.5 7v6a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-6z",
  filter: "M22 3H2l8 9.5V19l4 2v-8.5z",
  "arrow-down-up": "M3 16l4 4 4-4M7 20V4M21 8l-4-4-4 4M17 4v16",
};
function icon(name, cls = "icon") {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  // always keep the base `.icon` class (stroke:currentColor; fill:none); size
  // modifiers (icon-sm/icon-lg) only override width/height — without the base
  // class the browser would fill the stroke-only Lucide paths solid black.
  svg.setAttribute("class", cls === "icon" ? "icon" : "icon " + cls);
  svg.setAttribute("aria-hidden", "true");
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
// real activity chart: per-day stacked bars (success vs failed) with axis + legend
function activityChart(runs, days = 14) {
  const buckets = Array.from({ length: days }, () => ({ ok: 0, err: 0 }));
  const now = Date.now();
  runs.forEach(r => {
    const t = toDate(r.finished_at || r.started_at); if (!t) return;
    const diff = Math.floor((now - t.getTime()) / DAY_MS);
    if (diff >= 0 && diff < days) { const b = buckets[days - 1 - diff]; r.status === "error" ? b.err++ : b.ok++; }
  });
  const wrap = el("div", {});
  if (!runs.length) { wrap.append(el("div", { class: "dim", style: "padding:24px 0;text-align:center", text: "No sync activity recorded yet." })); return wrap; }
  const W = 560, H = 120, pad = 18, bw = (W - pad * 2) / days, max = Math.max(1, ...buckets.map(b => b.ok + b.err));
  const s = svg("svg", { viewBox: `0 0 ${W} ${H + 20}`, class: "chart", style: "width:100%;height:auto" });
  // baseline
  s.append(svg("line", { x1: pad, y1: H, x2: W - pad, y2: H, stroke: "var(--line-2)", "stroke-width": 1 }));
  buckets.forEach((b, i) => {
    const x = pad + i * bw + bw * 0.18, w = bw * 0.64;
    const total = b.ok + b.err; if (!total) return;
    const okH = (b.ok / max) * (H - 10), errH = (b.err / max) * (H - 10);
    if (b.ok) s.append(svg("rect", { x, y: H - okH, width: w, height: okH, rx: 2, fill: "var(--accent)" }));
    if (b.err) s.append(svg("rect", { x, y: H - okH - errH, width: w, height: errH, rx: 2, fill: "var(--err)" }));
  });
  // x ticks: first / mid / last day
  [[0, days - 1], [Math.floor(days / 2), Math.floor(days / 2)], [days - 1, 0]].forEach(([idx, ago]) => {
    const d = new Date(now - ago * DAY_MS);
    const t = svg("text", { x: pad + idx * bw + bw / 2, y: H + 15, "text-anchor": "middle", "font-size": 10, fill: "var(--text-lo)" });
    t.textContent = d.toLocaleDateString([], { month: "short", day: "numeric" });
    s.append(t);
  });
  wrap.append(s, el("div", { style: "display:flex;gap:16px;margin-top:8px;font-size:12px;color:var(--text-mid)" },
    el("span", {}, el("span", { style: "display:inline-block;width:9px;height:9px;border-radius:2px;background:var(--accent);margin-right:6px;vertical-align:-1px" }), "Successful"),
    el("span", {}, el("span", { style: "display:inline-block;width:9px;height:9px;border-radius:2px;background:var(--err);margin-right:6px;vertical-align:-1px" }), "Failed")));
  return wrap;
}

/* ---------------------------------------------------------------- api + util */
const CAP = {
  restore: "__RESTORE_CAP_TOKEN__",
  sync: "__SYNC_CAP_TOKEN__",
  share: "__SHARE_CAP_TOKEN__",
  verify: "__VERIFY_CAP_TOKEN__",
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
// Activity timestamps come back as unix seconds (audit_timestamp); everything
// else is an ISO/RFC string. Normalise both to a JS Date.
function toDate(s) {
  if (s == null || s === "") return null;
  if (/^\d{9,11}$/.test(String(s))) return new Date(Number(s) * 1000); // unix seconds
  const d = new Date(s); return isNaN(d) ? null : d;
}
function fmtDate(s) {
  const d = toDate(s); if (!d) return s ? String(s) : "";
  const now = Date.now(), diff = now - d.getTime();
  if (diff < 864e5 && d.getDate() === new Date().getDate()) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (diff < 6048e5) return d.toLocaleDateString([], { weekday: "short" });
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}
function fmtFullDate(s) {
  const d = toDate(s); if (!d) return s ? String(s) : "";
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
  { id: "todo", label: "To Do", icon: "check-square" },
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
      const cnt = App.counts[s.id];
      const connected = cnt != null && cnt > 0;
      const item = el("button", {
        class: "nav-item" + (App.route === s.id ? " active" : ""),
        style: `--svc: var(--svc-${s.id})`,
        dataset: { service: s.id },
        onclick: () => go(s.id),
      },
        icon(s.icon),
        el("span", { class: "label", text: s.label }),
        s.id !== "overview" ? el("span", { class: "nav-meta" },
          connected ? el("span", { class: "nav-dot", style: `background:var(--svc-${s.id})`, title: "Connected" }) : null,
          el("span", { class: "count", text: cnt != null ? String(cnt) : "·" })) : null,
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
    el("div", { class: "sb-section", text: "System" }),
    el("div", { id: "sync-widget", class: "sync-widget" }),
  );
  const topbar = el("header", { class: "topbar" },
    el("div", { class: "crumbs" }, el("b", { text: routeLabel(App.route) })),
    el("div", { class: "spacer" }),
    el("button", { class: "search-trigger", onclick: openPalette },
      icon("search", "icon-sm"), el("span", { class: "label-text", text: "Search everything" }), el("span", { class: "kbd", text: "⌘K" })),
    el("button", { class: "topbar-btn" + (App.route === "settings" ? " active" : ""), title: "Settings", onclick: () => go("settings") }, icon("settings", "icon-sm")),
  );
  const main = el("main", { class: "main" }, topbar, el("div", { id: "view", class: "view" }));
  const app = clear($("#app"));
  app.append(sidebar, main);
  renderSyncWidget();
}

// Sidebar "Archive health" card: real health pill + last-sync + sync controls.
async function renderSyncWidget() {
  const box = $("#sync-widget"); if (!box) return;
  let st = { enabled: false, paused: false }, runs = [];
  try { st = await api("/api/v1/sync/state"); } catch {}
  if (App.account) { try { runs = (await api("/api/v1/activity?" + qs({ account: App.account, limit: 30 }))).runs || []; } catch {} }
  const failed = runs.filter(r => r.status === "error").length, last = runs[0];
  clear(box);
  const pillCls = !runs.length ? "info" : failed ? "warn" : "ok";
  const pillTxt = !runs.length ? "Ready" : failed ? `${failed} failed` : "Healthy";
  box.append(
    el("div", { class: "sw-head" },
      el("span", { class: "pill " + pillCls }, el("span", { class: "dot" }), pillTxt),
      (st.enabled && CAP.sync) ? el("div", { class: "sw-actions" },
        el("button", { onclick: () => syncCmd("now"), title: "Sync now" }, icon("refresh-cw", "icon-sm")),
        st.paused ? el("button", { onclick: () => syncCmd("resume"), title: "Resume" }, icon("play", "icon-sm"))
          : el("button", { onclick: () => syncCmd("pause"), title: "Pause" }, icon("pause", "icon-sm"))) : null),
    el("div", { class: "sw-meta dim" }, last ? "Last sync " + fmtDate(last.finished_at) : "No syncs yet"),
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
const EXTRA_ROUTES = { search: "Search", settings: "Settings" };
const routeLabel = (r) => (SERVICES.find(s => s.id === r) || {}).label || EXTRA_ROUTES[r] || "iSyncYou";
function onRoute() {
  const raw = location.hash.replace(/^#\//, "") || "overview";
  App.route = raw.split("?")[0];
  App.query = (raw.split("?")[1] || "").replace(/^q=/, "");
  if (!SERVICES.find(s => s.id === App.route) && !EXTRA_ROUTES[App.route]) App.route = "overview";
  renderShell();
  const view = $("#view");
  if (App.route === "overview") renderOverview(view);
  else if (App.route === "mail") renderMailView(view);
  else if (App.route === "onedrive") renderOnedriveView(view);
  else if (App.route === "calendar") renderCalendarView(view);
  else if (App.route === "contacts") renderContactsView(view);
  else if (App.route === "todo") renderTodoView(view);
  else if (App.route === "onenote") renderOnenoteView(view);
  else if (App.route === "search") renderSearchView(view);
  else if (App.route === "settings") renderSettingsView(view);
  else renderServiceView(view, App.route);
}

/* ---------------------------------------------------------------- overview (dashboard showpiece) */
// concrete hues for SVG charts (SVG presentation attributes don't take CSS vars)
const SVC_COLOR = {
  overview: "#6366f1", mail: "#6366f1", onedrive: "#3b9eff", calendar: "#e5688f",
  contacts: "#3fb950", todo: "#d29922", onenote: "#a371f7", shared: "#768390",
};
// canonical service display names (consistent everywhere — no "Onedrive"/"Todo")
const SVC_LABEL = { overview: "Overview", mail: "Mail", onedrive: "OneDrive", calendar: "Calendar", contacts: "Contacts", todo: "To Do", onenote: "OneNote" };
const svcLabel = (id) => SVC_LABEL[id] || id;
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
  clear(view).append(
    el("h1", { class: "view-title", text: "Microsoft 365 archive overview" }),
    el("p", { class: "view-sub", text: "Backup health, activity and connected services at a glance." }),
    el("div", { id: "ov-body" }),
  );
  const body = $("#ov-body");
  body.append(el("div", { class: "card", style: "height:64px" }, el("div", { class: "skel", style: "height:24px;width:40%" })));
  if (!App.account) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "No account configured" }))); return; }
  try {
    const [st, act, cfg, sy] = await Promise.all([
      api("/api/v1/status?" + qs({ account: App.account })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 200 })).catch(() => ({ runs: [] })),
      api("/api/v1/settings").catch(() => ({})),
      api("/api/v1/sync/state").catch(() => ({})),
    ]);
    const services = (st.services || []).slice().sort((a, b) => b.items - a.items);
    services.forEach(s => { App.counts[s.service] = s.items; });
    updateNavCounts();
    const runs = act.runs || [];
    const failed = runs.filter(r => r.status === "error").length;
    const lastRun = runs[0], lastOk = runs.find(r => r.status === "ok");
    const items = st.totals?.items ?? 0, archived = st.totals?.archived ?? 0;
    const sync = cfg.sync || {}, acc = (cfg.accounts || []).find(a => a.id === App.account) || {};
    const healthy = failed === 0;
    clear(body);

    // ---- status header (the most important line)
    body.append(el("div", { class: "status-bar" },
      el("span", { class: "status-health" },
        el("span", { class: "chip " + (healthy ? "ok" : "warn") }, el("span", { class: "dot" }), healthy ? "Archive healthy" : "Attention needed")),
      el("div", { class: "status-facts" },
        el("span", {}, el("b", { text: String(services.length) }), " services connected"),
        el("span", { class: "sep", text: "·" }),
        el("span", {}, "Last sync ", el("b", { text: lastRun ? fmtFullDate(lastRun.finished_at) : "never" })),
        el("span", { class: "sep", text: "·" }),
        el("span", {}, el("b", { text: String(failed) }), failed === 1 ? " failed run" : " failed runs"),
        el("span", { class: "sep", text: "·" }),
        el("span", {}, el("b", { text: items.toLocaleString() }), " items protected")),
      el("div", { class: "spacer" }),
      el("div", { class: "status-actions" },
        CAP.sync ? el("button", { class: "btn primary sm", onclick: () => syncCmd("now") }, icon("refresh-cw", "icon-sm"), "Sync now") : null,
        el("button", { class: "btn sm", onclick: () => go("settings") }, icon("settings", "icon-sm"), "Settings"))));

    // ---- KPI row (modest numbers + real context)
    const kpi = el("div", { class: "kpi-row" });
    const kpiCard = (icn, head, val, unit, ctxEl) => el("div", { class: "card kpi" },
      el("div", { class: "kpi-head" }, icon(icn, "icon-sm"), head),
      el("div", { class: "kpi-val" }, String(val), unit ? el("span", { class: "unit", text: unit }) : null),
      ctxEl || null);
    kpi.append(
      kpiCard("layout-dashboard", "Items protected", items.toLocaleString(), "", el("div", { class: "kpi-ctx", text: `across ${services.length} services` })),
      kpiCard("download", "Archived bodies", archived.toLocaleString(), "", el("div", { class: "kpi-ctx", text: items ? `${Math.round(archived / items * 100)}% have content` : "—" })),
      kpiCard("rotate-ccw", "Failed runs", failed, "", el("div", { class: "kpi-ctx" }, el("span", { class: "chip " + (failed ? "err" : "ok") }, failed ? "needs review" : "all clear"))),
      kpiCard("clock", "Trash retention", sync.trash_retention_days ?? "—", "days", el("div", { class: "kpi-ctx", text: sync.body_index ? "full-text index on" : "index off" })));
    body.append(kpi);

    // ---- main grid: sync activity (real chart) + service breakdown (table)
    const grid = el("div", { class: "dash-grid" });
    body.append(grid);
    grid.append(el("div", { class: "card panel" },
      el("div", { class: "panel-head" }, icon("clock", "icon-sm"), "Sync activity", el("div", { class: "spacer" }),
        el("span", { class: "dim", style: "font-size:12px;text-transform:none;letter-spacing:0", text: `${runs.length} runs · 14 days` })),
      el("div", { class: "panel-body" }, activityChart(runs, 14))));
    // service breakdown table
    const tbl = el("table", { class: "svc-table" },
      el("thead", {}, el("tr", {}, el("th", { text: "Service" }), el("th", { class: "num", text: "Items" }), el("th", { class: "num", text: "Archived" }))),
    );
    const tb = el("tbody", {});
    const maxItems = Math.max(1, ...services.map(s => s.items));
    services.forEach(s => {
      const col = SVC_COLOR[s.service] || "#888";
      tb.append(el("tr", { onclick: () => go(s.service) },
        el("td", {}, el("div", { class: "svc-cell" }, el("span", { class: "svc-dot", style: `background:${col}` }), svcLabel(s.service))),
        el("td", { class: "num" }, el("div", { style: "display:flex;align-items:center;gap:8px;justify-content:flex-end" },
          el("div", { class: "svc-bar", style: "width:64px" }, el("i", { style: `width:${Math.round(s.items / maxItems * 100)}%;background:${col}` })),
          el("span", { text: s.items.toLocaleString() }))),
        el("td", { class: "num dim", text: (s.archived ?? 0).toLocaleString() })));
    });
    tbl.append(tb);
    grid.append(el("div", { class: "card panel" },
      el("div", { class: "panel-head" }, icon("hard-drive", "icon-sm"), "Service breakdown"),
      el("div", { class: "panel-body", style: "padding-top:var(--sp-2)" }, tbl)));

    // ---- recent runs
    const runsPanel = el("div", { class: "card panel" }, el("div", { class: "panel-head" }, icon("rotate-ccw", "icon-sm"), "Recent runs"));
    const recent = runs.slice(0, 6);
    if (recent.length) recent.forEach(r => {
      const ok = r.status === "ok";
      runsPanel.append(el("div", { class: "run-row" },
        el("span", { class: "chip " + (ok ? "ok" : "err") }, el("span", { class: "dot" }), ok ? "Success" : "Failed"),
        el("div", { class: "grow", style: "min-width:0" },
          el("span", { class: "run-kind", text: r.kind || "sync" }),
          el("div", { class: "run-sum truncate", text: r.summary || "" })),
        el("span", { class: "dim tnum", style: "font-size:12px;white-space:nowrap", text: fmtDate(r.finished_at) })));
    });
    else runsPanel.append(el("div", { class: "run-row dim", text: "No runs recorded yet." }));
    body.append(runsPanel);

    // ---- connection & policy (trust signals — real data only)
    body.append(el("div", { class: "card panel", style: "margin-top:var(--sp-3)" },
      el("div", { class: "panel-head" }, icon("users", "icon-sm"), "Connection & policy"),
      el("div", { class: "panel-body" }, el("dl", { class: "conn-grid" },
        connItem("Account", acc.username || App.account),
        connItem("Scheduled sync", sy.enabled ? (sy.paused ? "Paused" : "Running") : "Off"),
        connItem("Change source", sync.change_source || "—"),
        connItem("Body index", sync.body_index ? "On (full-text)" : "Off"),
        connItem("OneDrive delta", st.onedrive_cursor ? "Active" : "—"),
        connItem("Last successful", lastOk ? fmtFullDate(lastOk.finished_at) : "—")))));
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load overview" }), el("p", { text: e.message }))); }
}
function connItem(k, v) { return el("div", { class: "conn-item" }, el("dt", { text: k }), el("dd", { text: v == null ? "—" : String(v) })); }

/* shared per-view header: title + live metric line + honest chips (enterprise standard) */
function viewHeader(title, metrics, chips) {
  return el("header", { class: "svc-head" },
    el("div", { class: "grow", style: "min-width:0" },
      el("h1", { class: "view-title", style: "margin:0", text: title }),
      el("div", { class: "svc-metrics dim", text: metrics || "" })),
    chips && chips.length ? el("div", { class: "svc-chips" }, ...chips) : null);
}
const readonlyChip = () => el("span", { class: "chip muted" }, icon("shield-check", "icon-sm"), "Read-only");

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
const Mail = { all: [], filter: "all", sort: "newest", q: "", selected: null };
// auto-categorisation by sender/subject keywords (a labelled heuristic, not metadata)
const MAIL_CATS = [
  { key: "security", label: "Security", color: "#f85149", icon: "shield", re: /secur|verif|\bcode\b|login|sign[\s-]?in|password|\b2fa\b|\botp\b|alert|suspicious/i },
  { key: "billing", label: "Billing", color: "#3fb950", icon: "file-text", re: /invoice|payment|receipt|billing|subscription|renew|paypal|distrokid|\border\b|charged|refund/i },
  { key: "social", label: "Social", color: "#a371f7", icon: "users", re: /instagram|facebook|twitter|linkedin|tiktok|snapchat|social|follow|friend|tagged|mention/i },
  { key: "promo", label: "Promotions", color: "#d29922", icon: "mail", re: /spotify|newsletter|offer|\bdeal|\bsale\b|promo|unsubscribe|marketing|discount|\b% off/i },
];
function mailCat(it) {
  const p = it.preview || {};
  const hay = ((p.from || "") + " " + (p.subject || it.name || "")).toLowerCase();
  return MAIL_CATS.find(c => c.re.test(hay)) || { key: "other", label: "Other", color: "#768390", icon: "mail" };
}
const mailDate = (it) => { const p = it.preview || {}; return toDate(p.date || it.remote_mtime) || new Date(0); };

async function renderMailView(view) {
  Mail.all = []; Mail.filter = "all"; Mail.sort = Mail.sort || "newest"; Mail.q = ""; Mail.selected = null;
  clear(view).append(el("div", { id: "mail-page", class: "mail-page" },
    // header: title + live metrics + honest trust chips
    el("header", { class: "mail-head" },
      el("div", { class: "grow", style: "min-width:0" },
        el("h1", { class: "view-title", style: "margin:0", text: "Mail archive" }),
        el("div", { id: "mail-metrics", class: "mail-metrics dim", text: "Loading…" })),
      el("div", { class: "mail-chips" },
        el("span", { class: "chip muted" }, icon("shield-check", "icon-sm"), "Read-only"),
        el("span", { class: "chip muted" }, icon("rotate-ccw", "icon-sm"), "Restore-ready"))),
    // toolbar
    el("div", { class: "mail-toolbar" },
      el("div", { class: "tb-search" }, icon("search", "icon-sm"),
        el("input", { id: "mail-search", placeholder: "Search this mailbox…", oninput: () => { Mail.q = $("#mail-search").value.trim().toLowerCase(); mailRender(); } })),
      el("div", { class: "spacer", style: "flex:1" }),
      el("label", { class: "tb-sort" }, icon("arrow-down-up", "icon-sm"),
        el("select", { class: "input", onchange: (e) => { Mail.sort = e.target.value; mailRender(); } },
          el("option", { value: "newest", text: "Newest first" }),
          el("option", { value: "oldest", text: "Oldest first" }),
          el("option", { value: "sender", text: "Sender A–Z" }))),
      el("button", { class: "btn sm", title: "View sync log", onclick: () => go("overview") }, icon("clock", "icon-sm"), "Sync log")),
    // 3-pane: filter rail | list | reader
    el("div", { id: "mail-layout", class: "mail-layout" },
      el("aside", { id: "mail-rail", class: "mail-rail" }),
      el("div", { id: "mail-list", class: "mail-list" }),
      el("div", { id: "mail-reader", class: "mail-reader" }))));
  renderMailReader(null);
  const list = $("#mail-list");
  for (let i = 0; i < 9; i++) list.append(el("div", { class: "mail-item skel-row" }, el("div", { class: "skel grow", style: "height:46px" })));
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "mail", limit: 1000 }));
    Mail.all = (d.items || []).filter(it => it.item_type === "message");
    App.counts.mail = Mail.all.length; updateNavCounts();
    mailRenderRail(); mailRender();
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Could not load mail" }), el("p", { text: e.message }))); }
}
function mailFiltered() {
  let rows = Mail.all;
  const f = Mail.filter;
  if (f === "attach") rows = rows.filter(it => (it.preview || {}).attachments > 0);
  else if (f === "restore") rows = rows.filter(it => it.has_body);
  else if (f !== "all") rows = rows.filter(it => mailCat(it).key === f);
  if (Mail.q) rows = rows.filter(it => { const p = it.preview || {}; return ((p.subject || it.name || "") + " " + (p.from || "") + " " + (p.snippet || "")).toLowerCase().includes(Mail.q); });
  const dir = Mail.sort === "oldest" ? 1 : -1;
  if (Mail.sort === "sender") rows = rows.slice().sort((a, b) => addrLabel((a.preview || {}).from).localeCompare(addrLabel((b.preview || {}).from)));
  else rows = rows.slice().sort((a, b) => dir * (mailDate(a) - mailDate(b)));
  return rows;
}
function mailRenderRail() {
  const rail = $("#mail-rail"); if (!rail) return; clear(rail);
  const withAtt = Mail.all.filter(it => (it.preview || {}).attachments > 0).length;
  const restore = Mail.all.filter(it => it.has_body).length;
  const catCounts = {}; Mail.all.forEach(it => { const k = mailCat(it).key; catCounts[k] = (catCounts[k] || 0) + 1; });
  const folder = (key, label, icn, count, color) => el("button", { class: "mail-folder" + (Mail.filter === key ? " active" : ""), onclick: () => { Mail.filter = key; mailRenderRail(); mailRender(); } },
    el("span", { class: "mf-ico", style: color ? `color:${color}` : "" }, icon(icn, "icon-sm")),
    el("span", { class: "grow truncate", text: label }), el("span", { class: "count tnum", text: String(count) }));
  rail.append(
    el("div", { class: "mail-rail-sec", text: "Mailbox" }),
    folder("all", "All messages", "inbox", Mail.all.length),
    folder("attach", "With attachments", "paperclip", withAtt),
    folder("restore", "Restore-ready", "rotate-ccw", restore),
    el("div", { class: "mail-rail-sec", text: "Categories" }),
    ...MAIL_CATS.map(c => folder(c.key, c.label, c.icon, catCounts[c.key] || 0, c.color)),
    catCounts.other ? folder("other", "Other", "mail", catCounts.other, "#768390") : null,
    el("div", { class: "spacer" }),
    el("p", { class: "mail-rail-foot dim", text: "Read-only archive · restore re-creates a copy in your mailbox. Categories are auto-detected." }));
}
function mailRender() {
  const list = $("#mail-list"); if (!list) return;
  const rows = mailFiltered();
  const withAtt = Mail.all.filter(it => (it.preview || {}).attachments > 0).length;
  const archived = Mail.all.filter(it => it.has_body).length;
  const m = $("#mail-metrics"); if (m) m.textContent = `${Mail.all.length} messages · ${archived} with content · ${withAtt} with attachments`;
  clear(list);
  if (!Mail.all.length) { list.append(el("div", { class: "empty" }, emptyArt("empty-mail"), el("h3", { text: "No mail archived" }), el("p", { text: "Run a backup to populate your mailbox." }))); return; }
  if (!rows.length) { list.append(el("div", { class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No matches" }), el("p", { text: "Adjust the filter or search." }))); return; }
  const frag = document.createDocumentFragment();
  rows.forEach(it => frag.append(mailRow(it)));
  list.append(frag);
}
function mailRow(it) {
  const p = it.preview || {}, cat = mailCat(it);
  const from = addrLabel(p.from), subject = p.subject || it.name || "(no subject)";
  const sel = Mail.selected && Mail.selected.remote_id === it.remote_id;
  const badges = el("div", { class: "mi-badges" });
  if (p.attachments > 0) badges.append(el("span", { class: "mi-chip", title: p.attachments + " attachment(s)" }, icon("paperclip", "icon-sm"), String(p.attachments)));
  badges.append(el("span", { class: "mi-cat", style: `--c:${cat.color}`, text: cat.label }));
  return el("button", { class: "mail-item" + (sel ? " active" : ""), dataset: { id: it.remote_id }, onclick: () => mailSelect(it) },
    el("span", { class: "avatar mail-av", style: `--c:${cat.color}`, text: initials(from || subject) }),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "mi-top" }, el("span", { class: "mi-from truncate", text: from || "(unknown sender)" }),
        el("span", { class: "mi-date dim tnum", text: fmtDate(p.date || it.remote_mtime) })),
      el("div", { class: "mi-subject truncate", text: subject }),
      el("div", { class: "mi-bottom" },
        el("span", { class: "mi-snippet truncate dim", text: p.snippet || "" }),
        el("span", { class: "mi-status " + (it.has_body ? "ok" : "muted"), title: it.has_body ? "Full body archived" : "Header only" }, it.has_body ? "Body saved" : "Header only")),
      badges));
}
function mailSelect(it) {
  Mail.selected = it;
  document.querySelectorAll(".mail-item").forEach(r => r.classList.toggle("active", r.dataset.id === it.remote_id));
  $("#mail-layout")?.classList.add("reading");
  renderMailReader(it);
}
function mailBack() { Mail.selected = null; $("#mail-layout")?.classList.remove("reading"); document.querySelectorAll(".mail-item.active").forEach(r => r.classList.remove("active")); renderMailReader(null); }
// restrained archive-vault illustration for the empty reading pane (trusted in-code SVG)
const VAULT_SVG = '<svg viewBox="0 0 260 180" xmlns="http://www.w3.org/2000/svg"><g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="40" y="34" width="120" height="84" rx="10" opacity="0.35" transform="rotate(-7 100 76)"/><rect x="64" y="44" width="120" height="84" rx="10" opacity="0.6"/><path d="M64 60h120" opacity="0.6"/><circle cx="200" cy="120" r="38" fill="color-mix(in oklab, var(--accent) 10%, transparent)"/><circle cx="200" cy="120" r="38"/><circle cx="200" cy="120" r="14"/><path d="M200 92v10M200 138v10M172 120h10M218 120h10"/></g><path d="M191 119l6 6 12-13" fill="none" stroke="var(--accent)" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';
function renderMailReader(it) {
  const box = $("#mail-reader"); if (!box) return; clear(box);
  if (!it) {
    const archived = Mail.all.filter(x => x.has_body).length;
    const withAtt = Mail.all.filter(x => (x.preview || {}).attachments > 0).length;
    box.append(el("div", { class: "mail-empty" },
      el("div", { class: "vault-art", html: VAULT_SVG }),
      el("h3", { text: "Select a message to inspect it" }),
      el("p", { class: "dim", text: "Read-only Microsoft 365 mail archive. Choose a message to read its sanitized body and metadata." }),
      el("div", { class: "mail-empty-metrics" },
        metricCard("download", archived, "bodies archived"),
        metricCard("paperclip", withAtt, "with attachments"),
        metricCard("inbox", Mail.all.length, "messages")),
      el("div", { class: "mail-empty-actions" },
        el("button", { class: "btn sm", onclick: () => $("#mail-search")?.focus() }, icon("search", "icon-sm"), "Search archive"),
        el("button", { class: "btn sm", onclick: () => go("overview") }, icon("clock", "icon-sm"), "View sync log"))));
    return;
  }
  const p = it.preview || {}, from = parseAddr(p.from), cat = mailCat(it);
  const subject = p.subject || it.name || "(no subject)", when = p.date || it.remote_mtime;
  const q = { account: App.account, service: "mail", id: it.remote_id };
  const actions = el("div", { class: "mr-actions" },
    el("a", { class: "btn ghost sm", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener", title: "Open in new tab" }, icon("external-link", "icon-sm")));
  if (CAP.restore) actions.append(el("button", { class: "btn sm", title: "Restore to cloud", onclick: (e) => doRestore(it, e.currentTarget) }, icon("rotate-ccw", "icon-sm"), "Restore"));
  box.append(
    el("header", { class: "mail-reader-head" },
      el("button", { class: "mail-back btn ghost sm", title: "Back", onclick: mailBack }, icon("chevron-left", "icon-sm")),
      el("div", { class: "grow", style: "min-width:0" },
        el("div", { class: "mr-tags" }, el("span", { class: "mi-cat", style: `--c:${cat.color}`, text: cat.label }),
          p.attachments > 0 ? el("span", { class: "mi-chip" }, icon("paperclip", "icon-sm"), p.attachments + (p.attachments === 1 ? " attachment" : " attachments")) : null,
          el("span", { class: "mi-status " + (it.has_body ? "ok" : "muted") }, it.has_body ? "Body saved" : "Header only")),
        el("h2", { class: "mr-subject", text: subject }),
        el("div", { class: "mr-meta" },
          el("span", { class: "avatar mail-av", style: `--c:${cat.color}`, text: initials(from.name || from.email || subject) }),
          el("div", { class: "grow", style: "min-width:0" },
            el("div", { class: "mr-from truncate" }, el("b", { text: from.name || from.email || "(unknown sender)" }),
              from.name && from.email ? el("span", { class: "dim", text: " <" + from.email + ">" }) : null),
            (p.to && p.to.length) ? el("div", { class: "mr-to dim truncate", text: "To: " + p.to.join(", ") }) : null),
          el("span", { class: "mr-date dim tnum", text: fmtFullDate(when) }))),
      actions),
    el("iframe", { class: "mail-frame", src: `/api/v1/view?${qs(q)}`, title: "Message body", loading: "lazy" }));
}
function metricCard(icn, val, label) {
  return el("div", { class: "metric-card" }, el("span", { class: "mc-ico" }, icon(icn, "icon-sm")),
    el("div", {}, el("div", { class: "mc-val tnum", text: String(val) }), el("div", { class: "mc-lbl dim", text: label })));
}

/* ---------------------------------------------------------------- onedrive (file explorer) */
const Drive = { stack: [], layout: "grid", items: [] };
// extension → {icon, color} category for file glyphs
const FILE_KINDS = [
  { icon: "image", color: "#38bdf8", ext: ["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "heic", "tiff"] },
  { icon: "file-text", color: "#f87171", ext: ["pdf"] },
  { icon: "file-text", color: "#60a5fa", ext: ["doc", "docx", "odt", "rtf", "txt", "md", "one", "onetoc2"] },
  { icon: "table", color: "#34d399", ext: ["xls", "xlsx", "csv", "ods"] },
  { icon: "file-text", color: "#fb923c", ext: ["ppt", "pptx", "odp"] },
  { icon: "music", color: "#a855f7", ext: ["mp3", "wav", "flac", "m4a", "aac", "ogg"] },
  { icon: "film", color: "#f472b6", ext: ["mp4", "mov", "mkv", "avi", "webm"] },
  { icon: "archive", color: "#fbbf24", ext: ["zip", "rar", "7z", "tar", "gz", "tgz"] },
  { icon: "code", color: "#818cf8", ext: ["js", "ts", "rs", "py", "c", "cpp", "h", "java", "go", "json", "html", "css", "sh", "toml", "yaml", "yml", "xml"] },
];
const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico"]); // raster only (svg served inert)
const fileExt = (name) => { const m = /\.([a-z0-9]+)$/i.exec(name || ""); return m ? m[1].toLowerCase() : ""; };
const fileKind = (ext) => FILE_KINDS.find(k => k.ext.includes(ext));
const fileIcon = (ext) => (fileKind(ext) || {}).icon || "file";
const fileColor = (ext) => (fileKind(ext) || {}).color || "var(--text-lo)";

async function renderOnedriveView(view) {
  Drive.stack = []; Drive.layout = Drive.layout || "grid"; Drive.items = [];
  clear(view).append(
    viewHeader("OneDrive", `${App.counts.onedrive != null ? App.counts.onedrive + " items archived · " : ""}file & folder archive`, [readonlyChip()]),
    el("div", { class: "drive-bar" },
      el("div", { id: "drive-crumbs", class: "drive-crumbs" }),
      el("div", { class: "spacer", style: "flex:1" }),
      el("div", { class: "seg" },
        el("button", { id: "drive-grid", class: "seg-btn" + (Drive.layout === "grid" ? " active" : ""), title: "Grid view", onclick: () => setDriveLayout("grid") }, icon("layout-dashboard", "icon-sm")),
        el("button", { id: "drive-list", class: "seg-btn" + (Drive.layout === "list" ? " active" : ""), title: "List view", onclick: () => setDriveLayout("list") }, icon("list", "icon-sm")))),
    el("div", { id: "drive-body" }),
  );
  await driveOpen(null, "OneDrive", true);
}
function setDriveLayout(l) { Drive.layout = l; $("#drive-grid")?.classList.toggle("active", l === "grid"); $("#drive-list")?.classList.toggle("active", l === "list"); driveRender(); }
async function driveOpen(id, name, reset) {
  if (reset) Drive.stack = [{ id: "root", name: "OneDrive" }];
  else Drive.stack.push({ id, name });
  await driveLoad();
}
function driveCrumbTo(i) { Drive.stack = Drive.stack.slice(0, i + 1); driveLoad(); }
function renderCrumbs() {
  const c = $("#drive-crumbs"); if (!c) return; clear(c);
  Drive.stack.forEach((s, i) => {
    if (i) c.append(icon("chevron-right", "icon-sm"));
    c.append(el("button", { class: "crumb" + (i === Drive.stack.length - 1 ? " cur" : ""), onclick: () => driveCrumbTo(i) },
      i === 0 ? icon("hard-drive", "icon-sm") : null, el("span", { text: s.name })));
  });
}
async function driveLoad() {
  renderCrumbs();
  const body = $("#drive-body"); if (!body) return;
  clear(body);
  const sk = el("div", { class: "drive-grid" });
  for (let i = 0; i < 8; i++) sk.append(el("div", { class: "card drive-tile" }, el("div", { class: "skel", style: "height:84px" }), el("div", { class: "skel", style: "height:14px;width:70%" })));
  body.append(sk);
  const cur = Drive.stack[Drive.stack.length - 1].id;
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "onedrive", parent: cur }));
    Drive.items = d.items || [];
    driveRender();
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load folder" }), el("p", { text: e.message }))); }
}
function driveSort(items) {
  return items.slice().sort((a, b) =>
    (a.item_type === "folder" ? 0 : 1) - (b.item_type === "folder" ? 0 : 1) || (a.name || "").localeCompare(b.name || ""));
}
function driveRender() {
  const body = $("#drive-body"); if (!body) return; clear(body);
  if (!Drive.items.length) { body.append(el("div", { class: "empty" }, emptyArt("empty-files"), el("h3", { text: "Empty folder" }), el("p", { text: "Nothing is archived here." }))); return; }
  const items = driveSort(Drive.items);
  if (Drive.layout === "grid") {
    const grid = el("div", { class: "drive-grid stagger" });
    items.forEach(it => grid.append(driveTile(it)));
    body.append(grid);
  } else {
    const list = el("div", { class: "card", style: "padding:0;overflow:hidden" });
    items.forEach(it => list.append(driveRow(it)));
    body.append(list);
  }
}
function syncBadge(it) {
  if (!it.sync_state || it.sync_state === "clean") return null;
  const kind = it.sync_state === "deleted" ? "err" : "warn";
  return el("span", { class: "pill " + kind + " sync-badge", title: "Sync state: " + it.sync_state }, el("span", { class: "dot" }));
}
function driveActions(it) {
  if (it.item_type === "folder") return null;
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const box = el("div", { class: "drive-actions" });
  if (it.has_body) box.append(el("a", { class: "act", href: `/api/v1/body?${qs(q)}`, download: it.name || "", title: "Download", onclick: (e) => e.stopPropagation() }, icon("download", "icon-sm")));
  if (CAP.share) box.append(el("button", { class: "act", title: "Share", onclick: (e) => { e.stopPropagation(); doShare(it, e.currentTarget); } }, icon("share2", "icon-sm")));
  return box;
}
function driveTile(it) {
  const folder = it.item_type === "folder";
  const ext = fileExt(it.name);
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const tile = el("div", { class: "card drive-tile rise" + (folder ? " is-folder" : ""), onclick: () => folder ? driveOpen(it.remote_id, it.name) : window.open(`/api/v1/view?${qs(q)}`, "_blank", "noopener") });
  let thumb;
  if (!folder && it.has_body && IMAGE_EXT.has(ext))
    thumb = el("img", { class: "drive-thumb-img", src: `/api/v1/body?${qs(q)}`, alt: "", loading: "lazy" });
  else
    thumb = el("div", { class: "drive-thumb", style: folder ? "" : `color:${fileColor(ext)}` }, icon(folder ? "folder" : fileIcon(ext), "icon-lg"));
  tile.append(...[thumb,
    el("div", { class: "drive-name truncate", text: it.name || "(no name)" }),
    el("div", { class: "drive-meta dim", text: folder ? "Folder" : [fmtSize(it.size), it.remote_mtime ? fmtDate(it.remote_mtime) : ""].filter(Boolean).join(" · ") }),
    syncBadge(it), driveActions(it)].filter(Boolean)); // native append stringifies null → drop nulls
  return tile;
}
function driveRow(it) {
  const folder = it.item_type === "folder";
  const ext = fileExt(it.name);
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const row = el("div", { class: "list-row", onclick: () => folder ? driveOpen(it.remote_id, it.name) : window.open(`/api/v1/view?${qs(q)}`, "_blank", "noopener") },
    el("span", { class: "drive-row-ico", style: folder ? "color:var(--svc-onedrive)" : `color:${fileColor(ext)}` }, icon(folder ? "folder" : fileIcon(ext))),
    el("div", { class: "grow" },
      el("div", { class: "truncate", text: it.name || "(no name)" }),
      el("div", { class: "dim", style: "font-size:12px", text: folder ? "Folder" : (fmtSize(it.size) || "—") })),
    syncBadge(it),
    el("span", { class: "dim tnum", style: "font-size:12px", text: fmtDate(it.remote_mtime) }));
  const acts = el("div", { style: "display:flex;gap:4px" });
  if (!folder && it.has_body) acts.append(el("a", { class: "btn ghost sm", href: `/api/v1/body?${qs(q)}`, download: it.name || "", title: "Download", onclick: (e) => e.stopPropagation() }, icon("download", "icon-sm")));
  if (!folder && CAP.share) acts.append(el("button", { class: "btn ghost sm", title: "Share", onclick: (e) => { e.stopPropagation(); doShare(it, e.currentTarget); } }, icon("share2", "icon-sm")));
  row.append(acts);
  return row;
}

/* ---------------------------------------------------------------- calendar (month / week / agenda) */
const Cal = { events: [], view: "agenda", cursor: null };
const DAY_MS = 864e5, HOUR_PX = 44, DAY_NAMES = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
// Graph datetime ("2026-02-04T09:00:00.0000000" + tz) → JS Date
function evDate(dt, tz) {
  if (!dt) return null;
  let s = String(dt).replace(/(\.\d{3})\d*$/, "$1");            // trim fraction to ms (JS parses ≤3)
  if (tz === "UTC" && !/[zZ]$|[+\-]\d\d:?\d\d$/.test(s)) s += "Z";
  const d = new Date(s); return isNaN(d) ? null : d;
}
const ymd = (d) => d.getFullYear() + "-" + (d.getMonth() + 1) + "-" + d.getDate();
const startOfDay = (d) => { const x = new Date(d); x.setHours(0, 0, 0, 0); return x; };
function startOfWeek(d) { const x = startOfDay(d); const dow = (x.getDay() + 6) % 7; x.setDate(x.getDate() - dow); return x; } // Monday
const hhmm = (d) => d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

async function renderCalendarView(view) {
  Cal.events = []; Cal.cursor = new Date(); Cal.view = Cal.view || "agenda";
  clear(view).append(
    viewHeader("Calendar", `${App.counts.calendar != null ? App.counts.calendar + " events archived" : "event archive"}`, [readonlyChip()]),
    el("div", { class: "cal-bar" },
      el("div", { class: "cal-nav" },
        el("button", { class: "btn ghost sm icon-only", title: "Previous", onclick: () => calNav(-1) }, icon("chevron-left", "icon-sm")),
        el("button", { class: "btn ghost sm", onclick: () => { Cal.cursor = new Date(); calRender(); } }, "Today"),
        el("button", { class: "btn ghost sm icon-only", title: "Next", onclick: () => calNav(1) }, icon("chevron-right", "icon-sm"))),
      el("div", { id: "cal-label", class: "cal-label" }),
      el("div", { class: "spacer", style: "flex:1" }),
      el("div", { class: "seg" },
        ["month", "week", "agenda"].map(v => el("button", { class: "seg-btn" + (Cal.view === v ? " active" : ""), dataset: { calview: v }, onclick: () => setCalView(v), text: v[0].toUpperCase() + v.slice(1) }))),
    ),
    el("div", { id: "cal-body" }),
  );
  await calLoad();
}
function setCalView(v) { Cal.view = v; document.querySelectorAll("[data-calview]").forEach(b => b.classList.toggle("active", b.dataset.calview === v)); calRender(); }
function calNav(dir) {
  const c = Cal.cursor;
  if (Cal.view === "week") c.setDate(c.getDate() + dir * 7);
  else c.setMonth(c.getMonth() + dir);
  Cal.cursor = new Date(c); calRender();
}
async function calLoad() {
  const body = $("#cal-body"); clear(body).append(el("div", { class: "card" }, el("div", { class: "skel", style: "height:360px" })));
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "calendar", limit: 1000 }));
    Cal.events = (d.items || []).map(it => {
      const p = it.preview || {};
      const start = evDate(p.start, p.start_tz) || (it.remote_mtime ? new Date(it.remote_mtime) : null);
      return { it, subject: it.name || "(no title)", start, end: evDate(p.end, p.end_tz), allDay: !!p.all_day, location: p.location || "" };
    }).filter(e => e.start);
    App.counts.calendar = d.total ?? Cal.events.length; updateNavCounts();
    calRender();
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load calendar" }), el("p", { text: e.message }))); }
}
function eventsForDay(day) {
  const s = startOfDay(day).getTime(), e = s + DAY_MS;
  return Cal.events.filter(ev => ev.start.getTime() < e && (ev.end ? ev.end.getTime() : ev.start.getTime() + 36e5) > s)
    .sort((a, b) => a.start - b.start);
}
function calRender() {
  const body = $("#cal-body"); if (!body) return; clear(body);
  if (!Cal.events.length && Cal.view === "agenda") { body.append(el("div", { class: "empty" }, emptyArt("empty-calendar"), el("h3", { text: "No events archived" }), el("p", { text: "Run a backup to populate your calendar." }))); return; }
  if (Cal.view === "month") calRenderMonth(body);
  else if (Cal.view === "week") calRenderWeek(body);
  else calRenderAgenda(body);
}
function calRenderMonth(body) {
  const cur = Cal.cursor;
  $("#cal-label").textContent = MONTHS[cur.getMonth()] + " " + cur.getFullYear();
  const first = new Date(cur.getFullYear(), cur.getMonth(), 1);
  const gridStart = startOfWeek(first);
  const todayKey = ymd(new Date());
  const grid = el("div", { class: "cal-month" });
  DAY_NAMES.forEach(n => grid.append(el("div", { class: "cal-dow", text: n })));
  for (let i = 0; i < 42; i++) {
    const day = new Date(gridStart.getTime() + i * DAY_MS);
    const outside = day.getMonth() !== cur.getMonth();
    const cell = el("div", { class: "cal-cell" + (outside ? " outside" : "") + (ymd(day) === todayKey ? " today" : "") });
    cell.append(el("div", { class: "cal-daynum", text: String(day.getDate()) }));
    const evs = eventsForDay(day);
    evs.slice(0, 3).forEach(ev => cell.append(el("button", { class: "cal-chip", style: "--svc:var(--svc-calendar)", title: ev.subject, onclick: () => openEventSheet(ev) },
      ev.allDay ? null : el("span", { class: "cal-chip-time", text: hhmm(ev.start) }), el("span", { class: "truncate", text: ev.subject }))));
    if (evs.length > 3) cell.append(el("div", { class: "cal-more", text: "+" + (evs.length - 3) + " more" }));
    grid.append(cell);
  }
  body.append(grid);
}
function calRenderWeek(body) {
  const ws = startOfWeek(Cal.cursor), days = Array.from({ length: 7 }, (_, i) => new Date(ws.getTime() + i * DAY_MS));
  $("#cal-label").textContent = days[0].toLocaleDateString([], { month: "short", day: "numeric" }) + " – " + days[6].toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" });
  const todayKey = ymd(new Date());
  const wrap = el("div", { class: "cal-week card" });
  // header
  const head = el("div", { class: "cal-week-head" }, el("div", { class: "cal-gutter" }));
  days.forEach(d => head.append(el("div", { class: "cal-wday" + (ymd(d) === todayKey ? " today" : "") },
    el("span", { class: "dim", text: DAY_NAMES[(d.getDay() + 6) % 7] }), el("b", { text: String(d.getDate()) }))));
  wrap.append(head);
  // all-day strip
  const allday = el("div", { class: "cal-allday" }, el("div", { class: "cal-gutter dim", text: "all-day" }));
  days.forEach(d => {
    const cell = el("div", { class: "cal-allday-cell" });
    eventsForDay(d).filter(e => e.allDay).forEach(ev => cell.append(el("button", { class: "cal-chip", title: ev.subject, onclick: () => openEventSheet(ev) }, el("span", { class: "truncate", text: ev.subject }))));
    allday.append(cell);
  });
  wrap.append(allday);
  // time grid (00–24, scrollable)
  const grid = el("div", { class: "cal-grid", style: `--hour-px:${HOUR_PX}px` });
  const gutter = el("div", { class: "cal-gutter-col" });
  for (let h = 0; h < 24; h++) gutter.append(el("div", { class: "cal-hour", style: `height:${HOUR_PX}px` }, el("span", { text: (h < 10 ? "0" : "") + h + ":00" })));
  grid.append(gutter);
  days.forEach(d => {
    const col = el("div", { class: "cal-daycol" });
    for (let h = 0; h < 24; h++) col.append(el("div", { class: "cal-slot", style: `height:${HOUR_PX}px` }));
    eventsForDay(d).filter(e => !e.allDay).forEach(ev => {
      const dayStart = startOfDay(d).getTime();
      const top = Math.max(0, (ev.start.getTime() - dayStart) / 36e5) * HOUR_PX;
      const endT = ev.end ? ev.end.getTime() : ev.start.getTime() + 36e5;
      const h = Math.max(18, ((endT - ev.start.getTime()) / 36e5) * HOUR_PX - 2);
      col.append(el("button", { class: "cal-event", style: `top:${top}px;height:${h}px`, onclick: () => openEventSheet(ev) },
        el("div", { class: "cal-event-time", text: hhmm(ev.start) }), el("div", { class: "cal-event-title truncate", text: ev.subject }),
        ev.location ? el("div", { class: "cal-event-loc truncate", text: ev.location }) : null));
    });
    grid.append(col);
  });
  wrap.append(grid);
  body.append(wrap);
}
function calRenderAgenda(body) {
  const cur = Cal.cursor;
  $("#cal-label").textContent = MONTHS[cur.getMonth()] + " " + cur.getFullYear();
  const evs = Cal.events.slice().sort((a, b) => a.start - b.start);
  if (!evs.length) { body.append(el("div", { class: "empty" }, el("h3", { text: "No events" }))); return; }
  const box = el("div", { class: "cal-agenda" });
  let lastKey = null;
  evs.forEach(ev => {
    const key = ymd(ev.start);
    if (key !== lastKey) {
      lastKey = key;
      box.append(el("div", { class: "cal-agenda-day" },
        el("b", { text: ev.start.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long" }) }),
        ymd(ev.start) === ymd(new Date()) ? el("span", { class: "pill info", style: "margin-left:8px" }, "Today") : null));
    }
    box.append(el("button", { class: "cal-agenda-row", onclick: () => openEventSheet(ev) },
      el("span", { class: "cal-agenda-time tnum", text: ev.allDay ? "All day" : hhmm(ev.start) + (ev.end ? "–" + hhmm(ev.end) : "") }),
      el("span", { class: "cal-dot", style: "background:var(--svc-calendar)" }),
      el("div", { class: "grow" }, el("div", { class: "truncate", text: ev.subject }),
        ev.location ? el("div", { class: "dim truncate", style: "font-size:12px" }, icon("map-pin", "icon-sm"), ev.location) : null)));
  });
  body.append(box);
}
async function openEventSheet(ev) {
  const q = { account: App.account, service: "calendar", id: ev.it.remote_id };
  const content = el("div", { class: "body" }, el("div", { class: "spinner" }));
  const scrim = el("div", { class: "scrim", onclick: closeSheet });
  const sheet = el("aside", { class: "sheet" },
    el("header", {}, el("h2", { class: "grow truncate", text: ev.subject }),
      el("button", { class: "btn ghost sm icon-only", onclick: closeSheet }, icon("x", "icon-sm"))),
    content);
  sheetEl = el("div", {}, scrim, sheet); document.body.append(sheetEl);
  // structured detail rendered via textContent only (never innerHTML on cloud data)
  const kv = el("dl", { class: "kv" });
  const add = (k, v, ic) => { if (!v) return; kv.append(el("dt", {}, ic ? icon(ic, "icon-sm") : null, el("span", { text: k })), el("dd", { text: v })); };
  add("When", ev.allDay ? ev.start.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long", year: "numeric" }) + " · all day"
    : fmtFullDate(ev.start) + (ev.end ? " – " + hhmm(ev.end) : ""), "clock");
  add("Location", ev.location, "map-pin");
  try {
    const full = await api("/api/v1/body?" + qs(q));
    const org = ((full.organizer || {}).emailAddress || {});
    add("Organizer", org.name || org.address, "users");
    const att = (full.attendees || []).map(a => (a.emailAddress || {}).name || (a.emailAddress || {}).address).filter(Boolean);
    if (att.length) add("Attendees", att.join(", "), "users");
    // event description is HTML → extract plain text safely (DOMParser runs no scripts, loads nothing)
    const html = (full.body || {}).content || "";
    if (html) {
      const txt = new DOMParser().parseFromString(html, "text/html").body.textContent.trim();
      if (txt) { clear(content).append(kv, el("h3", { class: "sb-section", text: "Notes" }), el("p", { class: "muted", style: "white-space:pre-wrap", text: txt.slice(0, 4000) })); }
      else clear(content).append(kv);
    } else clear(content).append(kv);
  } catch { clear(content).append(kv); }
  content.append(el("a", { class: "btn ghost sm", style: "margin-top:16px", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener" }, icon("external-link", "icon-sm"), "Open full event"));
}
let sheetEl = null;
function closeSheet() { if (sheetEl) { sheetEl.remove(); sheetEl = null; } }

/* ---------------------------------------------------------------- contacts (avatar cards) */
const Contacts = { all: [], selected: null, filter: "all", q: "", sort: "name", lastSync: null, runs: [], retentionDays: null };
const conLetter = (it) => ((it.name || "#").trim()[0] || "#").toUpperCase();
const conPrev = (it) => it.preview || {};
const CON_FILTERS = [["all", "All"], ["email", "With email"], ["company", "With company"], ["restore", "Restore-ready"]];
async function renderContactsView(view) {
  Object.assign(Contacts, { all: [], selected: null, filter: "all", q: "", sort: "name", status: null });
  clear(view).append(el("div", { class: "con-page" },
    // header: title + live metrics + honest trust chips (Source / Encryption / Read-only)
    el("header", { class: "con-page-head" },
      el("div", { class: "grow", style: "min-width:0" },
        el("h1", { class: "view-title", style: "margin:0", text: "Contacts" }),
        el("div", { id: "con-metrics", class: "svc-metrics dim", text: "Loading…" })),
      el("div", { class: "svc-chips" },
        el("span", { class: "chip muted" }, icon("archive", "icon-sm"), "Microsoft 365"),
        el("span", { class: "chip muted" }, icon("shield", "icon-sm"), "Encrypted at rest"),
        readonlyChip())),
    // top metric row (real data: counts + integrity from verify + sync health/activity)
    el("div", { id: "con-metrics-row", class: "con-metrics-row" }),
    // toolbar: search + filters + sort + verify + sync-log
    el("div", { class: "con-toolbar" },
      el("div", { class: "tb-search" }, icon("search", "icon-sm"),
        el("input", { id: "con-search", placeholder: "Search by name, email, or company…", oninput: () => { Contacts.q = ($("#con-search").value || "").trim().toLowerCase(); contactsRenderList(); } })),
      el("div", { class: "con-filters" }, ...CON_FILTERS.map(([k, l]) =>
        el("button", { class: "con-filter" + (Contacts.filter === k ? " active" : ""), dataset: { k }, onclick: () => { Contacts.filter = k; document.querySelectorAll(".con-filter").forEach(b => b.classList.toggle("active", b.dataset.k === k)); contactsRenderList(); } }, l))),
      el("div", { class: "spacer", style: "flex:1" }),
      el("label", { class: "tb-sort" }, icon("arrow-down-up", "icon-sm"),
        el("select", { class: "input", onchange: (e) => { Contacts.sort = e.target.value; contactsRenderList(); } },
          el("option", { value: "name", text: "Name A–Z" }),
          el("option", { value: "company", text: "Company A–Z" }),
          el("option", { value: "recent", text: "Recently archived" }))),
      CAP.verify ? el("button", { class: "btn sm", title: "Re-hash every archived record and check integrity", onclick: (e) => contactsVerify(e.currentTarget) }, icon("shield-check", "icon-sm"), "Verify") : null,
      el("button", { class: "btn sm", title: "View sync log", onclick: () => go("overview") }, icon("clock", "icon-sm"), "Sync log")),
    // master–detail: directory list | record detail
    el("div", { id: "con-layout", class: "con-layout" },
      el("div", { class: "con-listwrap" }, el("div", { id: "con-list", class: "con-list" }), el("div", { id: "con-az", class: "con-az" })),
      el("div", { id: "con-detail", class: "con-detail" }))));
  renderContactDetail(null);
  const list = $("#con-list");
  for (let i = 0; i < 9; i++) list.append(el("div", { class: "con-row skel-row" }, el("div", { class: "skel grow", style: "height:38px" })));
  try {
    const [d, act, settings, status] = await Promise.all([
      api("/api/v1/items?" + qs({ account: App.account, service: "contacts", limit: 1000 })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 30 })).catch(() => ({ runs: [] })),
      api("/api/v1/settings").catch(() => ({})),
      api("/api/v1/status?" + qs({ account: App.account })).catch(() => ({})),
    ]);
    Contacts.all = (d.items || []).filter(it => it.item_type !== "folder");
    Contacts.runs = act.runs || [];
    Contacts.status = status;
    Contacts.lastSync = Contacts.runs.filter(r => /sync|backup/i.test(r.kind || "")).map(r => r.finished_at || r.started_at).filter(Boolean).sort().pop() || null;
    Contacts.retentionDays = (settings.sync || {}).trash_retention_days ?? null;
    App.counts.contacts = d.total ?? Contacts.all.length; updateNavCounts();
    contactsRenderMetrics(); contactsRenderList();
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Could not load contacts" }), el("p", { text: e.message }))); }
}
// re-hash every archived record (real verify), then refresh the integrity signals
async function contactsVerify(btn) {
  btn.disabled = true;
  try {
    const r = await post("/api/v1/verify?" + qs({ account: App.account }), CAP.verify);
    toast(`Integrity: ${r.verified}/${r.checked} records verified`);
    const [status, d] = await Promise.all([
      api("/api/v1/status?" + qs({ account: App.account })).catch(() => Contacts.status),
      api("/api/v1/items?" + qs({ account: App.account, service: "contacts", limit: 1000 })),
    ]);
    Contacts.status = status;
    Contacts.all = (d.items || []).filter(it => it.item_type !== "folder");
    contactsRenderMetrics(); contactsRenderList();
    if (Contacts.selected) { const s = Contacts.all.find(x => x.remote_id === Contacts.selected.remote_id); if (s) { Contacts.selected = s; renderContactDetail(s); } }
  } catch (e) { toast("Verify failed: " + e.message, "err"); } finally { btn.disabled = false; }
}
function contactsFiltered() {
  let rows = Contacts.all;
  const f = Contacts.filter;
  if (f === "email") rows = rows.filter(it => conPrev(it).email);
  else if (f === "company") rows = rows.filter(it => conPrev(it).company);
  else if (f === "restore") rows = rows.filter(it => it.has_body);
  if (Contacts.q) rows = rows.filter(it => ((it.name || "") + " " + (conPrev(it).company || "") + " " + (conPrev(it).email || "") + " " + (conPrev(it).job || "")).toLowerCase().includes(Contacts.q));
  const s = Contacts.sort;
  const ts = (it) => { const d = toDate(it.remote_mtime); return d ? d.getTime() : 0; };
  if (s === "company") rows = rows.slice().sort((a, b) => (conPrev(a).company || "￿").localeCompare(conPrev(b).company || "￿") || (a.name || "").localeCompare(b.name || ""));
  else if (s === "recent") rows = rows.slice().sort((a, b) => ts(b) - ts(a));
  else rows = rows.slice().sort((a, b) => (a.name || "").localeCompare(b.name || ""));
  return rows;
}
function conMetric(icn, value, label, sub, tone) {
  return el("div", { class: "card rise con-metric" },
    el("span", { class: "cm-ico" + (tone ? " " + tone : "") }, icon(icn, "icon-lg")),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "cm-label dim", text: label }),
      el("div", { class: "cm-val tnum" + (tone ? " " + tone : ""), text: String(value) }),
      el("div", { class: "cm-sub dim truncate", text: sub })));
}
// top metric row — every value real: counts from items, integrity from /status
// verify, sync health + last activity from /activity runs.
function contactsRenderMetrics() {
  const row = $("#con-metrics-row"); if (!row) return; clear(row);
  const v = (Contacts.status || {}).verify || {};
  const restore = Contacts.all.filter(it => it.has_body).length;
  const total = Contacts.all.length;
  const pct = v.checked ? Math.round((v.verified / v.checked) * 100) : null;
  const runs = Contacts.runs || [];
  // sync health = sync/backup runs only (a verify finding drift is not "sync unhealthy")
  const failed = runs.filter(r => /sync|backup/i.test(r.kind || "") && /error|fail/i.test(r.status || "")).length;
  const lastRun = runs[0];
  row.append(
    conMetric("users", total, "Total contacts", "Across all directories"),
    conMetric("rotate-ccw", restore, "Restore-ready", restore === total ? "100% of archive" : `${restore} of ${total} archived`, "ok"),
    conMetric("shield-check", pct == null ? "—" : pct + "%", "Integrity verified",
      pct == null ? "Run verify to check" : `${v.verified} of ${v.checked} records`,
      pct == null ? "" : pct === 100 ? "ok" : "warn"),
    conMetric("refresh-cw", failed ? "Issues" : "Healthy", "Sync health", failed ? `${failed} failed run(s)` : "All systems operational", failed ? "warn" : "ok"),
    conMetric("clock", lastRun ? fmtDate(lastRun.finished_at) : "—", "Last archive activity", lastRun ? `${lastRun.kind} · ${fmtFullDate(lastRun.finished_at)}` : "no runs recorded yet"));
}
function contactsRenderList() {
  const wrap = clear($("#con-list")), az = clear($("#con-az"));
  const withEmail = Contacts.all.filter(it => conPrev(it).email).length;
  const withCompany = Contacts.all.filter(it => conPrev(it).company).length;
  const m = $("#con-metrics"); if (m) m.textContent = `${Contacts.all.length} contacts archived · ${withEmail} with email · ${withCompany} with company`;
  if (!Contacts.all.length) { wrap.append(el("div", { class: "empty" }, emptyArt("empty-contacts"), el("h3", { text: "No contacts" }), el("p", { text: "Run a backup to populate your contacts." }))); return; }
  const rows = contactsFiltered();
  if (!rows.length) { wrap.append(el("div", { class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No matches" }), el("p", { text: "Adjust the filter or search." }))); return; }
  const frag = document.createDocumentFragment();
  const grouped = Contacts.sort === "name";        // letter sections only make sense alphabetically
  let cur = "";
  rows.forEach(it => {
    if (grouped) { const letter = conLetter(it); if (letter !== cur) { cur = letter; frag.append(el("div", { class: "con-sec", dataset: { letter }, text: letter })); } }
    frag.append(contactRow(it));
  });
  wrap.append(frag);
  if (grouped) [...new Set(rows.map(conLetter))].sort().forEach(L =>
    az.append(el("button", { class: "az-letter", text: L, onclick: () => { const t = wrap.querySelector(`.con-sec[data-letter="${L}"]`); t && t.scrollIntoView({ block: "start", behavior: "smooth" }); } })));
}
function contactRow(it) {
  const p = conPrev(it);
  const sub = [p.job, p.company].filter(Boolean).join(" · ") || p.email || "";
  const sel = Contacts.selected && Contacts.selected.remote_id === it.remote_id;
  return el("button", { class: "con-row" + (sel ? " active" : ""), dataset: { id: it.remote_id }, onclick: () => contactSelect(it) },
    el("span", { class: "avatar con-av", text: initials(it.name) }),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "con-name truncate", text: it.name || "(no name)" }),
      sub ? el("div", { class: "con-sub truncate", text: sub }) : null),
    el("span", { class: "con-row-meta" },
      p.email ? el("span", { class: "con-dot", title: "Has email", style: "background:var(--svc-contacts)" }) : null,
      it.has_body ? el("span", { class: "con-restore", title: "Restore-ready" }, icon("rotate-ccw", "icon-sm")) : null));
}
function contactSelect(it) {
  Contacts.selected = it;
  document.querySelectorAll(".con-row").forEach(r => r.classList.toggle("active", r.dataset.id === it.remote_id));
  $("#con-layout")?.classList.add("reading");
  renderContactDetail(it);
}
function contactBack() {
  Contacts.selected = null; $("#con-layout")?.classList.remove("reading");
  document.querySelectorAll(".con-row.active").forEach(r => r.classList.remove("active"));
  renderContactDetail(null);
}
// fields that count toward record completeness (each backed by real archived data)
const CONTACT_FIELDS = [
  ["Name", (c) => !!(c.displayName || c.givenName || c.surname)],
  ["Email", (c) => (c.emailAddresses || []).some(e => e.address)],
  ["Mobile", (c) => !!c.mobilePhone],
  ["Business phone", (c) => (c.businessPhones || []).length > 0],
  ["Company", (c) => !!c.companyName],
  ["Department", (c) => !!c.department],
  ["Job title", (c) => !!c.jobTitle],
  ["Address", (c) => { const a = c.businessAddress || c.homeAddress || {}; return !!(a.street || a.city || a.postalCode); }],
  ["Notes", (c) => !!c.personalNotes],
];
const shortEtag = (e) => { const m = String(e).replace(/[^A-Za-z0-9]/g, ""); return m.length > 10 ? "…" + m.slice(-8) : (m || "—"); };
// completeness ring (real: populated archived fields / known fields)
function completenessRing(filled, total) {
  const pct = total ? Math.round((filled / total) * 100) : 0;
  const R = 34, C = 2 * Math.PI * R, len = (pct / 100) * C;
  const color = pct >= 70 ? "var(--ok)" : pct >= 40 ? "var(--warn)" : "var(--text-lo)";
  const s = svg("svg", { viewBox: "0 0 84 84", style: "width:84px;height:84px;flex:none" });
  s.append(svg("circle", { cx: 42, cy: 42, r: R, fill: "none", stroke: "var(--bg-3)", "stroke-width": 8 }));
  s.append(svg("circle", { cx: 42, cy: 42, r: R, fill: "none", stroke: color, "stroke-width": 8, "stroke-linecap": "round",
    "stroke-dasharray": `${len.toFixed(2)} ${(C - len).toFixed(2)}`, transform: "rotate(-90 42 42)" }));
  return el("div", { style: "position:relative;display:grid;place-items:center;flex:none" }, s,
    el("div", { class: "con-ring-lbl tnum", text: pct + "%" }));
}
function completenessCard(c) {
  const present = CONTACT_FIELDS.filter(([, f]) => f(c));
  const list = el("ul", { class: "con-fields" });
  CONTACT_FIELDS.forEach(([label, f]) => { const on = f(c); list.append(el("li", { class: on ? "on" : "off" }, icon(on ? "check" : "circle", "icon-sm"), el("span", { text: label }))); });
  return el("div", { class: "card con-block" },
    el("div", { class: "con-block-head" }, icon("check-square", "icon-sm"), el("span", { text: "Record completeness" })),
    el("div", { class: "con-complete" },
      completenessRing(present.length, CONTACT_FIELDS.length),
      el("div", { class: "grow" },
        el("div", { class: "con-complete-lead" }, el("b", { class: "tnum", text: `${present.length} of ${CONTACT_FIELDS.length}` }), el("span", { class: "dim", text: " fields archived" })),
        list)));
}
// honest archive-metadata card: every signal is backed by a real feature/datum
function archiveMetaCard(it) {
  const restoreOk = !!(CAP.restore && it.has_body);
  const meta = el("dl", { class: "kv meta-kv" });
  const row = (k, v, ic, cls) => meta.append(el("dt", {}, icon(ic, "icon-sm"), el("span", { text: k })), el("dd", { class: cls || "", text: v }));
  row("Source", "Microsoft 365", "archive");
  row("Access", "Read-only", "shield-check", "ok");
  row("Storage", "Encrypted at rest (SQLCipher)", "shield");
  row("Content", it.has_body ? "Full record archived" : "Header only", it.has_body ? "check" : "circle", it.has_body ? "ok" : "");
  if (it.etag) row("Version", shortEtag(it.etag), "file-text");
  if (it.size) row("Size", fmtSize(it.size), "archive");
  if (Contacts.retentionDays != null) row("Retention", Contacts.retentionDays + " days (trash)", "clock");
  row("Restore", restoreOk ? "Available" : "Unavailable", "rotate-ccw", restoreOk ? "ok" : "");
  row("Archived", fmtFullDate(it.remote_mtime), "clock");
  return el("div", { class: "card con-block" },
    el("div", { class: "con-block-head" }, icon("archive", "icon-sm"), el("span", { text: "Archive details" })), meta);
}
// compliance & integrity card — driven entirely by the real verify pass
function complianceCard(it) {
  const st = it.verify_status;
  const ok = st === "verified";
  const last = ((Contacts.status || {}).verify || {}).last_verified;
  const tone = ok ? "ok" : st === "changed" ? "warn" : st === "failed" ? "err" : "";
  const head = ok ? "All integrity checks passed"
    : st === "changed" ? "Content changed since last check"
      : st === "failed" ? "Integrity check failed"
        : "Not verified yet";
  const note = ok ? "SHA-256 of the archived body matches the recorded baseline."
    : st === "changed" ? "The archived body differs from the last recorded hash."
      : st === "failed" ? "The archived body could not be read for hashing."
        : "Run Verify to hash this record and check its integrity.";
  const meta = el("dl", { class: "kv meta-kv" });
  const row = (k, v, ic, cls) => meta.append(el("dt", {}, icon(ic, "icon-sm"), el("span", { text: k })), el("dd", { class: cls || "", text: v }));
  row("Status", st || "unverified", ok ? "check" : "circle", tone);
  row("Method", "SHA-256 re-hash", "shield");
  row("Last verified", last ? fmtFullDate(last) : "—", "clock");
  return el("div", { class: "card con-block" },
    el("div", { class: "con-block-head" }, icon("shield-check", "icon-sm"), el("span", { text: "Compliance & integrity" })),
    el("div", { class: "con-compliance" + (tone ? " " + tone : "") },
      el("span", { class: "cc-ico" }, icon(ok ? "shield-check" : "shield", "icon-lg")),
      el("div", { class: "grow", style: "min-width:0" },
        el("div", { class: "cc-head", text: head }),
        el("div", { class: "dim", style: "font-size:12px", text: note }))),
    meta);
}
async function renderContactDetail(it) {
  const box = $("#con-detail"); if (!box) return; clear(box);
  if (!it) {
    const withEmail = Contacts.all.filter(x => conPrev(x).email).length;
    const withCompany = Contacts.all.filter(x => conPrev(x).company).length;
    const fact = (icn, label) => el("div", { class: "crc-row" }, icon(icn, "icon-sm"), el("span", { text: label }));
    const runs = Contacts.runs || [];
    box.append(el("div", { class: "con-empty" },
      el("div", { class: "vault-art", html: EMPTY_ART["empty-contacts"] }),
      el("h3", { text: "Select a contact" }),
      el("p", { class: "dim", text: "Read-only Microsoft 365 contact archive. Choose a person to view their archived record, completeness and details." }),
      el("div", { class: "mail-empty-metrics" },
        metricCard("users", Contacts.all.length, "contacts"),
        metricCard("mail", withEmail, "with email"),
        metricCard("building", withCompany, "with company")),
      el("div", { class: "card con-block con-empty-insights" },
        el("div", { class: "con-block-head" }, icon("shield", "icon-sm"), el("span", { text: "Archive insights" })),
        el("div", { class: "crc-list" },
          fact("archive", "Source · Microsoft 365"),
          fact("shield", "Encrypted at rest (SQLCipher)"),
          fact("shield-check", "Read-only — the archive never writes back"),
          fact("rotate-ccw", "Restore re-creates a copy in your account"),
          Contacts.retentionDays != null ? fact("clock", `Trash retention · ${Contacts.retentionDays} days`) : null,
          Contacts.lastSync ? fact("clock", "Last synced " + fmtFullDate(Contacts.lastSync)) : fact("clock", "No sync recorded yet"))),
      runs.length ? el("div", { class: "card con-block con-empty-insights" },
        el("div", { class: "con-block-head" }, icon("clock", "icon-sm"), el("span", { text: "Archive activity" }),
          el("span", { class: "spacer", style: "flex:1" }), el("span", { class: "dim", style: "font-size:11px", text: "account-wide" })),
        el("div", { class: "con-block-body" }, activityChart(runs, 14))) : null,
      el("div", { class: "con-empty-actions" },
        el("button", { class: "btn sm", onclick: () => $("#con-search")?.focus() }, icon("search", "icon-sm"), "Search directory"),
        el("button", { class: "btn sm", onclick: () => go("overview") }, icon("clock", "icon-sm"), "View sync log"))));
    return;
  }
  const p = conPrev(it);
  const sub = [p.job, p.company].filter(Boolean).join(" · ");
  const actions = el("div", { class: "con-detail-actions" },
    el("a", { class: "btn ghost sm", href: `/api/v1/body?${qs({ account: App.account, service: "contacts", id: it.remote_id })}`, target: "_blank", rel: "noopener", title: "View raw archived record" }, icon("external-link", "icon-sm"), "Raw"));
  if (CAP.restore && it.has_body) actions.append(el("button", { class: "btn sm", title: "Restore to cloud as a new copy", onclick: (e) => doRestore(it, e.currentTarget) }, icon("rotate-ccw", "icon-sm"), "Restore"));
  const verified = it.verify_status === "verified";
  box.append(el("header", { class: "con-detail-head" },
    el("button", { class: "con-back btn ghost sm", title: "Back", onclick: contactBack }, icon("chevron-left", "icon-sm")),
    el("span", { class: "avatar con-av lg", text: initials(it.name) }),
    el("div", { class: "grow", style: "min-width:0" },
      el("h2", { class: "con-detail-name truncate", text: it.name || "(no name)" }),
      p.email ? el("button", { class: "con-detail-email truncate", title: "Copy email", onclick: (e) => { navigator.clipboard?.writeText(p.email).then(() => toast("Email copied")).catch(() => {}); } }, el("span", { class: "truncate", text: p.email }), icon("share2", "icon-sm")) : (sub ? el("div", { class: "con-detail-sub truncate", text: sub }) : null),
      el("div", { class: "con-detail-chips" }, readonlyChip(),
        (CAP.restore && it.has_body) ? el("span", { class: "chip ok" }, icon("rotate-ccw", "icon-sm"), "Restore-ready")
          : it.has_body ? el("span", { class: "chip muted" }, icon("check", "icon-sm"), "Body archived")
            : el("span", { class: "chip muted" }, "Header only"),
        verified ? el("span", { class: "chip ok" }, icon("shield-check", "icon-sm"), "Verified")
          : it.verify_status === "changed" ? el("span", { class: "chip warn" }, icon("shield", "icon-sm"), "Changed")
            : it.verify_status === "failed" ? el("span", { class: "chip err" }, icon("shield", "icon-sm"), "Check failed") : null,
        el("span", { class: "chip muted" }, icon("archive", "icon-sm"), "Microsoft 365"))),
    actions));
  const body = el("div", { class: "con-detail-body" });
  const fieldsBody = el("div", { class: "con-block-body" }, el("div", { class: "spinner" }));
  const fields = el("div", { class: "card con-block" },
    el("div", { class: "con-block-head" }, icon("users", "icon-sm"), el("span", { text: "Contact details" })), fieldsBody);
  const main = el("div", { class: "con-col-main" }, fields);
  const side = el("div", { class: "con-col-side" }, archiveMetaCard(it), complianceCard(it));
  body.append(main, side);
  box.append(body);
  try {
    const c = await api("/api/v1/body?" + qs({ account: App.account, service: "contacts", id: it.remote_id }));
    const kv = el("dl", { class: "kv" });
    const add = (k, v, ic) => { if (!v || (Array.isArray(v) && !v.length)) return; kv.append(el("dt", {}, ic ? icon(ic, "icon-sm") : null, el("span", { text: k })), el("dd", { text: Array.isArray(v) ? v.join(", ") : v })); };
    add("Email", (c.emailAddresses || []).map(e => e.address).filter(Boolean), "mail");
    add("Mobile", c.mobilePhone, "phone");
    add("Business", c.businessPhones, "phone");
    add("Home", c.homePhones, "phone");
    add("Company", [c.companyName, c.department].filter(Boolean).join(" — "), "building");
    add("Title", c.jobTitle, "users");
    const addr = c.businessAddress || c.homeAddress || {};
    add("Address", [addr.street, addr.city, addr.postalCode, addr.countryOrRegion].filter(Boolean).join(", "), "map-pin");
    clear(fieldsBody);
    if (kv.childElementCount) fieldsBody.append(kv);
    else fieldsBody.append(el("p", { class: "dim", style: "padding:2px", text: "No additional fields archived for this contact." }));
    // record-completeness card at the top of the side column (needs the loaded body)
    side.insertBefore(completenessCard(c), side.firstChild);
    if (c.personalNotes) main.append(el("div", { class: "card con-block" },
      el("div", { class: "con-block-head" }, icon("file-text", "icon-sm"), el("span", { text: "Notes" })),
      el("p", { class: "con-notes", style: "white-space:pre-wrap", text: c.personalNotes })));
  } catch (e) { clear(fieldsBody).append(el("p", { class: "dim", text: "Could not load contact: " + e.message })); }
}

/* ---------------------------------------------------------------- todo (lists + checklists) */
const Todo = { lists: [], tasks: [] };
const TODO_STATUS = { notStarted: { icon: "circle", cls: "" }, inProgress: { icon: "clock", cls: "prog" }, completed: { icon: "check-square", cls: "done" } };
async function renderTodoView(view) {
  clear(view).append(viewHeader("To Do", `${App.counts.todo != null ? App.counts.todo + " items archived" : "task archive"}`, [readonlyChip()]));
  const board = el("div", { id: "todo-board", class: "todo-board" });
  view.append(board);
  board.append(el("div", { class: "card", style: "min-width:280px" }, el("div", { class: "skel", style: "height:200px" })));
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "todo", limit: 1000 }));
    const items = d.items || [];
    Todo.lists = items.filter(it => it.item_type === "list");
    Todo.tasks = items.filter(it => it.item_type === "task");
    App.counts.todo = d.total ?? items.length; updateNavCounts();
    todoRender();
  } catch (e) { clear(board).append(el("div", { class: "empty" }, el("h3", { text: "Could not load ToDo" }), el("p", { text: e.message }))); }
}
function todoRender() {
  const board = clear($("#todo-board"));
  if (!Todo.lists.length && !Todo.tasks.length) { board.append(el("div", { class: "empty" }, emptyArt("empty-tasks"), el("h3", { text: "No tasks" }), el("p", { text: "Run a backup to populate your task lists." }))); return; }
  // group tasks by their parent list; tasks whose list is unknown go to "Tasks"
  const byList = new Map(Todo.lists.map(l => [l.remote_id, []]));
  const orphan = [];
  Todo.tasks.forEach(t => (byList.has(t.parent_remote_id) ? byList.get(t.parent_remote_id) : orphan).push(t));
  const order = ["notStarted", "inProgress", "completed"];
  const rank = (t) => order.indexOf((t.preview || {}).status || "notStarted");
  const column = (title, tasks) => {
    const sorted = tasks.slice().sort((a, b) => rank(a) - rank(b) || (a.name || "").localeCompare(b.name || ""));
    const col = el("div", { class: "todo-col card" }, el("div", { class: "todo-col-head" }, el("b", { text: title }), el("span", { class: "count tnum", text: String(tasks.length) })));
    if (!sorted.length) col.append(el("div", { class: "dim", style: "padding:8px", text: "No tasks" }));
    sorted.forEach(t => col.append(taskRow(t)));
    return col;
  };
  Todo.lists.forEach(l => board.append(column(l.name || "List", byList.get(l.remote_id) || [])));
  if (orphan.length) board.append(column("Tasks", orphan));
}
function taskRow(t) {
  const p = t.preview || {};
  const st = TODO_STATUS[p.status] || TODO_STATUS.notStarted;
  return el("button", { class: "todo-task" + (p.status === "completed" ? " done" : ""), onclick: () => openTaskSheet(t) },
    el("span", { class: "todo-check " + st.cls }, icon(st.icon, "icon-sm")),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "todo-title truncate", text: t.name || "(untitled)" }),
      (p.due || p.importance === "high") ? el("div", { class: "todo-meta dim" },
        p.importance === "high" ? el("span", { class: "todo-flag", title: "High importance" }, icon("flag", "icon-sm")) : null,
        p.due ? el("span", { text: "Due " + fmtDate(evDate(p.due, "UTC")) }) : null) : null));
}
async function openTaskSheet(t) {
  const q = { account: App.account, service: "todo", id: t.remote_id };
  const p = t.preview || {};
  const content = el("div", { class: "body" }, el("div", { class: "spinner" }));
  openSheet(t.name || "Task", content);
  try {
    const full = await api("/api/v1/body?" + qs(q));
    const kv = el("dl", { class: "kv" });
    const add = (k, v, ic) => { if (!v) return; kv.append(el("dt", {}, ic ? icon(ic, "icon-sm") : null, el("span", { text: k })), el("dd", { text: v })); };
    add("Status", (full.status || "").replace(/([A-Z])/g, " $1").replace(/^./, c => c.toUpperCase()), "check-square");
    add("Importance", full.importance, "flag");
    if (full.dueDateTime) add("Due", fmtFullDate(evDate(full.dueDateTime.dateTime, full.dueDateTime.timeZone)), "clock");
    if (full.completedDateTime) add("Completed", fmtFullDate(evDate(full.completedDateTime.dateTime, full.completedDateTime.timeZone)), "check");
    clear(content).append(kv);
    const note = (full.body || {}).content || "";
    if (note.trim()) {
      const txt = (full.body.contentType === "html") ? new DOMParser().parseFromString(note, "text/html").body.textContent : note;
      content.append(el("h3", { class: "sb-section", text: "Notes" }), el("p", { class: "muted", style: "white-space:pre-wrap", text: txt.trim().slice(0, 4000) }));
    }
  } catch (e) { clear(content).append(el("p", { class: "dim", text: "Could not load task: " + e.message })); }
}

/* ---------------------------------------------------------------- onenote (page list + reader) */
async function renderOnenoteView(view) {
  clear(view);
  const list = el("div", { id: "note-list", class: "note-list" });
  const reader = el("div", { id: "note-reader", class: "note-reader" });
  view.append(el("div", { class: "note-page" },
    el("header", { class: "note-page-head" },
      el("div", { class: "grow", style: "min-width:0" }, el("h1", { class: "view-title", style: "margin:0", text: "OneNote" }),
        el("div", { class: "svc-metrics dim", text: `${App.counts.onenote != null ? App.counts.onenote + " pages archived" : "notebook archive"}` })),
      el("div", { class: "svc-chips" }, readonlyChip())),
    el("div", { class: "note-layout" }, list, reader)));
  renderNoteReader(null);
  for (let i = 0; i < 5; i++) list.append(el("div", { class: "note-item" }, el("div", { class: "skel grow", style: "height:30px" })));
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "onenote", limit: 1000 }));
    const pages = (d.items || []).filter(it => it.item_type === "page");
    App.counts.onenote = d.total ?? pages.length; updateNavCounts();
    clear(list);
    if (!pages.length) { list.append(el("div", { class: "empty" }, emptyArt("empty-notes"), el("h3", { text: "No notes" }), el("p", { text: "Run a backup to populate OneNote." }))); return; }
    pages.forEach((it, i) => {
      const row = el("button", { class: "note-item", dataset: { id: it.remote_id }, onclick: () => noteSelect(it) },
        icon("notebook"), el("div", { class: "grow", style: "min-width:0" },
          el("div", { class: "truncate", text: it.name || "(untitled)" }),
          el("div", { class: "dim", style: "font-size:12px", text: fmtDate(it.remote_mtime) })));
      list.append(row);
      if (i === 0) setTimeout(() => noteSelect(it), 0);
    });
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Could not load OneNote" }), el("p", { text: e.message }))); }
}
function noteSelect(it) {
  document.querySelectorAll(".note-item").forEach(r => r.classList.toggle("active", r.dataset.id === it.remote_id));
  renderNoteReader(it);
}
function renderNoteReader(it) {
  const box = $("#note-reader"); if (!box) return; clear(box);
  if (!it) { box.append(el("div", { class: "empty", style: "margin:auto" }, logoGlyph(64), el("h3", { text: "Select a page" }))); return; }
  const q = { account: App.account, service: "onenote", id: it.remote_id };
  box.append(
    el("header", { class: "note-reader-head" }, el("h2", { class: "grow truncate", text: it.name || "(untitled)" }),
      el("a", { class: "btn ghost sm", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener", title: "Open in new tab" }, icon("external-link", "icon-sm"))),
    el("iframe", { class: "note-frame", src: `/api/v1/view?${qs(q)}`, title: "Note", loading: "lazy" }));
}

/* shared detail sheet (used by calendar/contacts/todo) */
function openSheet(title, contentEl, leading) {
  closeSheet();
  const scrim = el("div", { class: "scrim", onclick: closeSheet });
  const sheet = el("aside", { class: "sheet" },
    el("header", {}, leading || null, el("h2", { class: "grow truncate", text: title }),
      el("button", { class: "btn ghost sm icon-only", onclick: closeSheet }, icon("x", "icon-sm"))),
    contentEl);
  sheetEl = el("div", {}, scrim, sheet); document.body.append(sheetEl);
}

/* ---------------------------------------------------------------- empty-state art (curated in-code line-art SVG) */
// Hand-authored, cohesive line-art illustrations: stroke=currentColor (tinted by
// .empty-art) over one soft accent-gradient blob each (unique gradient id). No
// script/remote refs — embedded as trusted in-code SVG (like logoGlyph).
const EMPTY_ART = {
  "empty-mail": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-m" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="84" rx="72" ry="46" fill="url(#ea-m)" opacity="0.14"/><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><rect x="60" y="54" width="100" height="64" rx="9"/><path d="M60 62l50 36 50-36"/></g></svg>',
  "empty-files": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-f" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="86" rx="72" ry="44" fill="url(#ea-f)" opacity="0.14"/><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M58 58h30l9 11h65v44a6 6 0 0 1-6 6H64a6 6 0 0 1-6-6z"/><path d="M58 80h104"/></g></svg>',
  "empty-calendar": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-c" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="86" rx="70" ry="46" fill="url(#ea-c)" opacity="0.14"/><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><rect x="64" y="52" width="92" height="72" rx="9"/><path d="M64 72h92M86 46v12M134 46v12"/></g><g fill="currentColor" opacity="0.5"><circle cx="86" cy="90" r="3"/><circle cx="110" cy="90" r="3"/><circle cx="134" cy="90" r="3"/><circle cx="86" cy="108" r="3"/><circle cx="110" cy="108" r="3"/></g></svg>',
  "empty-contacts": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-u" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="86" rx="72" ry="44" fill="url(#ea-u)" opacity="0.14"/><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><rect x="58" y="54" width="104" height="64" rx="10"/><circle cx="88" cy="82" r="11"/><path d="M72 106a16 16 0 0 1 32 0"/><path d="M120 78h28M120 92h20"/></g></svg>',
  "empty-tasks": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-t" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="86" rx="66" ry="46" fill="url(#ea-t)" opacity="0.14"/><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><rect x="72" y="50" width="76" height="74" rx="8"/><rect x="94" y="44" width="32" height="14" rx="4"/><path d="M84 80l7 7 14-15"/><path d="M114 82h22M84 104h44"/></g></svg>',
  "empty-notes": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-n" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="86" rx="64" ry="46" fill="url(#ea-n)" opacity="0.14"/><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><rect x="74" y="48" width="76" height="76" rx="6"/><path d="M90 48v76"/><path d="M100 70h40M100 86h40M100 102h26"/></g></svg>',
  "empty-search": '<svg viewBox="0 0 220 160" xmlns="http://www.w3.org/2000/svg"><defs><linearGradient id="ea-s" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#a855f7"/></linearGradient></defs><ellipse cx="110" cy="84" rx="68" ry="46" fill="url(#ea-s)" opacity="0.14"/><g fill="currentColor" opacity="0.4"><circle cx="78" cy="58" r="2.5"/><circle cx="142" cy="64" r="2.5"/><circle cx="150" cy="104" r="2.5"/><circle cx="72" cy="106" r="2.5"/></g><g fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="102" cy="78" r="26"/><path d="M121 97l18 18"/></g></svg>',
};
function emptyArt(name, fallbackIcon) {
  if (EMPTY_ART[name]) return el("div", { class: "empty-art", html: EMPTY_ART[name] }); // trusted in-code SVG only
  return icon(fallbackIcon || "search", "icon-lg");
}

/* ---------------------------------------------------------------- search (global results) */
async function renderSearchView(view) {
  const q0 = App.query ? decodeURIComponent(App.query) : "";
  clear(view).append(
    el("h1", { class: "view-title", text: "Search" }),
    el("div", { class: "view-sub" }, el("input", {
      id: "search-input", class: "input", style: "max-width:560px", placeholder: "Search across all services…", value: q0,
      onkeydown: (e) => { if (e.key === "Enter") go("search?q=" + encodeURIComponent(e.target.value.trim())); },
    })),
    el("div", { id: "search-results" }),
  );
  const inp = $("#search-input"); inp.focus(); try { inp.setSelectionRange(q0.length, q0.length); } catch {}
  if (q0) doSearch(q0);
  else $("#search-results").append(el("div", { class: "empty" }, emptyArt("empty-search"), el("h3", { text: "Search your archive" }), el("p", { text: "Find mail, files, events, contacts, tasks and notes." })));
}
async function doSearch(q) {
  const box = clear($("#search-results")); box.append(el("div", { class: "spinner" }));
  try {
    const d = await api("/api/v1/search?" + qs({ account: App.account, q }));
    const hits = d.hits || [];
    clear(box);
    if (!hits.length) { box.append(el("div", { class: "empty" }, emptyArt("empty-search"), el("h3", { text: "No matches" }), el("p", { text: `Nothing matches “${q}”.` }))); return; }
    const groups = {};
    hits.forEach(h => (groups[h.service] = groups[h.service] || []).push(h));
    box.append(el("div", { class: "dim", style: "margin-bottom:12px", text: `${hits.length} result${hits.length === 1 ? "" : "s"} for “${q}”` }));
    SERVICES.forEach(s => {
      const g = groups[s.id]; if (!g || !g.length) return;
      box.append(el("h3", { class: "sb-section", style: "display:flex;align-items:center;gap:8px" }, icon(s.icon, "icon-sm"), `${s.label} (${g.length})`));
      const list = el("div", { class: "card", style: "padding:0;overflow:hidden;margin-bottom:16px" });
      g.forEach(h => list.append(searchRow(h)));
      box.append(list);
    });
  } catch (e) { clear(box).append(el("div", { class: "empty" }, el("h3", { text: "Search failed" }), el("p", { text: e.message }))); }
}
function searchRow(h) {
  const q = { account: App.account, service: h.service, id: h.remote_id };
  return el("button", { class: "list-row search-row", onclick: () => h.has_body ? window.open(`/api/v1/view?${qs(q)}`, "_blank", "noopener") : go(h.service) },
    el("span", { class: "avatar", style: `--svc:var(--svc-${h.service});background:color-mix(in oklab,var(--svc-${h.service}) 30%,var(--bg-3));width:30px;height:30px;font-size:11px`, text: initials(h.name) }),
    el("div", { class: "grow" }, el("div", { class: "truncate", text: h.name || "(no name)" }), el("div", { class: "dim", style: "font-size:12px", text: h.item_type })),
    el("span", { class: "badge", text: h.service }));
}

/* ---------------------------------------------------------------- settings */
function kvList(rows) { const dl = el("dl", { class: "kv" }); rows.forEach(([k, v]) => dl.append(el("dt", { text: k }), el("dd", { text: v == null ? "—" : String(v) }))); return dl; }
async function renderSettingsView(view) {
  clear(view).append(el("h1", { class: "view-title", text: "Settings" }), el("p", { class: "view-sub", text: "Configuration and sync controls (read-only)." }));
  const body = el("div", { class: "grid", style: "max-width:720px" }); view.append(body);
  body.append(el("div", { class: "card" }, el("div", { class: "spinner" })));
  try {
    const [cfg, st] = await Promise.all([api("/api/v1/settings").catch(() => ({})), api("/api/v1/sync/state").catch(() => ({}))]);
    const sy = cfg.sync || {}, acc = (cfg.accounts || []).find(a => a.id === App.account) || {};
    clear(body);
    body.append(el("div", { class: "card" }, el("h3", { class: "sb-section", text: "Account" }),
      kvList([["User", acc.username || App.account], ["Sync root", acc.sync_root], ["Archive root", acc.archive_root], ["Mount point", acc.mount_point || "—"]])));
    const syncCard = el("div", { class: "card" }, el("h3", { class: "sb-section", text: "Sync" }),
      kvList([["Scheduled", st.enabled ? (st.paused ? "paused" : "running") : "off"], ["Trash retention", (sy.trash_retention_days ?? "—") + " days"], ["Body index (FTS)", sy.body_index ? "on" : "off"], ["Change source", sy.change_source || "—"]]));
    if (st.enabled && CAP.sync) syncCard.append(el("div", { style: "display:flex;gap:8px;margin-top:12px" },
      el("button", { class: "btn", onclick: () => syncCmd("now") }, icon("refresh-cw", "icon-sm"), "Sync now"),
      st.paused ? el("button", { class: "btn", onclick: () => syncCmd("resume") }, icon("play", "icon-sm"), "Resume") : el("button", { class: "btn", onclick: () => syncCmd("pause") }, icon("pause", "icon-sm"), "Pause")));
    body.append(syncCard);
    body.append(el("div", { class: "card", style: "display:flex;align-items:center;gap:16px" }, logoGlyph(48),
      el("div", {}, el("div", { style: "font-size:16px;font-weight:700", html: "iSync<span style='background:var(--grad-accent);-webkit-background-clip:text;background-clip:text;color:transparent'>You</span>" }),
        el("div", { class: "dim", text: "Microsoft 365 personal backup & archive" }))));
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load settings" }), el("p", { text: e.message }))); }
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
  const jumps = [
    ...SERVICES.map(s => ({ label: "Go to " + s.label, icon: s.icon, run: () => { closePalette(); go(s.id); } })),
    { label: "Go to Settings", icon: "settings", run: () => { closePalette(); go("settings"); } },
    { label: "Sync now", icon: "refresh-cw", run: () => { closePalette(); syncCmd("now"); } },
    { label: "Pause sync", icon: "pause", run: () => { closePalette(); syncCmd("pause"); } },
    { label: "Resume sync", icon: "play", run: () => { closePalette(); syncCmd("resume"); } },
  ];
  renderRes(jumps);
  let timer;
  input.addEventListener("input", () => {
    const q = input.value.trim(); clearTimeout(timer);
    if (!q) { sel = 0; return renderRes(jumps); }
    timer = setTimeout(async () => {
      const local = jumps.filter(j => j.label.toLowerCase().includes(q.toLowerCase()));
      const full = { label: `Search everywhere for “${q}”`, icon: "search", run: () => { closePalette(); go("search?q=" + encodeURIComponent(q)); } };
      let hits = [];
      try { const d = await api("/api/v1/search?" + qs({ account: App.account, q })); hits = (d.hits || []).slice(0, 8).map(h => ({ label: h.name || "(no name)", icon: (SERVICES.find(s => s.id === h.service) || {}).icon, badge: h.service, run: () => { closePalette(); if (h.has_body) window.open(`/api/v1/view?${qs({ account: App.account, service: h.service, id: h.remote_id })}`, "_blank", "noopener"); else go(h.service); } })); } catch {}
      sel = 0; renderRes([full, ...local, ...hits]);
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
    else if (e.key === "Escape" && sheetEl) closeSheet();
  });
  try {
    const d = await api("/api/v1/accounts");
    App.accounts = d.accounts || [];
    if (App.accounts.length) App.account = App.accounts[0].id;
  } catch {}
  onRoute();
}
init();
