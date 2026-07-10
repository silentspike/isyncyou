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
  for (const kid of kids.flat(Infinity)) {
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
  x: "M18 6L6 18M6 6l12 12", "chevron-right": "M9 6l6 6-6 6", "chevron-left": "M15 6l-6 6 6 6", "chevron-down": "M6 9l6 6 6-6",
  plus: "M12 5v14M5 12h14",
  paperclip: "M21.4 11.05l-9.19 9.19a5 5 0 0 1-7.07-7.07l9.19-9.19a3.5 3.5 0 0 1 4.95 4.95l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48",
  "external-link": "M15 3h6v6M10 14L21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6",
  clock: "M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20M12 6v6l4 2",
  list: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01",
  image: "M19 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V5a2 2 0 0 0-2-2zM8.5 10a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3M21 15l-5-5L5 21",
  globe: "M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z",
  "file-text": "M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8zM14 2v6h6M16 13H8M16 17H8M10 9H8",
  "info": "M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zM12 16v-4M12 8h.01",
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
  send: "M22 2L11 13M22 2l-7 20-4-9-9-4z",
  "corner-up-left": "M9 14L4 9l5-5M4 9h11a4 4 0 0 1 4 4v7",
  "corner-up-right": "M15 14l5-5-5-5M20 9H9a4 4 0 0 0-4 4v7",
  "trash-2": "M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M10 11v6M14 11v6",
  tag: "M20.59 13.41l-7.17 7.17a2 2 0 0 1-2.83 0L2 12V2h10l8.59 8.59a2 2 0 0 1 0 2.82zM7 7h.01",
  "mail-open": "M21 8v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8M3 8l9-6 9 6M3 8l9 6 9-6",
  sparkles: "M12 3l2.2 6.8L21 12l-6.8 2.2L12 21l-2.2-6.8L3 12l6.8-2.2z",
  upload: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4M17 8l-5-5-5 5M12 3v12",
  pencil: "M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z",
  "folder-input": "M2 9V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H20a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H2M2 13h10M9 16l3-3-3-3",
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

/* ---------------------------------------------------------------- ambient graphics (trusted in-code SVG)
   The approved mockup is a "cosmic archive console": a constellation network
   behind the whole shell, a flowing particle stream in open space, and line-art
   vault/shield motifs. All generated deterministically here (no external assets,
   CSP-safe — same trust as logoGlyph). */
// tiny deterministic PRNG (mulberry32) so the network is stable across renders
function rng(seed) { let s = seed >>> 0; return () => { s = s + 0x6D2B79F5 | 0; let t = Math.imul(s ^ s >>> 15, 1 | s); t = t + Math.imul(t ^ t >>> 7, 61 | t) ^ t; return ((t ^ t >>> 14) >>> 0) / 4294967296; }; }
// living constellation: one shared rAF loop drives every registered <canvas>.
// Nodes drift on their own slow velocities (parallax-ish, accent stars a touch
// faster), near-neighbour links are redrawn each frame so the web "breathes",
// dots gently twinkle. Paused when the document is hidden; honours
// prefers-reduced-motion (one static frame, no loop); self-heals by dropping
// layers whose canvas left the DOM (e.g. a closed sheet).
const Net = (() => {
  const layers = new Set();
  let raf = 0, last = 0;
  const reduce = !!(window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches);
  const TWO_PI = Math.PI * 2;
  // Frame throttle: the constellation drifts slowly, so 60 fps is wasted work — every redraw
  // forces the glass layers' backdrop-filter blur to recompute (the measured CPU/GPU hotspot).
  // Cap to ~20 fps: physics use the real elapsed time so the drift speed is identical, only the
  // redraw (and thus blur-recompute) frequency drops ~3×. The animation itself is unchanged.
  const FRAME_MS = 1000 / 20;
  function makeNodes(w, h, topWeighted) {
    const rnd = rng(0x51e3a17), N = Math.max(8, Math.round(w * h / 11000)), nodes = [];
    for (let i = 0; i < N; i++) {
      const yb = rnd(), accent = rnd() > 0.7, sp = accent ? 1.5 : 1.0;
      nodes.push({ x: rnd() * w, y: (topWeighted ? yb * yb : yb) * h, r: 0.7 + rnd() * 1.9, a: accent,
        vx: (rnd() - 0.5) * sp, vy: (rnd() - 0.5) * sp, tw: rnd() * TWO_PI, ts: 0.6 + rnd() * 0.9 });
    }
    return nodes;
  }
  function resize(layer) {
    const r = layer.canvas.getBoundingClientRect();
    layer.w = Math.max(1, r.width); layer.h = Math.max(1, r.height);
    // The constellation sits behind blurred glass, so sub-native resolution is invisible; cap
    // the DPR lower than the display's to cut canvas fill-rate (fewer pixels to paint + blur).
    layer.dpr = Math.min(window.devicePixelRatio || 1, 1.5);
    layer.canvas.width = Math.round(layer.w * layer.dpr); layer.canvas.height = Math.round(layer.h * layer.dpr);
    layer.ctx.setTransform(layer.dpr, 0, 0, layer.dpr, 0, 0);
    layer.nodes = makeNodes(layer.w, layer.h, layer.topWeighted);
  }
  function draw(layer, now) {
    const { ctx, w, h, nodes } = layer, maxD = Math.min(w, h) * 0.14;
    ctx.clearRect(0, 0, w, h);
    ctx.lineWidth = 0.9; ctx.strokeStyle = "#9aa2fb";
    for (let i = 0; i < nodes.length; i++) {
      let c = 0;
      for (let j = i + 1; j < nodes.length && c < 3; j++) {
        const dx = nodes[i].x - nodes[j].x, dy = nodes[i].y - nodes[j].y, d = Math.hypot(dx, dy);
        if (d < maxD) { ctx.globalAlpha = (1 - d / maxD) * 0.7; ctx.beginPath(); ctx.moveTo(nodes[i].x, nodes[i].y); ctx.lineTo(nodes[j].x, nodes[j].y); ctx.stroke(); c++; }
      }
    }
    for (const n of nodes) {
      ctx.globalAlpha = (n.a ? 0.95 : 0.7) * (0.68 + 0.32 * Math.sin(now * 0.0016 * n.ts + n.tw));
      ctx.fillStyle = n.a ? "#a78bfa" : "#c7d2fe";
      ctx.beginPath(); ctx.arc(n.x, n.y, n.r, 0, TWO_PI); ctx.fill();
    }
    ctx.globalAlpha = 1;
  }
  function tick(now) {
    if (!last) last = now;
    const elapsed = now - last;
    if (document.hidden) {
      last = now;                                   // don't bank a huge dt while hidden
    } else if (elapsed >= FRAME_MS) {               // throttle: skip frames under the fps cap
      const dt = Math.min(4, elapsed / 16.67); last = now;
      for (const layer of layers) {
        if (!layer.canvas.isConnected) { layers.delete(layer); continue; }   // self-heal
        const m = 8;
        for (const n of layer.nodes) {
          n.x += n.vx * dt; n.y += n.vy * dt;
          if (n.x < -m) n.x = layer.w + m; else if (n.x > layer.w + m) n.x = -m;
          if (n.y < -m) n.y = layer.h + m; else if (n.y > layer.h + m) n.y = -m;
        }
        draw(layer, now);
      }
    }
    raf = layers.size ? requestAnimationFrame(tick) : 0;
  }
  function register(canvas, opts) {
    const layer = { canvas, ctx: canvas.getContext("2d"), topWeighted: !opts || opts.topWeighted !== false, w: 1, h: 1, dpr: 1, nodes: [] };
    resize(layer); layers.add(layer);
    if (reduce) draw(layer, 0);
    else if (!raf) { last = 0; raf = requestAnimationFrame(tick); }
    return layer;
  }
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) { if (raf) { cancelAnimationFrame(raf); raf = 0; } }
    else if (layers.size && !reduce && !raf) { last = 0; raf = requestAnimationFrame(tick); }
  });
  return { register, resize, reduce, _layers: layers };
})();
let _bgLayer = null;
function paintBackdrop() {
  const host = document.getElementById("bg-net"); if (!host) return;
  let canvas = host.querySelector("canvas");
  if (!canvas) { host.innerHTML = ""; canvas = el("canvas", { class: "net-canvas" }); host.append(canvas); }
  if (!_bgLayer || !_bgLayer.canvas.isConnected) _bgLayer = Net.register(canvas, { topWeighted: true });
  else Net.resize(_bgLayer);
}
// the signature geometric constellation, as a backdrop element for a detail sheet
// (so sheets carry the same animated background as the main views, not just a glow).
// Registered after the sheet is in the DOM (next frame) so the canvas has a size.
function sheetNet() {
  const wrap = el("div", { class: "sheet-net" }), canvas = el("canvas", { class: "net-canvas" });
  wrap.append(canvas);
  requestAnimationFrame(() => { if (canvas.isConnected) Net.register(canvas, { topWeighted: false }); });
  return wrap;
}
// flowing particle stream — a wave of dots, brightest mid-span, fading at the ends
function flowWave(w = 540, h = 300) {
  const dots = [], rows = 6, n = 74;
  for (let r = 0; r < rows; r++) {
    const amp = 26 + r * 7, phase = r * 0.6, yb = h * 0.52 + (r - rows / 2) * 9;
    for (let i = 0; i < n; i++) {
      const t = i / (n - 1), edge = Math.sin(t * Math.PI);
      const x = t * w, y = yb + Math.sin(t * Math.PI * 2.2 + phase) * amp * edge;
      dots.push(`<circle cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="${(0.5 + edge * 1.5).toFixed(2)}" opacity="${((0.05 + edge * 0.5) * (1 - r * 0.11)).toFixed(3)}"/>`);
    }
  }
  return `<svg viewBox="0 0 ${w} ${h}" width="100%" height="100%" preserveAspectRatio="xMidYMid meet" xmlns="http://www.w3.org/2000/svg" fill="#7c8cf8">${dots.join("")}</svg>`;
}

/* ---------------------------------------------------------------- charts (pure SVG, no lib) */
const SVGNS = "http://www.w3.org/2000/svg";
function svg(tag, attrs) {
  const n = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) n.setAttribute(k, v);
  return n;
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
  backup: "__BACKUP_CAP_TOKEN__",
  mobileJobs: "__MOBILE_JOB_CAP_TOKEN__",
  sync: "__SYNC_CAP_TOKEN__",
  share: "__SHARE_CAP_TOKEN__",
  verify: "__VERIFY_CAP_TOKEN__",
  settings: "__SETTINGS_CAP_TOKEN__",
  mailwrite: "__MAILWRITE_CAP_TOKEN__",
  calendarwrite: "__CALENDARWRITE_CAP_TOKEN__",
  contactwrite: "__CONTACTWRITE_CAP_TOKEN__",
  todowrite: "__TASKWRITE_CAP_TOKEN__",
  onenotewrite: "__ONENOTEWRITE_CAP_TOKEN__",
  onedrivewrite: "__ONEDRIVEWRITE_CAP_TOKEN__",
  account: "__ACCOUNT_CAP_TOKEN__",
  push: "__PUSH_CAP_TOKEN__",
  agent: "__AGENT_CAP_TOKEN__",
  onedriveMode: "__ONEDRIVE_MODE_CAP_TOKEN__",
  transfers: "__TRANSFER_CAP_TOKEN__",
  onedriveManage: "__ONEDRIVE_MANAGE_CAP_TOKEN__",
};

/* Transport (#0A/#721): the standalone phone exposes an origin-bound WebMessage bridge.
   WebView JS never reads or sends the native session token; Kotlin injects the trusted
   Activity session for bridge requests and app-origin resources. Desktop keeps the normal
   fetch/EventSource path. */
const BRIDGE = (typeof window !== "undefined" && window.__isyBridge) || null;
const MOBILE = !!BRIDGE;
let _bridgeSeq = 0;
const BRIDGE_TIMEOUT_MS = 15000;
const NATIVE_TIMEOUT_MS = 5000;
const BIO_TIMEOUT_MS = 120000;
const BRIDGE_STREAM_TIMEOUT_MS = BRIDGE_TIMEOUT_MS;
const _bridgePending = new Map(); // request/native id -> { resolve, reject, timer }
const _bridgeStreams = new Map(); // stream id -> onEvent handler
const _bioPending = new Map();    // biometric request id -> { resolve, timer } (#0.6)
const _bridgeStats = { requests: 0, native: 0, streams: 0, bio: 0, events: 0 };
if (typeof window !== "undefined") window.__isyBridgeTransportStats = _bridgeStats;
if (BRIDGE) {
  BRIDGE.onmessage = (ev) => {
    let m; try { m = JSON.parse(ev.data); } catch (_) { return; }
    if (m.t === "res") {
      const p = _bridgePending.get(m.id);
      if (p) { _bridgePending.delete(m.id); clearTimeout(p.timer); p.resolve({ status: m.status, body: m.body }); }
    } else if (m.t === "bio") {
      // Native BiometricPrompt result (#0.6): {ok} tells us whether the human confirmed.
      const p = _bioPending.get(m.id);
      if (p) { _bioPending.delete(m.id); clearTimeout(p.timer); p.resolve(!!m.ok); }
    } else if (m.t === "evt") {
      const h = _bridgeStreams.get(m.id);
      _bridgeStats.events++;
      if (h && m.ev) {
        if (h.timer) { clearTimeout(h.timer); h.timer = null; }
        h.onEvent(m.ev.event || "message", m.ev.data || "");
      }
    } else if (m.t === "end") {
      const h = _bridgeStreams.get(m.id);
      if (h) {
        if (h.timer) clearTimeout(h.timer);
        _bridgeStreams.delete(m.id);
        if (h.onError) h.onError();
      }
    }
  };
}
function bridgeRoundTrip(msg, timeoutMs) {
  if (!BRIDGE) return Promise.reject(new Error("Bridge unavailable"));
  return new Promise((resolve, reject) => {
    const id = msg.id || ("n" + (++_bridgeSeq));
    msg.id = id;
    const timer = setTimeout(() => {
      _bridgePending.delete(id);
      reject(new Error("Bridge timeout"));
    }, timeoutMs || BRIDGE_TIMEOUT_MS);
    _bridgePending.set(id, { resolve, reject, timer });
    try {
      BRIDGE.postMessage(JSON.stringify(msg));
    } catch (e) {
      clearTimeout(timer);
      _bridgePending.delete(id);
      reject(e);
    }
  });
}
function bridgeSend(method, path, headers, body) {
  const id = "r" + (++_bridgeSeq);
  _bridgeStats.requests++;
  return bridgeRoundTrip({ t: "req", id, method, path, headers, body: body ?? null }, BRIDGE_TIMEOUT_MS);
}
async function nativeCall(op, payload, timeoutMs) {
  const id = "n" + (++_bridgeSeq);
  _bridgeStats.native++;
  const res = await bridgeRoundTrip({ t: "native", id, op, payload: payload || {} }, timeoutMs || NATIVE_TIMEOUT_MS);
  let body = {};
  try {
    body = res.body ? JSON.parse(res.body) : {};
  } catch (_) {
    throw new Error("Native call returned non-JSON response");
  }
  const status = Number(res.status);
  if (!Number.isFinite(status) || status < 200 || status >= 300) {
    throw new Error(body.error || body.reason || status || "Native call failed");
  }
  return body;
}
/* ---------------------------------------------------------------- push registration (#576)
   In the Android shell, the FCM token is read through the origin-bound native bridge.
   Empty token is a no-op/retry; a plain browser has no bridge and skips this entirely. */
async function registerPushToken() {
  try {
    if (!CAP.push || !BRIDGE) return false;
    const d = await nativeCall("pushToken", {}, NATIVE_TIMEOUT_MS);
    const token = d && d.token;
    if (!token) return false;
    await post(`/api/v1/push/register?${qs({ token })}`, CAP.push);
    return true;
  } catch (_) { return false; } // best-effort: never block UI load on push
}
/* Ask the native side (#0.6) to run a BiometricPrompt and, on success, arm the server's
   per-action token for `pat`. Resolves true only if the human authenticated. Without the
   native bridge there is no biometric path, so a destructive op cannot be confirmed. */
function runBiometricConfirm(pat, label) {
  if (!BRIDGE) return Promise.resolve(false);
  return new Promise((resolve) => {
    const id = "b" + (++_bridgeSeq);
    const timer = setTimeout(() => {
      _bioPending.delete(id);
      resolve(false);
    }, BIO_TIMEOUT_MS);
    _bioPending.set(id, { resolve, timer });
    _bridgeStats.bio++;
    try {
      BRIDGE.postMessage(JSON.stringify({ t: "bio", id, pat, label }));
    } catch (_) {
      clearTimeout(timer);
      _bioPending.delete(id);
      resolve(false);
    }
  });
}
/* A short human label for the biometric sheet from the challenge payload (#0.6). */
function biometricLabel(d) {
  const verb = d.op === "delete" ? "Delete" : d.op === "share" ? "Share"
    : d.op === "move-out-of-protected" ? "Move out of offline folder"
    : d.op === "mode-switch-offline-large" ? "Make folder offline"
    : d.op === "bulk" ? "Bulk OneDrive change"
    : d.op ? d.op.charAt(0).toUpperCase() + d.op.slice(1) : "Confirm";
  const service = d.service === "onedrive" ? "OneDrive" : d.service || "Microsoft 365";
  return `${verb} in ${service}`;
}
/* Open an SSE-style stream over the active transport (#0A). Mobile bridge mode uses
   the native stream path and never falls back to EventSource. Desktop uses EventSource. */
function openEventStream(path, onEvent, onError) {
  if (BRIDGE) {
    const id = "s" + (++_bridgeSeq);
    const timer = setTimeout(() => {
      const h = _bridgeStreams.get(id);
      if (!h) return;
      _bridgeStreams.delete(id);
      try { BRIDGE.postMessage(JSON.stringify({ t: "unsub", id })); } catch (_) {}
      if (h.onError) h.onError();
    }, BRIDGE_STREAM_TIMEOUT_MS);
    _bridgeStreams.set(id, { onEvent, onError, timer });
    _bridgeStats.streams++;
    try { BRIDGE.postMessage(JSON.stringify({ t: "sub", id, path })); }
    catch (e) {
      clearTimeout(timer);
      _bridgeStreams.delete(id);
      if (onError) setTimeout(onError, 0);
    }
    return {
      close() {
        const h = _bridgeStreams.get(id);
        if (h && h.timer) clearTimeout(h.timer);
        _bridgeStreams.delete(id);
        try { BRIDGE.postMessage(JSON.stringify({ t: "unsub", id })); } catch (_) {}
      }
    };
  }
  const es = new EventSource(path);
  es.onmessage = (e) => onEvent("message", e.data);
  es.addEventListener("change", () => onEvent("change", ""));
  es.addEventListener("done", () => onEvent("done", ""));
  es.onerror = () => { if (onError) onError(); };
  return { close() { try { es.close(); } catch (_) {} } };
}
/* One request over the active transport; returns parsed JSON, throws on non-2xx. */
async function request(method, path, opts) {
  const o = opts || {};
  const headers = {};
  if (o.capToken) headers["X-Capability-Token"] = o.capToken;
  if (o.headers) {
    Object.entries(o.headers).forEach(([k, v]) => {
      if (BRIDGE && k.toLowerCase() === "x-session-token") return;
      headers[k] = v;
    });
  } // #657: e.g. X-Body-Encoding: base64
  let status, d;
  if (BRIDGE) {
    const res = await bridgeSend(method, path, headers, o.body);
    status = Number(res.status);
    d = {}; try { d = res.body ? JSON.parse(res.body) : {}; } catch (_) { d = {}; }
  } else {
    const init = { method, headers };
    if (o.body !== undefined) init.body = o.body;
    const r = await fetch(path, init);
    status = r.status;
    d = await r.json().catch(() => ({}));
  }
  // #onedrive-mobile 0.6: a destructive op the mobile router gated answers with a
  // confirmation_required challenge instead of acting. Run the native biometric and, on a
  // human confirm, re-issue exactly once with the per-action token. Guarded against loops:
  // a request that already carries `_pat` is never re-challenged into another biometric.
  if (status >= 200 && status < 300 && d && d.status === "confirmation_required"
      && d.pending_action_id && !/[?&]_pat=/.test(path)) {
    const ok = await runBiometricConfirm(d.pending_action_id, biometricLabel(d));
    if (!ok) throw new Error("Confirmation cancelled");
    const sep = path.includes("?") ? "&" : "?";
    return request(method, `${path}${sep}_pat=${encodeURIComponent(d.pending_action_id)}`, opts);
  }
  if (!Number.isFinite(status) || status < 200 || status >= 300) throw new Error(d.error || status || "Request failed");
  return d;
}
async function api(path) { return request("GET", path); }
async function post(path, capToken, body) { return request("POST", path, { capToken, body }); }
/* Base64-encode raw bytes (Uint8Array) in fromCharCode-safe chunks (#657). The mobile bridge
   body is text-only, so a binary upload rides base64; serve.rs decodes on X-Body-Encoding. */
function bytesToBase64(bytes) {
  let bin = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  return btoa(bin);
}
/* POST a binary body base64-encoded, flagged X-Body-Encoding: base64 for serve.rs to decode.
   Uniform across the native bridge and the desktop HTTP path (#657). */
async function postBinary(path, capToken, bytes) {
  return request("POST", path, { capToken, body: bytesToBase64(bytes), headers: { "X-Body-Encoding": "base64" } });
}
/* Confirm a destructive action before it is sent (#0.6). On the standalone phone the
   native biometric per-action gate IS the confirmation — a strictly stronger one shown
   right before the op — so the blocking window.confirm() is skipped there (the WebView
   has no dialog handler for it anyway). Desktop keeps the classic confirm(). */
function confirmDestructive(message) {
  if (MOBILE) return true;
  return confirm(message);
}
const qs = (o) => Object.entries(o).map(([k, v]) => `${k}=${encodeURIComponent(v)}`).join("&");
const initials = (s) => (s || "?").trim().split(/[\s@.]+/).filter(Boolean).slice(0, 2).map(x => x[0].toUpperCase()).join("") || "?";

/* ---------------------------------------------------------------- shared Live∪Backup status badge (#560) */
// Mirrors backup_state() in lib.rs: every element is one of four states.
// Reused by every list + detail view so coverage reads identically everywhere.
// No emoji — Lucide glyphs only, tinted per state in CSS.
// On the standalone phone (#89 P6) the local store is a *cache*, not a
// backup-of-record, so the four states read in "cached on this device" terms.
const STATES = MOBILE ? {
  live_only:   { icon: "globe",        label: "Live only",     title: "In Microsoft 365 — not yet cached on this device" },
  live_backup: { icon: "shield-check", label: "Live + cached", title: "In Microsoft 365 and cached on this device" },
  backup_only: { icon: "archive",      label: "Cached only",   title: "Deleted from Microsoft 365 — still cached on this device" },
  stale:       { icon: "rotate-ccw",   label: "Stale",         title: "Microsoft 365 changed since the last refresh — re-syncing" },
} : {
  live_only:   { icon: "globe",        label: "Live only",     title: "In Microsoft 365 — not yet in your backup" },
  live_backup: { icon: "shield-check", label: "Live + backup", title: "In Microsoft 365 and safely backed up" },
  backup_only: { icon: "archive",      label: "Backup only",   title: "Deleted from Microsoft 365 — preserved in your backup" },
  stale:       { icon: "rotate-ccw",   label: "Stale",         title: "Microsoft 365 changed since the last backup — re-run a backup to refresh" },
};
const STATE_KEYS = new Set(Object.keys(STATES));
const stateKey = (it) => (STATES[it.state] ? it.state : "live_only");
// NB: distinct from the older `syncBadge()` (transient sync_state pill) — this is
// the persistent Live∪Backup coverage badge. Both axes can show on one row.
function coverageBadge(it) {
  const k = stateKey(it), s = STATES[k];
  return el("span", { class: "badge state-" + k, title: s.title }, icon(s.icon, "icon-sm"), el("span", { class: "badge-label", text: s.label }));
}
const stateMatch = (it, f) => f === "all" || stateKey(it) === f;
// Shared inline filter chips for the bespoke, fully-loaded views (drive/calendar/
// todo/onenote) — JS re-render over the in-memory item set. (The generic
// server-paginated fallback uses the CSS-driven `stateChipBar` instead, which
// hides across pages without re-rendering.) `onPick(key)` re-renders the view.
function stateFilterBar(items, current, onPick) {
  const counts = { all: items.length };
  for (const k of STATE_KEYS) counts[k] = 0;
  items.forEach(it => { counts[stateKey(it)]++; });
  const mk = (key, label) => el("button", { class: "state-chip" + (key === current ? " active" : ""), onclick: () => onPick(key) },
    key === "all" ? null : icon(STATES[key].icon, "icon-sm"), el("span", { text: label }), el("span", { class: "sc-count", text: String(counts[key] || 0) }));
  return el("div", { class: "state-chips" }, mk("all", "All"), mk("live_only", STATES.live_only.label), mk("live_backup", STATES.live_backup.label), mk("backup_only", STATES.backup_only.label), mk("stale", STATES.stale.label));
}
// Activity timestamps come back as unix seconds (audit_timestamp); everything
// else is an ISO/RFC string. Normalise both to a JS Date.
function toDate(s) {
  if (s == null || s === "") return null;
  if (/^\d{9,11}$/.test(String(s))) return new Date(Number(s) * 1000); // unix seconds
  const d = new Date(s); return isNaN(d) ? null : d;
}
// Full absolute timestamp everywhere, e.g. "22.06.2026 14:30:21" (DD.MM.YYYY
// HH:MM:SS, 24h) — the user wants the exact date + time in every list/row, not a
// relative "weekday"/"time only" form.
function fmtDate(s) {
  const d = toDate(s); if (!d) return s ? String(s) : "";
  const p = (n) => String(n).padStart(2, "0");
  return `${p(d.getDate())}.${p(d.getMonth() + 1)}.${d.getFullYear()} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
function fmtFullDate(s) {
  return fmtDate(s);
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
  { id: "assistant", label: "Assistant", icon: "sparkles", cap: "agent", appOnly: true },
];
const serviceVisible = (s) => !s.cap || !!CAP[s.cap];
const visibleServices = () => SERVICES.filter(serviceVisible);
const archiveServices = () => SERVICES.filter(s => !s.appOnly);
const RESTORABLE = new Set(["mail", "calendar", "contacts", "todo", "onenote"]);
const SHAREABLE = new Set(["onedrive"]);

/* ---------------------------------------------------------------- global state */
const App = { account: null, accounts: [], route: "overview", counts: {}, svcFilter: {} };
// Per-service filter sub-items shown in the LEFT sidebar, indented under the
// active service (NOT a separate rail). Lazy so Mail.cats is populated at call.
// Map a well-known mail folder to an icon; custom folders fall back to "folder".
function mailFolderIcon(name) {
  const n = (name || "").toLowerCase();
  if (n === "inbox") return "inbox";
  if (n === "sent items" || n === "outbox") return "send";
  if (n === "drafts") return "file-text";
  if (n === "deleted items") return "trash-2";
  if (n === "junk email") return "shield";
  if (n === "archive") return "archive";
  return "folder";
}
// Resolve a message's parent folder id → folder display name (or null).
function mailFolderName(id) {
  if (!id) return null;
  const f = (Mail.folders || []).find(x => x.remote_id === id);
  return f ? (f.name || null) : null;
}
// Sidebar nav specs for the mailbox folder tree (#563): one entry per archived
// mailFolder, ordered like Outlook, each counting the messages that live in it
// (message.parent_remote_id === folder id). Folders are already loaded into
// Mail.folders; this only navigates them.
function mailFolderSpecs() {
  const order = ["inbox", "drafts", "sent items", "archive", "deleted items", "junk email", "outbox", "conversation history"];
  const rank = n => { const i = order.indexOf((n || "").toLowerCase()); return i < 0 ? 50 : i; };
  return (Mail.folders || []).slice()
    .sort((a, b) => rank(a.name) - rank(b.name) || (a.name || "").localeCompare(b.name || ""))
    .map(f => ({
      key: "folder:" + f.remote_id,
      label: f.name || "(folder)",
      icon: mailFolderIcon(f.name),
      count: m => m.filter(it => it.parent_remote_id === f.remote_id).length,
    }));
}
function svcFilters(service) {
  if (service === "mail") return [
    { sec: "Mailbox" },
    { key: "all", label: "All messages", icon: "inbox", count: m => m.length },
    { key: "attach", label: "With attachments", icon: "paperclip", count: m => m.filter(it => (it.preview || {}).attachments > 0).length },
    { key: "restore", label: (MOBILE ? "Has content" : "Restore-ready"), icon: "rotate-ccw", count: m => m.filter(it => it.has_body).length },
    ...(Mail.folders && Mail.folders.length ? [{ sec: "Folders" }, ...mailFolderSpecs()] : []),
    { sec: "Status" },
    { key: "unread", label: "Unread", icon: "mail", count: m => m.filter(it => (it.preview || {}).isRead === false).length },
    { key: "flagged", label: "Flagged", icon: "flag", count: m => m.filter(it => (it.preview || {}).flag === "flagged").length },
    { key: "high", label: "High importance", icon: "flag", count: m => m.filter(it => (it.preview || {}).importance === "high").length },
    ...stateFilterSpecs(),
    { sec: "Categories" },
    ...(Mail.cats || []).map(c => ({
      key: c.name,
      label: c.name,
      color: presetColor((c.preview || {}).color),
      count: m => m.filter(it => ((it.preview || {}).categories || []).includes(c.name)).length,
    })),
  ];
  if (service === "contacts") return [
    { sec: "Directory" },
    { key: "all", label: "All contacts", icon: "users", count: c => c.length },
    { key: "email", label: "With email", icon: "mail", count: c => c.filter(it => (it.preview || {}).email).length },
    { key: "company", label: "With company", icon: "building", count: c => c.filter(it => (it.preview || {}).company).length },
    { key: "restore", label: (MOBILE ? "Has content" : "Restore-ready"), icon: "rotate-ccw", count: c => c.filter(it => it.has_body).length },
    ...stateFilterSpecs(),
  ];
  return null;
}
// shared "Backup status" sidebar section (#560) for the bespoke views.
function stateFilterSpecs() {
  return [
    { sec: MOBILE ? "Cache status" : "Backup status" },
    ...[...STATE_KEYS].map(k => ({ key: k, label: STATES[k].label, icon: STATES[k].icon,
      count: items => items.filter(it => stateKey(it) === k).length })),
  ];
}
const svcFilter = (service) => App.svcFilter[service] || "all";
function setSvcFilter(service, key) {
  App.svcFilter[service] = key;
  document.querySelectorAll(`#subnav-${service} .nav-subitem`).forEach(b => b.classList.toggle("active", b.dataset.k === key));
  if (service === "mail") mailRender();
  else if (service === "contacts") contactsRenderList();
}
// fill the sidebar sub-item counts once the active view has loaded its items
function fillSubnavCounts(service, items) {
  const specs = svcFilters(service); if (!specs) return;
  specs.filter(f => f.key).forEach(f => {
    const c = document.querySelector(`#subnav-${service} [data-cnt="${f.key}"]`);
    if (c) c.textContent = String(f.count(items));
  });
}
// Rebuild one service's sidebar sub-items in place (without re-rendering the
// shell, which would wipe the active view). Used after mail loads so the real
// Outlook categories appear (they're only known once the items are fetched).
function rebuildSubnav(service) {
  const host = $(`#subnav-${service}`);
  const specs = svcFilters(service);
  if (!host || !specs) return;
  clear(host);
  specs.forEach(f => {
    if (f.sec) { host.append(el("div", { class: "nav-sub-sec", text: f.sec })); return; }
    host.append(el("button", { class: "nav-subitem" + (svcFilter(service) === f.key ? " active" : ""), dataset: { k: f.key }, onclick: () => setSvcFilter(service, f.key) },
      f.color ? el("span", { class: "nav-sub-dot", style: `background:${f.color}` }) : icon(f.icon, "icon-sm"),
      el("span", { class: "grow truncate", text: f.label }),
      el("span", { class: "count tnum", dataset: { cnt: f.key }, text: "·" })));
  });
}
const refreshMailSubnav = () => rebuildSubnav("mail");

/* ---------------------------------------------------------------- toasts */
function toast(msg, kind = "ok") {
  const box = $("#toasts");
  const t = el("div", { class: `toast ${kind}` }, icon(kind === "ok" ? "check-square" : "x"), el("span", { text: msg }));
  box.append(t);
  setTimeout(() => t.remove(), 3800);
}

/* ---------------------------------------------------------------- shell render */
/* ---------------------------------------------------------------- easter egg 🛸 */
// 5 taps on the Settings "about" line toggle a hidden UFO nav item that opens a
// tiny Space Invaders game. State persists in localStorage; 5 more taps hide it.
const eggOn = () => { try { return localStorage.getItem("isy_egg") === "1"; } catch (_) { return false; } };
let _eggTaps = 0, _eggTapT = 0;
function eggTap() {
  clearTimeout(_eggTapT);
  _eggTapT = setTimeout(() => { _eggTaps = 0; }, 1500);   // taps must be in quick succession
  if (++_eggTaps < 5) return;
  _eggTaps = 0;
  const on = !eggOn();
  try { localStorage.setItem("isy_egg", on ? "1" : "0"); } catch (_) { }
  toast(on ? "UFO unlocked — check the nav" : "UFO hidden");
  if (!on && App.route === "invaders") { go("overview"); return; }
  renderShell();
}
// custom UFO glyph (mirrors icon(): an <svg class="icon"> with stroke:currentColor)
function ufoGlyph(cls = "icon") {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24"); svg.setAttribute("class", cls);
  svg.innerHTML = '<ellipse cx="12" cy="13" rx="9" ry="3.4"/>' +
    '<path d="M7.5 11.3C7.5 9 9.5 7.2 12 7.2s4.5 1.8 4.5 4.1"/>' +
    '<path d="M6 15.6 4.6 18.6M12 16.5V19.3M18 15.6 19.4 18.6"/>';
  return svg;
}
// a tiny, self-contained Space Invaders on a <canvas>. Keyboard (← → space) on
// desktop, on-screen buttons + tap on touch. Self-stops when its canvas leaves
// the DOM (route change), removing its window listeners.
// game sound: ElevenLabs-generated SFX served same-origin from /sfx/*.mp3, fetched
// once and played via Web Audio (so CSP media-src isn't needed). Toggle persists.
const SFX = {
  ctx: null, buf: {}, ready: false,
  on() { try { return localStorage.getItem("isy_sfx") !== "0"; } catch (_) { return true; } },   // default on
  toggle() { try { localStorage.setItem("isy_sfx", this.on() ? "0" : "1"); } catch (_) { } return this.on(); },
  async init() {
    try {
      if (!this.ctx) this.ctx = new (window.AudioContext || window.webkitAudioContext)();   // reuse the context, but always (re)load the buffers below so changed SFX take effect on each game open
      const src = { shoot: "/sfx/shoot.mp3", boom: "/sfx/boom.mp3", level: "/sfx/level.mp3", drop: "/sfx/drop.mp3", pickup: "/sfx/pickup.mp3", hit: "/sfx/hit.mp3" };
      const cb = "?v=" + Date.now();   // cache-buster: bypass any stale fetch-cache entry from an old max-age response
      for (const k in src) this.buf[k] = await this.ctx.decodeAudioData(await (await fetch(src[k] + cb)).arrayBuffer());
      this.ready = true;
    } catch (_) { }
  },
  resume() { try { if (this.ctx && this.ctx.state === "suspended") this.ctx.resume(); } catch (_) { } },
  play(name, gain = 0.5) {
    if (!this.ready || !this.on() || !this.ctx || !this.buf[name]) return;
    try {
      const s = this.ctx.createBufferSource(); s.buffer = this.buf[name];
      const g = this.ctx.createGain(); g.gain.value = gain;
      s.connect(g); g.connect(this.ctx.destination); s.start();
    } catch (_) { }
  },
};
function speakerGlyph(on, cls = "icon icon-sm") {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24"); svg.setAttribute("class", cls);
  svg.innerHTML = '<path d="M4 9v6h4l5 4V5L8 9H4z"/>' + (on
    ? '<path d="M16 8.5a4.5 4.5 0 0 1 0 7M18.5 6a8 8 0 0 1 0 12"/>'
    : '<path d="M22 9.5l-5 5M17 9.5l5 5"/>');
  return svg;
}
function invToggleSfx() {
  const on = SFX.toggle();
  const b = document.getElementById("sfx-toggle");
  if (b) { b.innerHTML = ""; b.append(speakerGlyph(on)); b.title = on ? "Sound on" : "Sound off"; }
  if (on) { SFX.resume(); SFX.init().then(() => SFX.resume()); toast("Sound on"); } else toast("Sound off");
}
function renderInvaders(view) {
  clear(view).append(
    el("h1", { class: "view-title", text: "Invaders" }),
    el("p", { class: "view-sub", text: "drag to move · auto-fire · grab power-ups · survive" }),
    el("div", { class: "inv-wrap" },
      el("canvas", { id: "inv-bg", class: "inv-bg" }),
      el("button", { id: "sfx-toggle", class: "btn ghost sm icon-only inv-sfx", title: SFX.on() ? "Sound on" : "Sound off", onclick: invToggleSfx }, speakerGlyph(SFX.on())),
      el("canvas", { id: "inv-canvas", class: "inv-canvas" })));
  const ib = document.getElementById("inv-bg"); if (ib) Net.register(ib, { topWeighted: false });   // our animated constellation, full-bleed behind the game
  invadersGame(document.getElementById("inv-canvas"));
}
function invadersGame(canvas) {
  if (!canvas) return;
  const ctx = canvas.getContext("2d"), dpr = Math.min(window.devicePixelRatio || 1, 2);
  let W = 0, H = 0, player, bullets, ebullets, foes, parts, pups, up, dir, base, score, state, level, raf = 0, lastShot = 0;
  function fit() {
    const r = canvas.getBoundingClientRect();
    W = Math.max(240, r.width); H = Math.max(300, r.height);
    canvas.width = Math.round(W * dpr); canvas.height = Math.round(H * dpr); ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }
  const FOE = {
    grunt: { add: 0, col: "#a78bfa", dome: "#c7d2fe", w: 26, h: 16, shoot: false },   // standard
    tank: { add: 3, col: "#fb7185", dome: "#fecdd3", w: 34, h: 20, shoot: false },    // bigger, more HP
    shooter: { add: 1, col: "#34d399", dome: "#a7f3d0", w: 26, h: 16, shoot: true },  // fires back
  };
  function spawnFoes() {
    foes = []; bullets = []; ebullets = []; dir = 1;
    base = 0.4 + (level - 1) * 0.3;                                  // faster each level
    const bhp = 1 + Math.floor((level - 1) / 2);                     // tougher every 2 levels
    const cols = Math.max(4, Math.min(9, Math.floor((W - 40) / 44)));
    const rows = Math.min(3 + level, 6);                            // more rows each level
    const gx = (W - cols * 44) / 2 + 22;
    const sP = level >= 2 ? Math.min(0.05 + level * 0.035, 0.3) : 0, tP = level >= 2 ? Math.min(0.08 + level * 0.025, 0.24) : 0;
    for (let r = 0; r < rows; r++) for (let c = 0; c < cols; c++) {
      const rr = Math.random(), t = rr < sP ? "shooter" : rr < sP + tP ? "tank" : "grunt", d = FOE[t], hp = bhp + d.add;
      foes.push({ x: gx + c * 44, y: 42 + r * 32, w: d.w, h: d.h, alive: true, hp, maxhp: hp, t, shoot: d.shoot, col: d.col, dome: d.dome, cd: 800 + Math.random() * 2200 });
    }
  }
  function reset() {
    player = { x: W / 2, y: H - 24, w: 30, h: 12, hp: 3, maxhp: 3, armor: 0, inv: 0 };
    targetX = W / 2; targetY = H - 24; parts = []; pups = [];
    up = { fire: 320, pierce: 0, bspeed: 7, ease: 0.35, shots: 1 };
    score = 0; level = 1; state = "play";
    spawnFoes();
  }
  function nextLevel() { level++; spawnFoes(); SFX.play("level", 0.6); }
  const keys = { left: false, right: false, up: false, down: false };
  let dragging = false, targetX = 0, targetY = 0;
  const onKey = (e, d) => {
    if (e.key === "ArrowLeft") keys.left = d; else if (e.key === "ArrowRight") keys.right = d;
    else if (e.key === "ArrowUp") keys.up = d; else if (e.key === "ArrowDown") keys.down = d; else return;
    e.preventDefault();
  };
  const kd = (e) => { SFX.resume(); onKey(e, true); }, ku = (e) => onKey(e, false);
  window.addEventListener("keydown", kd); window.addEventListener("keyup", ku);
  const aim = (e) => { const r = canvas.getBoundingClientRect(); targetX = e.clientX - r.left; targetY = e.clientY - r.top; };
  canvas.addEventListener("pointerdown", (e) => { SFX.resume(); if (state !== "play") { reset(); return; } dragging = true; aim(e); });
  canvas.addEventListener("pointermove", (e) => { if (dragging && state === "play") aim(e); });
  ["pointerup", "pointerleave", "pointercancel"].forEach(ev => canvas.addEventListener(ev, () => { dragging = false; }));
  function boom(x, y) {                                               // explosion burst on a hit
    for (let i = 0; i < 12; i++) {
      const a = Math.random() * 6.2832, s = 1 + Math.random() * 3;
      parts.push({ x, y, vx: Math.cos(a) * s, vy: Math.sin(a) * s, life: 1, col: Math.random() > 0.5 ? "#a78bfa" : "#fde68a" });
    }
  }
  function drawShip(x, y, now) {
    ctx.save(); ctx.translate(x, y);
    const fl = 7 + 3 * Math.abs(Math.sin(now * 0.02));               // flickering thruster
    ctx.fillStyle = "#fb923c"; ctx.beginPath(); ctx.moveTo(-3.5, 4); ctx.lineTo(0, 4 + fl); ctx.lineTo(3.5, 4); ctx.closePath(); ctx.fill();
    ctx.fillStyle = "#6366f1"; ctx.beginPath();                      // wings
    ctx.moveTo(-7, 1); ctx.lineTo(-15, 7); ctx.lineTo(-4, 5); ctx.closePath();
    ctx.moveTo(7, 1); ctx.lineTo(15, 7); ctx.lineTo(4, 5); ctx.closePath(); ctx.fill();
    ctx.fillStyle = "#3b9eff"; ctx.beginPath();                      // fuselage
    ctx.moveTo(0, -15); ctx.quadraticCurveTo(7, -5, 7, 5); ctx.lineTo(-7, 5); ctx.quadraticCurveTo(-7, -5, 0, -15); ctx.closePath(); ctx.fill();
    ctx.fillStyle = "#e0f2fe"; ctx.beginPath(); ctx.arc(0, -4, 2.6, 0, 6.2832); ctx.fill();   // cockpit
    ctx.restore();
  }
  function drawFoe(f) {
    ctx.globalAlpha = 0.5 + 0.5 * (f.hp / f.maxhp);                 // damaged foes fade
    ctx.fillStyle = f.col; ctx.beginPath(); ctx.ellipse(f.x, f.y + 2, f.w / 2, f.h / 3, 0, 0, 6.2832); ctx.fill();
    ctx.fillStyle = f.dome; ctx.beginPath(); ctx.ellipse(f.x, f.y - 2, f.w / 4, f.h / 3, 0, 0, 6.2832); ctx.fill();
    if (f.shoot) { ctx.fillStyle = "#064e3b"; ctx.fillRect(f.x - 2, f.y + f.h / 3 - 1, 4, 5); }   // shooter cannon
    ctx.globalAlpha = 1;
  }
  // power-up types — each killed foe may drop one; pick it up to apply forever
  const POW = [
    { k: "atkspeed", col: "#fde68a", apply: () => up.fire = Math.max(70, up.fire - 45) },
    { k: "pierce", col: "#67e8f9", apply: () => up.pierce += 1 },
    { k: "hp", col: "#fb7185", apply: () => { player.maxhp += 1; player.hp = Math.min(player.maxhp, player.hp + 1); } },
    { k: "armor", col: "#60a5fa", apply: () => player.armor += 1 },
    { k: "bspeed", col: "#fb923c", apply: () => up.bspeed += 2 },
    { k: "movespeed", col: "#34d399", apply: () => up.ease = Math.min(0.62, up.ease + 0.06) },
    { k: "morebullets", col: "#c084fc", apply: () => up.shots += 1 },
  ];
  function fire(now) {
    const n = up.shots, spread = 0.5;
    for (let i = 0; i < n; i++) {
      const a = n === 1 ? 0 : (i / (n - 1) - 0.5) * spread;
      bullets.push({ x: player.x, y: player.y - 14, vx: Math.sin(a) * up.bspeed, vy: -Math.cos(a) * up.bspeed, pierce: up.pierce });
    }
    lastShot = now; SFX.play("shoot", 0.2);
  }
  function hurt(now) {
    if (player.inv > now) return;
    if (player.armor > 0) player.armor -= 1; else player.hp -= 1;
    player.inv = now + 1100; boom(player.x, player.y); SFX.play("hit", 0.6);
    if (player.hp <= 0) state = "over";
  }
  function roundRect(x, y, w, h, r) { ctx.beginPath(); ctx.moveTo(x + r, y); ctx.arcTo(x + w, y, x + w, y + h, r); ctx.arcTo(x + w, y + h, x, y + h, r); ctx.arcTo(x, y + h, x, y, r); ctx.arcTo(x, y, x + w, y, r); ctx.closePath(); }
  function drawPowIcon(k, x, y) {
    ctx.save(); ctx.translate(x, y); ctx.strokeStyle = "#0b0b16"; ctx.fillStyle = "#0b0b16"; ctx.lineWidth = 1.7; ctx.lineJoin = "round"; ctx.lineCap = "round"; ctx.beginPath();
    if (k === "atkspeed") { ctx.moveTo(2, -5); ctx.lineTo(-3, 1); ctx.lineTo(0, 1); ctx.lineTo(-2, 5); ctx.lineTo(3, -1); ctx.lineTo(0, -1); ctx.closePath(); ctx.fill(); }
    else if (k === "pierce") { ctx.moveTo(-4, 2); ctx.lineTo(0, -4); ctx.lineTo(4, 2); ctx.moveTo(0, -4); ctx.lineTo(0, 5); ctx.stroke(); }
    else if (k === "hp") { ctx.moveTo(0, 5); ctx.bezierCurveTo(-6, 0, -4, -5, 0, -2); ctx.bezierCurveTo(4, -5, 6, 0, 0, 5); ctx.fill(); }
    else if (k === "armor") { ctx.moveTo(0, -5); ctx.lineTo(5, -3); ctx.lineTo(5, 1); ctx.quadraticCurveTo(5, 5, 0, 6); ctx.quadraticCurveTo(-5, 5, -5, 1); ctx.lineTo(-5, -3); ctx.closePath(); ctx.stroke(); }
    else if (k === "bspeed") { ctx.moveTo(-4, 1); ctx.lineTo(0, -4); ctx.lineTo(4, 1); ctx.moveTo(-4, 5); ctx.lineTo(0, 0); ctx.lineTo(4, 5); ctx.stroke(); }
    else if (k === "movespeed") { ctx.moveTo(-4, -4); ctx.lineTo(1, 0); ctx.lineTo(-4, 4); ctx.moveTo(1, -4); ctx.lineTo(6, 0); ctx.lineTo(1, 4); ctx.stroke(); }
    else { ctx.arc(-4, 0, 1.5, 0, 6.2832); ctx.fill(); ctx.beginPath(); ctx.arc(0, 0, 1.5, 0, 6.2832); ctx.fill(); ctx.beginPath(); ctx.arc(4, 0, 1.5, 0, 6.2832); ctx.fill(); }
    ctx.restore();
  }
  function drawPow(p, now) {
    const def = POW.find(d => d.k === p.t), bob = Math.sin(now * 0.006 + p.x) * 1.6;
    ctx.save(); ctx.globalAlpha = 0.95; ctx.fillStyle = def.col; roundRect(p.x - 11, p.y - 11 + bob, 22, 22, 6); ctx.fill(); ctx.restore();
    drawPowIcon(p.t, p.x, p.y + bob);
  }
  function drawHud() {
    for (let i = 0; i < player.maxhp; i++) {
      ctx.save(); ctx.translate(W - 16 - i * 17, 16); ctx.fillStyle = i < player.hp ? "#fb7185" : "rgba(148,163,184,.3)";
      ctx.beginPath(); ctx.moveTo(0, 4); ctx.bezierCurveTo(-6, -1, -4, -6, 0, -3); ctx.bezierCurveTo(4, -6, 6, -1, 0, 4); ctx.fill(); ctx.restore();
    }
  }
  function step(now) {
    if (!canvas.isConnected) { cancelAnimationFrame(raf); window.removeEventListener("keydown", kd); window.removeEventListener("keyup", ku); return; }
    if (state === "play") {
      if (keys.left) targetX -= 6; if (keys.right) targetX += 6;
      if (keys.up) targetY -= 6; if (keys.down) targetY += 6;
      targetX = Math.max(player.w / 2, Math.min(W - player.w / 2, targetX));
      targetY = Math.max(24, Math.min(H - 16, targetY));
      player.x += (targetX - player.x) * up.ease;                    // ease toward the finger
      player.y += (targetY - player.y) * up.ease;                    // full 2D follow
      if (now - lastShot > up.fire) fire(now);                       // auto-fire (rate + shots from upgrades)
      bullets.forEach(b => { b.x += b.vx; b.y += b.vy; });
      let lo = W, hi = 0, low = 0, n = 0;
      foes.forEach(f => { if (f.alive) { n++; lo = Math.min(lo, f.x - f.w / 2); hi = Math.max(hi, f.x + f.w / 2); low = Math.max(low, f.y + f.h / 2); } });
      if (n === 0) nextLevel();
      const sp = base + (foes.length - n) * 0.05;
      const edge = (dir > 0 && hi + sp > W - 6) || (dir < 0 && lo - sp < 6);
      foes.forEach(f => { if (f.alive) { if (edge) f.y += 14; else f.x += dir * sp; } });
      if (edge) dir *= -1;
      if (low >= H - 16 && n > 0) state = "over";                    // invasion reached the bottom
      foes.forEach(f => { if (f.alive && Math.abs(f.x - player.x) < f.w / 2 + 10 && Math.abs(f.y - player.y) < f.h / 2 + 8) hurt(now); });   // contact damage (HP/armor + i-frames)
      bullets.forEach(b => { if (b.dead) return; for (const f of foes) { if (f.alive && Math.abs(b.x - f.x) < f.w / 2 && Math.abs(b.y - f.y) < f.h / 2) { f.hp--; boom(f.x, f.y); if (f.hp <= 0) { f.alive = false; score += 10; SFX.play("boom", 0.5); if (pups.length < 3 && Math.random() < 0.06) { pups.push({ x: f.x, y: f.y, t: POW[(Math.random() * POW.length) | 0].k }); SFX.play("drop", 0.4); } } else score += 2; if (b.pierce > 0) b.pierce--; else b.dead = true; break; } } });   // pierce passes through
      bullets = bullets.filter(b => !b.dead && b.y > -16 && b.x > -12 && b.x < W + 12);
      pups.forEach(p => { p.y += 1.9; if (Math.abs(p.x - player.x) < 20 && Math.abs(p.y - player.y) < 20) { p.got = true; const d = POW.find(d => d.k === p.t); if (d) d.apply(); SFX.play("pickup", 0.5); } });
      pups = pups.filter(p => !p.got && p.y < H + 14);
      foes.forEach(f => { if (f.alive && f.shoot) { f.cd -= 16; if (f.cd <= 0) { f.cd = 1400 + Math.random() * 1800; const dx = player.x - f.x, dy = Math.max(20, player.y - f.y), m = Math.hypot(dx, dy), sp2 = 2.4 + level * 0.15; ebullets.push({ x: f.x, y: f.y + 8, vx: dx / m * sp2, vy: dy / m * sp2 }); } } });   // shooters fire at the player
      ebullets.forEach(b => { b.x += b.vx; b.y += b.vy; if (Math.abs(b.x - player.x) < player.w / 2 + 2 && Math.abs(b.y - player.y) < player.h) { b.dead = true; hurt(now); } });
      ebullets = ebullets.filter(b => !b.dead && b.y < H + 12 && b.x > -12 && b.x < W + 12);
    }
    parts.forEach(p => { p.x += p.vx; p.y += p.vy; p.vy += 0.05; p.life -= 0.045; });
    parts = parts.filter(p => p.life > 0);
    ctx.clearRect(0, 0, W, H);
    foes.forEach(f => { if (f.alive) drawFoe(f); });
    pups.forEach(p => drawPow(p, now));
    if (player.armor > 0) { ctx.save(); ctx.globalAlpha = 0.35 + 0.2 * Math.sin(now * 0.008); ctx.strokeStyle = "#60a5fa"; ctx.lineWidth = 2; for (let i = 0; i < player.armor; i++) { ctx.beginPath(); ctx.arc(player.x, player.y - 3, 20 + i * 4, 0, 6.2832); ctx.stroke(); } ctx.restore(); }   // armor shield rings
    if (!(player.inv > now && Math.floor(now / 90) % 2)) drawShip(player.x, player.y, now);   // blink while invulnerable
    ctx.fillStyle = "#fde68a"; bullets.forEach(b => ctx.fillRect(b.x - 1.5, b.y - 7, 3, 9));
    ebullets.forEach(b => {                                          // enemy fire — bright glowing orb, clearly visible
      ctx.globalAlpha = 0.32; ctx.fillStyle = "#fca5a5"; ctx.beginPath(); ctx.arc(b.x, b.y, 8, 0, 6.2832); ctx.fill();
      ctx.globalAlpha = 1; ctx.fillStyle = "#ef4444"; ctx.beginPath(); ctx.arc(b.x, b.y, 4.2, 0, 6.2832); ctx.fill();
      ctx.fillStyle = "#fff"; ctx.beginPath(); ctx.arc(b.x, b.y, 1.7, 0, 6.2832); ctx.fill();
    });
    parts.forEach(p => { ctx.globalAlpha = Math.max(0, p.life); ctx.fillStyle = p.col; ctx.beginPath(); ctx.arc(p.x, p.y, 2.6 * p.life + 0.6, 0, 6.2832); ctx.fill(); });
    ctx.globalAlpha = 1;
    ctx.fillStyle = "#c7d2fe"; ctx.font = "14px system-ui,sans-serif"; ctx.textAlign = "left"; ctx.fillText("Score " + score + "   ·   Level " + level, 52, 22);
    drawHud();
    if (state !== "play") {
      ctx.fillStyle = "rgba(5,6,20,.55)"; ctx.fillRect(0, 0, W, H);
      ctx.textAlign = "center"; ctx.fillStyle = "#fff"; ctx.font = "700 26px system-ui,sans-serif";
      ctx.fillText("Game over · Level " + level, W / 2, H / 2 - 6);
      ctx.font = "14px system-ui,sans-serif"; ctx.fillStyle = "#c7d2fe";
      ctx.fillText("Score " + score + " · tap or space to replay", W / 2, H / 2 + 20);
    }
    raf = requestAnimationFrame(step);
  }
  fit(); reset(); SFX.init(); raf = requestAnimationFrame(step);
}

function renderShell() {
  const acc = App.accounts.find(a => a.id === App.account) || {};
  const nav = el("nav", { class: "nav" },
    visibleServices().map(s => {
      const cnt = App.counts[s.id];
      const connected = cnt != null && cnt > 0;
      const item = el("button", {
        class: "nav-item" + (App.route === s.id ? " active" : ""),
        style: `--svc: var(--svc-${s.id})`,
        dataset: { service: s.id },
        onclick: () => {
          // Re-click on the already-active service collapses/expands its
          // sidebar sub-filters (live.com-style); otherwise navigate.
          if (App.route === s.id) {
            const sn = document.getElementById("subnav-" + s.id);
            if (sn) sn.classList.toggle("collapsed");
          } else go(s.id);
        },
      },
        icon(s.icon),
        el("span", { class: "label", text: s.label }),
        s.id !== "overview" ? el("span", { class: "nav-meta" },
          connected ? el("span", { class: "nav-dot", style: `background:var(--svc-${s.id})`, title: "Connected" }) : null,
          el("span", { class: "count", text: cnt != null ? String(cnt) : "·" })) : null,
      );
      // active service → its filters expand as indented sub-items right here in
      // the LEFT sidebar (no separate rail).
      const specs = App.route === s.id ? svcFilters(s.id) : null;
      const sub = specs ? el("div", { class: "nav-sub", id: `subnav-${s.id}` },
        specs.map(f => f.sec
          ? el("div", { class: "nav-sub-sec", text: f.sec })
          : el("button", { class: "nav-subitem" + (svcFilter(s.id) === f.key ? " active" : ""), dataset: { k: f.key }, onclick: () => setSvcFilter(s.id, f.key) },
            f.color ? el("span", { class: "nav-sub-dot", style: `background:${f.color}` }) : icon(f.icon, "icon-sm"),
            el("span", { class: "grow truncate", text: f.label }),
            el("span", { class: "count tnum", dataset: { cnt: f.key }, text: "·" })))) : null;
      return [item, sub];
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
    el("nav", { class: "nav sys-nav" },
      el("button", { id: "nav-audit", class: "nav-item", title: "Recent runs (audit log)", onclick: () => go("overview") },
        icon("clock"), el("span", { class: "label", text: "Audit log" })),
      el("button", { id: "nav-alerts", class: "nav-item", title: "Failed runs", onclick: () => go("overview") },
        icon("shield"), el("span", { class: "label", text: "Alerts" }),
        el("span", { class: "nav-meta" }, el("span", { id: "alerts-badge", class: "count", text: "·" }))),
      el("button", { id: "nav-settings", class: "nav-item" + (App.route === "settings" ? " active" : ""), title: "Settings", onclick: () => go("settings") },
        icon("settings"), el("span", { class: "label", text: "Settings" })),
      eggOn() ? el("button", { id: "nav-ufo", class: "nav-item" + (App.route === "invaders" ? " active" : ""), title: "Invaders", onclick: () => go("invaders") },
        ufoGlyph(), el("span", { class: "label", text: "Invaders" })) : null),
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

// Sidebar "System health" card: real health, activity sparkline, last-sync,
// sync controls — and it back-fills ALL per-service nav counts up-front + the
// Alerts badge. Every value is real (from /sync/state, /activity, /status).
async function renderSyncWidget() {
  const box = $("#sync-widget"); if (!box) return;
  let st = { enabled: false, paused: false }, runs = [], status = null;
  try { st = await api("/api/v1/sync/state"); } catch {}
  if (App.account) {
    try { runs = (await api("/api/v1/activity?" + qs({ account: App.account, limit: 60 }))).runs || []; } catch {}
    try { status = await api("/api/v1/status?" + qs({ account: App.account })); } catch {}
  }
  // back-fill every service's nav count so the sidebar shows them without
  // visiting each view first (the mockup shows all counts up-front).
  if (status && Array.isArray(status.services)) {
    status.services.forEach(s => { App.counts[s.service] = s.items; });
    updateNavCounts();
  }
  // alerts = failed sync/backup runs (real); reflect in the Alerts nav badge.
  const failed = runs.filter(r => /sync|backup/i.test(r.kind || "") && r.status === "error").length;
  const ab = $("#alerts-badge"); if (ab) ab.textContent = String(failed);
  $("#nav-alerts")?.classList.toggle("has-alerts", failed > 0);
  const last = runs.find(r => /sync|backup/i.test(r.kind || "")) || runs[0];
  const healthy = !failed;
  // daily run volume → sparkline (last 14 days)
  const days = 14, now = Date.now(), buckets = new Array(days).fill(0);
  runs.forEach(r => { const t = toDate(r.finished_at || r.started_at); if (!t) return; const d = Math.floor((now - t.getTime()) / 864e5); if (d >= 0 && d < days) buckets[days - 1 - d]++; });
  clear(box);
  const syncHead = el("div", { class: "sw-head" },
    el("span", { class: "sw-title", text: "System health" }));
  if (st.enabled && CAP.sync) {
    syncHead.append(el("div", { class: "sw-actions" },
      el("button", { onclick: () => syncCmd("now"), title: "Sync now" }, icon("refresh-cw", "icon-sm")),
      st.paused ? el("button", { onclick: () => syncCmd("resume"), title: "Resume" }, icon("play", "icon-sm"))
        : el("button", { onclick: () => syncCmd("pause"), title: "Pause" }, icon("pause", "icon-sm"))));
  }
  const syncNodes = [
    syncHead,
    el("div", { class: "sw-health " + (runs.length ? (healthy ? "ok" : "warn") : "") },
      el("span", { class: "dot" }), el("b", { text: !runs.length ? "Ready" : healthy ? "Healthy" : `${failed} alert${failed > 1 ? "s" : ""}` })),
  ];
  if (runs.length) syncNodes.push(el("div", { class: "sw-spark " + (healthy ? "ok" : "warn") }, sparkline(buckets, 32)));
  syncNodes.push(el("div", { class: "sw-meta dim", text: last ? "Last sync " + fmtDate(last.finished_at) : "No syncs yet" }));
  box.append(...syncNodes);
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
const EXTRA_ROUTES = { search: "Search", settings: "Settings", invaders: "Invaders" };
const routeLabel = (r) => (visibleServices().find(s => s.id === r) || {}).label || EXTRA_ROUTES[r] || "iSyncYou";
function onRoute() {
  // Each navigation rebuilds the view from scratch; the view's render re-registers its
  // own live-update handler (or leaves it null). Reset it here so a stale handler from
  // the previous route can't run against the new DOM (#0A soft refresh).
  App.liveUpdate = null;
  // Preserve the scroll position across a SAME-route re-render (e.g. an SSE
  // "change" tick / a live update) so the user isn't bounced back to the top of
  // a long page mid-scroll. Navigation to a different route starts at the top.
  const prevView = $("#view");
  const prevScroll = prevView ? prevView.scrollTop : 0;
  const prevRoute = App.route;
  const raw = location.hash.replace(/^#\//, "") || "overview";
  App.route = raw.split("?")[0];
  App.query = (raw.split("?")[1] || "").replace(/^q=/, "");
  if (!visibleServices().find(s => s.id === App.route) && !EXTRA_ROUTES[App.route]) App.route = "overview";
  if (App.route === "invaders" && !eggOn()) App.route = "overview";   // egg-gated route
  // Close any open overlay (detail sheet / command palette) on a real navigation
  // so it can't leak across routes; a same-route refresh keeps it open.
  if (App.route !== prevRoute) {
    closeSheet(); closePalette();
    // Leaving the assistant: close any live token stream so it can't leak across routes.
    if (prevRoute === "assistant") closeAssistantStream("route-exit");
  }
  renderShell();
  const view = $("#view");
  let p;
  if (App.route === "overview") p = renderOverview(view);
  else if (App.route === "mail") p = renderMailView(view);
  else if (App.route === "onedrive") p = renderOnedriveView(view);
  else if (App.route === "calendar") p = renderCalendarView(view);
  else if (App.route === "contacts") p = renderContactsView(view);
  else if (App.route === "todo") p = renderTodoView(view);
  else if (App.route === "onenote") p = renderOnenoteView(view);
  else if (App.route === "search") p = renderSearchView(view);
  else if (App.route === "settings") p = renderSettingsView(view);
  else if (App.route === "assistant") p = renderAssistantView(view);
  else if (App.route === "invaders") p = renderInvaders(view);
  else p = renderServiceView(view, App.route);
  if (prevScroll && App.route === prevRoute) {
    const restore = () => { const v = $("#view"); if (v && v.scrollHeight > v.clientHeight) v.scrollTop = prevScroll; };
    if (p && typeof p.then === "function") p.then(restore); else restore();
  }
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
async function renderOverview(view) {
  clear(view).append(
    el("h1", { class: "view-title", text: MOBILE ? "Microsoft 365 on this device" : "Microsoft 365 archive overview" }),
    el("p", { class: "view-sub", text: MOBILE ? "Your live view, cached on this device. Backups live on your computer." : "Backup health, activity and connected services at a glance." }),
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
        el("span", { class: "chip " + (healthy ? "ok" : "warn") }, el("span", { class: "dot" }), healthy ? (MOBILE ? "Connected" : "Archive healthy") : "Attention needed")),
      el("div", { class: "status-facts" },
        el("span", {}, el("b", { text: String(services.length) }), " services connected"),
        el("span", { class: "sep", text: "·" }),
        el("span", {}, "Last sync ", el("b", { text: lastRun ? fmtFullDate(lastRun.finished_at) : "never" })),
        el("span", { class: "sep", text: "·" }),
        el("span", {}, el("b", { text: String(failed) }), failed === 1 ? " failed run" : " failed runs"),
        el("span", { class: "sep", text: "·" }),
        el("span", {}, el("b", { text: items.toLocaleString() }), MOBILE ? " items cached" : " items protected")),
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
      kpiCard("layout-dashboard", MOBILE ? "Items cached" : "Items protected", items.toLocaleString(), "", el("div", { class: "kpi-ctx", text: `across ${services.length} services` })),
      kpiCard("download", MOBILE ? "Cached bodies" : "Archived bodies", archived.toLocaleString(), "", el("div", { class: "kpi-ctx", text: items ? `${Math.round(archived / items * 100)}% have content` : "—" })),
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
    // Live-refresh the dashboard when a sync tick actually changes the counts (fixes the
    // stale "0 items" Overview on mobile, where the initial render happens before the first
    // sync populates data). Repaints only on a real change — never per tick.
    App._ovSig = overviewSignature(st);
    App.liveUpdate = overviewLiveUpdate;
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load overview" }), el("p", { text: e.message }))); }
}
function overviewSignature(st) {
  const svc = (st.services || []).map(s => s.service + ":" + s.items).join(",");
  return (st.totals?.items ?? 0) + "/" + (st.totals?.archived ?? 0) + "/" + svc;
}
async function overviewLiveUpdate() {
  let st;
  try { st = await api("/api/v1/status?" + qs({ account: App.account })); }
  catch (_) { return; }
  if (overviewSignature(st) === App._ovSig) return; // counts unchanged → no repaint
  const view = $("#view"), vsc = view ? view.scrollTop : 0;
  await renderOverview($("#view")); // re-renders; re-arms App._ovSig + App.liveUpdate
  const v2 = $("#view"); if (v2) v2.scrollTop = vsc;
}
function connItem(k, v) { return el("div", { class: "conn-item" }, el("dt", { text: k }), el("dd", { text: v == null ? "—" : String(v) })); }

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
  list.dataset.filter = "all";
  const more = el("div", { id: "svc-more", style: "display:none;padding:14px;text-align:center" },
    el("button", { class: "btn ghost", onclick: () => loadMore(service) }, "Load more"));
  view.append(stateChipBar(), list, more);
  view._offset = 0; view._total = 0;
  await loadPage(service, true);
}
// shared 4-state filter bar (#560): CSS-driven via #svc-list[data-filter] +
// .list-row[data-state], so it keeps working across paginated "Load more".
function stateChipBar() {
  const mk = (key, label) => el("button", { class: "state-chip" + (key === "all" ? " active" : ""), dataset: { k: key }, onclick: () => applyStateFilter(key) },
    key === "all" ? null : icon(STATES[key].icon, "icon-sm"),
    el("span", { text: label }), el("span", { class: "sc-count", dataset: { cnt: key }, text: "0" }));
  return el("div", { id: "svc-statechips", class: "state-chips" },
    mk("all", "All"), mk("live_only", STATES.live_only.label), mk("live_backup", STATES.live_backup.label), mk("backup_only", STATES.backup_only.label), mk("stale", STATES.stale.label));
}
function applyStateFilter(key) {
  const list = $("#svc-list"); if (!list) return;
  list.dataset.filter = key;
  document.querySelectorAll("#svc-statechips .state-chip").forEach(b => b.classList.toggle("active", b.dataset.k === key));
  const visible = key === "all" ? list.querySelectorAll(".list-row").length : list.querySelectorAll(`.list-row[data-state="${key}"]`).length;
  let hint = $("#svc-filter-empty");
  if (!visible && list.querySelector(".list-row")) {
    if (!hint) list.append(el("div", { id: "svc-filter-empty", class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No matches" }), el("p", { text: "No loaded items have this status." })));
  } else if (hint) hint.remove();
}
function updateStateChipCounts() {
  const list = $("#svc-list"); if (!list) return;
  const rows = [...list.querySelectorAll(".list-row[data-state]")];
  const set = (k, n) => { const c = document.querySelector(`#svc-statechips [data-cnt="${k}"]`); if (c) c.textContent = String(n); };
  set("all", rows.length);
  for (const k of STATE_KEYS) set(k, rows.filter(r => r.dataset.state === k).length);
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
  row.dataset.state = stateKey(it);
  const actions = el("div", { style: "display:flex;gap:6px;align-items:center" });
  actions.append(coverageBadge(it));
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
    updateStateChipCounts(); applyStateFilter(list.dataset.filter || "all");
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
// Real Outlook categories (#563): the master-category colour map (preset0..24)
// is built on load from the backed-up `category` items (#562) — no keyword
// heuristic. Each message carries its real `categories` list in the preview.
const PRESET_COLORS = {
  preset0: "#e74c3c", preset1: "#e67e22", preset2: "#8d6e63", preset3: "#f1c40f", preset4: "#2ecc71",
  preset5: "#1abc9c", preset6: "#9aa700", preset7: "#3498db", preset8: "#9b59b6", preset9: "#e84393",
  preset10: "#95a5a6", preset11: "#607d8b", preset12: "#b2bec3", preset13: "#7f8c8d", preset14: "#2c3e50",
  preset15: "#c0392b", preset16: "#d35400", preset17: "#5d4037", preset18: "#f39c12", preset19: "#27ae60",
  preset20: "#16a085", preset21: "#6b7a00", preset22: "#2980b9", preset23: "#8e44ad", preset24: "#ad1457",
};
const presetColor = (preset) => PRESET_COLORS[preset] || "var(--text-lo)";
const categoryColor = (name) => (Mail.catColor && Mail.catColor.get(name)) || "var(--text-lo)";
// One chip per real category (colour from the master-category map). Empty → [].
function categoryChips(it) {
  return ((it.preview || {}).categories || [])
    .map(name => el("span", { class: "mi-cat", style: `--c:${categoryColor(name)}`, text: name }));
}
// Avatar tint: the first category's colour, else the mail service colour.
// Deterministic hue (0–359) from a key → a stable colour per identity. The same
// key always yields the same hue, so identical senders share a colour and distinct
// senders get distinct ones — no state to track.
function hashHue(key) {
  let h = 0;
  const s = (key || "").toLowerCase();
  for (let i = 0; i < s.length; i++) h = (Math.imul(h, 31) + s.charCodeAt(i)) | 0;
  return ((h % 360) + 360) % 360;
}
// Each sender gets its own colour, derived from its address so it's stable across
// the list and sessions (identical sender ⇒ identical colour).
function mailAvatarColor(it) {
  const p = it.preview || {};
  const a = parseAddr(p.from || "");
  const key = a.email || a.name || it.name || p.subject || "";
  return `hsl(${hashHue(key)} 58% 56%)`;
}
const mailDate = (it) => { const p = it.preview || {}; return toDate(p.date || it.remote_mtime) || new Date(0); };

async function renderMailView(view) {
  Mail.all = []; Mail.filter = "all"; Mail.sort = Mail.sort || "newest"; Mail.q = ""; Mail.selected = null;
  Mail.threaded = Mail.threaded === undefined ? true : Mail.threaded; // group by conversation (#563)
  clear(view).append(el("div", { id: "mail-page", class: "mail-page" },
    // top metric row leads (title is in the top-bar breadcrumb, counts live in the
    // cards + sidebar sub-nav) — no separate hero band, matching the mockup
    el("div", { id: "mail-metrics-row", class: "con-metrics-row top" }),
    // toolbar
    el("div", { class: "mail-toolbar" },
      el("div", { class: "tb-search" }, icon("search", "icon-sm"),
        el("input", { id: "mail-search", placeholder: "Search this mailbox…", oninput: () => { clearTimeout(Mail._qT); Mail._qT = setTimeout(() => { Mail.q = $("#mail-search").value.trim().toLowerCase(); mailRender(); }, 140); } })),
      el("div", { class: "spacer", style: "flex:1" }),
      el("label", { class: "tb-sort" }, icon("arrow-down-up", "icon-sm"),
        el("select", { class: "input", onchange: (e) => { Mail.sort = e.target.value; mailRender(); } },
          el("option", { value: "newest", text: "Newest first" }),
          el("option", { value: "oldest", text: "Oldest first" }),
          el("option", { value: "sender", text: "Sender A–Z" }))),
      el("button", { id: "mail-thread-toggle", class: "btn sm" + (Mail.threaded ? " active" : ""), title: "Group messages into conversations", onclick: (e) => { Mail.threaded = !Mail.threaded; e.currentTarget.classList.toggle("active", Mail.threaded); mailRender(); } }, icon("mail-open", "icon-sm"), "Conversations"),
      verifyButton(() => renderMailView(view)),
      CAP.mailwrite ? el("button", { class: "btn sm primary", title: "Compose a new message", onclick: () => openCompose() }, icon("send", "icon-sm"), "Compose") : null,
      el("button", { class: "btn sm", title: "View sync log", onclick: () => go("overview") }, icon("clock", "icon-sm"), "Sync log")),
    // 2-pane: list | reader (filters live in the left sidebar under "Mail")
    el("div", { id: "mail-layout", class: "mail-layout" },
      el("div", { id: "mail-list", class: "mail-list" }),
      el("div", { id: "mail-reader", class: "mail-reader" }))));
  renderMailReader(null);
  const list = $("#mail-list");
  for (let i = 0; i < 9; i++) list.append(el("div", { class: "mail-item skel-row" }, el("div", { class: "skel grow", style: "height:46px" })));
  try {
    const [d, act] = await Promise.all([
      api("/api/v1/items?" + qs({ account: App.account, service: "mail", limit: 1000 })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 30 })).catch(() => ({ runs: [] })),
    ]);
    Mail.all = (d.items || []).filter(it => it.item_type === "message");
    // real Outlook categories (#562): build the displayName → colour map
    Mail.cats = (d.items || []).filter(it => it.item_type === "category");
    Mail.catColor = new Map(Mail.cats.map(c => [c.name, presetColor((c.preview || {}).color)]));
    Mail.folders = (d.items || []).filter(it => it.item_type === "folder"); // move targets (#563 B5)
    Mail.runs = act.runs || [];
    App.counts.mail = Mail.all.length; updateNavCounts();
    refreshMailSubnav(); // rebuild the sidebar now that the real categories are known
    fillSubnavCounts("mail", Mail.all);
    mailRenderMetrics(); mailRender();
    // Register the soft live-update for background sync ticks (#0A): re-fetch + patch the
    // list in place (no teardown, no filter/scroll reset), repainting only when the
    // visible set actually changed.
    Mail._sig = mailListSignature();
    App.liveUpdate = mailLiveUpdate;
    // re-open the message that was selected before a live (SSE) refresh, if it survived
    if (Mail.pendingSelect) {
      const keep = Mail.all.find(x => x.remote_id === Mail.pendingSelect);
      Mail.pendingSelect = null;
      if (keep) mailSelect(keep);
    }
  } catch (e) { clear(list).append(el("div", { class: "empty" }, el("h3", { text: "Could not load mail" }), el("p", { text: e.message }))); }
}

// Signature of the currently *visible* mailbox (ids + read state, honouring the active
// filter/search/sort) — lets a background refresh detect whether anything on screen
// actually changed before repainting (#0A).
function mailListSignature() {
  return mailFiltered().map(it => it.remote_id + (((it.preview || {}).isRead) ? "1" : "0")).join(",");
}
// Soft background refresh for the mailbox (#0A): re-fetch, update counts, and repaint the
// list ONLY when the visible set changed — preserving filter/search/sort/scroll/selection
// so a sync tick never reloads the screen.
async function mailLiveUpdate() {
  let d;
  try { d = await api("/api/v1/items?" + qs({ account: App.account, service: "mail", limit: 1000 })); }
  catch (_) { return; }
  Mail.all = (d.items || []).filter(it => it.item_type === "message");
  App.counts.mail = Mail.all.length; updateNavCounts(); fillSubnavCounts("mail", Mail.all);
  const sig = mailListSignature();
  if (sig === Mail._sig) { mailRenderMetrics(); return; } // nothing on screen changed → no repaint
  Mail._sig = sig;
  const list = $("#mail-list"), view = $("#view");
  const lsc = list ? list.scrollTop : 0, vsc = view ? view.scrollTop : 0;
  mailRenderMetrics(); mailRender();
  const l2 = $("#mail-list"); if (l2) l2.scrollTop = lsc;
  const v2 = $("#view"); if (v2) v2.scrollTop = vsc;
}
// fill an existing .con-metrics-row container in place from card specs
function fillMetrics(row, cards) {
  if (!row) return; clear(row);
  row.style.gridTemplateColumns = `repeat(${cards.length}, 1fr)`;
  cards.forEach(c => row.append(conMetric(c.icon, c.value, c.label, c.sub, c.tone)));
}
function mailRenderMetrics() {
  const withAtt = Mail.all.filter(it => (it.preview || {}).attachments > 0).length;
  const restore = Mail.all.filter(it => it.has_body).length;
  fillMetrics($("#mail-metrics-row"), [
    { icon: "inbox", value: Mail.all.length, label: "Total messages", sub: "in this mailbox" },
    { icon: "paperclip", value: withAtt, label: "With attachments", sub: `${withAtt} of ${Mail.all.length}` },
    { icon: "rotate-ccw", value: restore, label: (MOBILE ? "Has content" : "Restore-ready"), sub: `${restore} with full body`, tone: "ok" },
    integrityMetric(Mail.all),
    lastActivityMetric(Mail.runs),
  ]);
}
function mailFiltered() {
  let rows = Mail.all;
  const f = svcFilter("mail");
  if (f === "attach") rows = rows.filter(it => (it.preview || {}).attachments > 0);
  else if (f === "restore") rows = rows.filter(it => it.has_body);
  else if (STATE_KEYS.has(f)) rows = rows.filter(it => stateKey(it) === f);
  else if (f === "unread") rows = rows.filter(it => (it.preview || {}).isRead === false);
  else if (f === "flagged") rows = rows.filter(it => (it.preview || {}).flag === "flagged");
  else if (f === "high") rows = rows.filter(it => (it.preview || {}).importance === "high");
  else if (f.startsWith("folder:")) { const fid = f.slice(7); rows = rows.filter(it => it.parent_remote_id === fid); }
  else if (f !== "all") rows = rows.filter(it => ((it.preview || {}).categories || []).includes(f));
  if (Mail.q) rows = rows.filter(it => { const p = it.preview || {}; return ((p.subject || it.name || "") + " " + (p.from || "") + " " + (p.snippet || "")).toLowerCase().includes(Mail.q); });
  const dir = Mail.sort === "oldest" ? 1 : -1;
  if (Mail.sort === "sender") rows = rows.slice().sort((a, b) => addrLabel((a.preview || {}).from).localeCompare(addrLabel((b.preview || {}).from)));
  else rows = rows.slice().sort((a, b) => dir * (mailDate(a) - mailDate(b)));
  return rows;
}
function mailRender() {
  const list = $("#mail-list"); if (!list) return;
  const rows = mailFiltered();
  const withAtt = Mail.all.filter(it => (it.preview || {}).attachments > 0).length;
  const archived = Mail.all.filter(it => it.has_body).length;
  const m = $("#mail-metrics"); if (m) m.textContent = `${Mail.all.length} messages · ${archived} with content · ${withAtt} with attachments`;
  clear(list);
  // Bump the render generation on every (re-)render so any in-flight progressive
  // batch from a previous render — including one interrupted by an early return
  // below — stops before touching this freshly cleared list.
  const gen = (Mail._renderGen = (Mail._renderGen || 0) + 1);
  if (!Mail.all.length) { list.append(el("div", { class: "empty" }, emptyArt("empty-mail"), el("h3", { text: "No mail archived" }), el("p", { text: "Run a backup to populate your mailbox." }))); return; }
  if (!rows.length) { list.append(el("div", { class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No matches" }), el("p", { text: "Adjust the filter or search." }))); return; }
  // Conversation grouping (#563): collapse messages sharing a conversationId into
  // one row (the newest, since rows are already sorted) carrying a thread count of
  // how many of the *currently listed* messages belong to it.
  let display;
  if (Mail.threaded) {
    const seen = new Map(); display = [];
    rows.forEach(it => {
      const cid = (it.preview || {}).conversationId;
      if (cid && seen.has(cid)) { seen.get(cid).n++; return; }
      const entry = { it, n: 1 }; if (cid) seen.set(cid, entry); display.push(entry);
    });
  } else {
    display = rows.map(it => ({ it, n: 1 }));
  }
  // Progressive render: fill the viewport instantly, then append the rest in
  // requestAnimationFrame batches so building the (potentially hundreds of) rows
  // never blocks the main thread — the mailbox appears immediately instead of the
  // UI freezing on a synchronous full build. A generation token cancels an in-flight
  // render when the list is re-rendered (search/filter/sort/thread/SSE refresh), so
  // stale batches never leak into a newer list. Every row is still rendered, in order
  // — no feature lost; only the *timing* of the off-screen rows changes.
  const FIRST = 24, BATCH = 40;
  const paint = (from, to) => {
    const frag = document.createDocumentFragment();
    for (let i = from; i < to && i < display.length; i++) frag.append(mailRow(display[i].it, display[i].n));
    list.append(frag);
  };
  paint(0, FIRST);
  if (display.length > FIRST) {
    let i = FIRST;
    const step = () => {
      if (Mail._renderGen !== gen || !list.isConnected) return; // superseded by a newer render
      paint(i, i + BATCH); i += BATCH;
      if (i < display.length) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }
}
function mailRow(it, threadCount = 1) {
  const p = it.preview || {};
  const from = addrLabel(p.from), subject = p.subject || it.name || "(no subject)";
  const sel = Mail.selected && Mail.selected.remote_id === it.remote_id;
  const badges = el("div", { class: "mi-badges" });
  if (threadCount > 1) badges.append(el("span", { class: "mi-chip mi-thread", title: threadCount + " messages in this conversation" }, icon("mail-open", "icon-sm"), String(threadCount)));
  if (p.attachments > 0) badges.append(el("span", { class: "mi-chip", title: p.attachments + " attachment(s)" }, icon("paperclip", "icon-sm"), String(p.attachments)));
  categoryChips(it).forEach(c => badges.append(c));
  if (CAP.mailwrite) {
    const flagged = p.flag === "flagged";
    badges.append(el("button", { class: "mi-flag" + (flagged ? " on" : ""), title: flagged ? "Clear flag" : "Flag",
      onclick: (e) => { e.stopPropagation(); mailSetFlag(it, flagged ? "notFlagged" : "flagged"); } }, icon("flag", "icon-sm")));
  }
  return el("button", { class: "mail-item" + (sel ? " active" : "") + (p.isRead === false ? " unread" : ""), dataset: { id: it.remote_id }, onclick: () => mailSelect(it) },
    el("span", { class: "avatar mail-av", style: `--c:${mailAvatarColor(it)}`, text: initials(from || subject) }),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "mi-top" }, el("span", { class: "mi-from truncate", text: from || "(unknown sender)" }),
        el("span", { class: "mi-date dim tnum", text: fmtDate(p.date || it.remote_mtime) })),
      el("div", { class: "mi-subject truncate", text: subject }),
      el("div", { class: "mi-bottom" },
        el("span", { class: "mi-snippet truncate dim", text: p.snippet || "" }),
        coverageBadge(it)),
      badges));
}
function mailSelect(it) {
  Mail.selected = it;
  document.querySelectorAll(".mail-item").forEach(r => r.classList.toggle("active", r.dataset.id === it.remote_id));
  $("#mail-layout")?.classList.add("reading");
  renderMailReader(it);
}
function mailBack() { Mail.selected = null; $("#mail-layout")?.classList.remove("reading"); document.querySelectorAll(".mail-item.active").forEach(r => r.classList.remove("active")); renderMailReader(null); }

// Compose / reply / forward share this sheet (#563). `opts` { title, to, cc,
// subject, bodyHtml } pre-fills it (reply/forward, B4). The body is a
// contenteditable the user authors and which is sent to Graph — it is never
// rendered as untrusted, so editing HTML here carries no XSS risk.
function openCompose(opts = {}) {
  if (!CAP.mailwrite) return;
  const o = opts || {};
  const field = (label, input) => el("label", { class: "cmp-field" }, el("span", { class: "cmp-label", text: label }), input);
  const toIn = el("input", { class: "input", id: "cmp-to", placeholder: "name@example.com, …", value: (o.to || []).join(", ") });
  const ccIn = el("input", { class: "input", id: "cmp-cc", placeholder: "Cc", value: (o.cc || []).join(", ") });
  const bccIn = el("input", { class: "input", id: "cmp-bcc", placeholder: "Bcc" });
  const subjIn = el("input", { class: "input", id: "cmp-subject", placeholder: "Subject", value: o.subject || "" });
  const bodyEl = el("div", { class: "cmp-body", id: "cmp-body", contenteditable: "true" });
  if (o.bodyHtml) bodyEl.innerHTML = o.bodyHtml; // our own quoted-reply markup (trusted)
  const ccRow = el("div", { class: "cmp-ccbcc", style: "display:none" }, field("Cc", ccIn), field("Bcc", bccIn));
  const ccToggle = el("button", { class: "btn ghost sm", type: "button", title: "Show Cc/Bcc", onclick: () => { ccRow.style.display = ccRow.style.display === "none" ? "grid" : "none"; } }, "Cc/Bcc");
  const impSel = el("select", { class: "input cmp-imp", id: "cmp-importance" },
    el("option", { value: "normal", text: "Normal" }),
    el("option", { value: "high", text: "High importance" }),
    el("option", { value: "low", text: "Low importance" }));
  const rrChk = el("input", { type: "checkbox", id: "cmp-rr" });
  const content = el("div", { class: "compose" },
    field("To", el("div", { class: "cmp-to-row" }, toIn, ccToggle)),
    ccRow,
    field("Subject", subjIn),
    bodyEl,
    el("div", { class: "cmp-footer" },
      el("label", { class: "cmp-opt", title: "Importance" }, icon("flag", "icon-sm"), impSel),
      el("label", { class: "cmp-opt" }, rrChk, el("span", { text: "Read receipt" })),
      el("div", { class: "spacer", style: "flex:1" }),
      el("button", { class: "btn ghost", type: "button", onclick: (e) => composeSubmit(e.currentTarget, true) }, icon("archive", "icon-sm"), "Save draft"),
      el("button", { class: "btn primary", type: "button", onclick: (e) => composeSubmit(e.currentTarget, false) }, icon("send", "icon-sm"), "Send")));
  openSheet(o.title || "New message", content);
  setTimeout(() => ((o.to && o.to.length) ? bodyEl : toIn).focus(), 60);
}

async function composeSubmit(btn, asDraft) {
  const to = ($("#cmp-to").value || "").trim();
  const cc = ($("#cmp-cc").value || "").trim();
  const bcc = ($("#cmp-bcc").value || "").trim();
  const subject = ($("#cmp-subject").value || "").trim();
  const body = $("#cmp-body").innerHTML || "";
  const importance = $("#cmp-importance").value;
  const rr = $("#cmp-rr").checked;
  if (!asDraft && !to) { toast("Add at least one recipient", "err"); return; }
  btn.disabled = true;
  try {
    if (asDraft) {
      await post("/api/v1/mail/draft?" + qs({ account: App.account, to, subject, body }), CAP.mailwrite);
      toast("Draft saved");
    } else {
      const params = { account: App.account, to, cc, bcc, subject, body };
      if (importance && importance !== "normal") params.importance = importance;
      if (rr) params.read_receipt = "1";
      await post("/api/v1/mail/send?" + qs(params), CAP.mailwrite);
      toast("Message sent");
    }
    closeSheet();
    if (App.route === "mail") renderMailView($("#view"));
  } catch (e) {
    toast((asDraft ? "Draft failed: " : "Send failed: ") + e.message, "err");
  } finally {
    btn.disabled = false;
  }
}

// Inline rich reply/forward (#67, live.com-style): the composer takes over the
// reader pane — a format toolbar + contenteditable for the user's NEW content on
// top, the original message shown read-only (sandboxed iframe) below. Only the new
// content is sent; the daemon prepends it above the quoted original (Mail-1), so
// the URL stays small and the full original is preserved + threaded.
function openInlineComposer(it, mode) {
  if (!CAP.mailwrite) return;
  const box = $("#mail-reader"); if (!box) return;
  const p = it.preview || {};
  const isFwd = mode === "forward";
  const title = isFwd ? "Forward" : mode === "replyAll" ? "Reply all" : "Reply";
  const editor = el("div", { class: "cmp-editor", id: "cmp-editor", contenteditable: "true" });
  editor.innerHTML = "<p><br></p>";
  const toIn = isFwd ? el("input", { class: "input", id: "cmp-fwd-to", placeholder: "To — comma-separated email addresses" }) : null;
  const tbBtn = (cmd, ic, ttl, arg) => el("button", {
    class: "cmp-tb-btn", type: "button", title: ttl,
    onmousedown: (e) => { e.preventDefault(); document.execCommand(cmd, false, arg); editor.focus(); },
  }, icon(ic, "icon-sm"));
  const toolbar = el("div", { class: "cmp-toolbar" },
    tbBtn("bold", "bold", "Bold"), tbBtn("italic", "italic", "Italic"), tbBtn("underline", "underline", "Underline"),
    el("span", { class: "cmp-tb-sep" }),
    tbBtn("insertUnorderedList", "list", "Bulleted list"), tbBtn("insertOrderedList", "list-ordered", "Numbered list"),
    el("button", {
      class: "cmp-tb-btn", type: "button", title: "Insert link",
      onmousedown: (e) => { e.preventDefault(); const u = prompt("Link URL:"); if (u) document.execCommand("createLink", false, u); editor.focus(); },
    }, icon("link", "icon-sm")));
  const q = { account: App.account, service: "mail", id: it.remote_id };
  const head = el("div", { class: "cmp-inline-head" },
    el("span", { class: "cmp-inline-title truncate" }, title + (p.subject ? " · " + p.subject : (it.name ? " · " + it.name : ""))),
    el("div", { style: "flex:1" }),
    el("button", { class: "btn ghost sm", type: "button", onclick: () => renderMailReader(it) }, "Discard"),
    el("button", { class: "btn primary sm", id: "cmp-send", type: "button", onclick: (e) => inlineComposerSend(e.currentTarget, it, mode) },
      icon(isFwd ? "corner-up-right" : "send", "icon-sm"), isFwd ? "Forward" : "Send"));
  const scroller = el("div", { class: "cmp-inline-scroll" });
  if (toIn) scroller.append(el("label", { class: "cmp-inline-to" }, el("span", { class: "cmp-label", text: "To" }), toIn));
  scroller.append(toolbar, editor,
    el("div", { class: "cmp-quote-label dim" }, `Original from ${addrLabel(p.from) || "sender"} · included below your ${isFwd ? "forward" : "reply"}`),
    quoteFrame);
  clear(box).append(head, scroller);
  setTimeout(() => editor.focus(), 50);
}
async function inlineComposerSend(btn, it, mode) {
  const editor = $("#cmp-editor"); if (!editor) return;
  const body = (editor.innerHTML || "").trim();
  const params = { account: App.account, id: it.remote_id, body };
  let path = "/api/v1/mail/reply";
  if (mode === "forward") {
    const to = ($("#cmp-fwd-to").value || "").trim();
    if (!to) { toast("Add at least one recipient", "err"); return; }
    params.to = to; path = "/api/v1/mail/forward";
  } else {
    params.all = mode === "replyAll" ? "1" : "0";
  }
  btn.disabled = true;
  try {
    await post(path + "?" + qs(params), CAP.mailwrite);
    toast(mode === "forward" ? "Forwarded" : "Reply sent");
    renderMailReader(it);
  } catch (e) { toast("Send failed: " + e.message, "err"); btn.disabled = false; }
}

/* ---- per-message manage (#563 B5): optimistic local update, reconcile on the
   server's SSE notify (B1); revert + toast on failure. ---- */
function mailRerender(it) {
  mailRender();
  if (Mail.selected && Mail.selected.remote_id === it.remote_id) renderMailReader(it);
}
async function mailManage(it, optimistic, revert, path) {
  optimistic(it.preview = it.preview || {});
  mailRerender(it);
  try {
    await post(path, CAP.mailwrite);
  } catch (e) {
    revert(it.preview);
    mailRerender(it);
    toast("Action failed: " + e.message, "err");
  }
}
const mailSetRead = (it, isRead) => mailManage(
  it, p => { p.isRead = isRead; }, () => { (it.preview || {}).isRead = !isRead; },
  "/api/v1/mail/read?" + qs({ account: App.account, id: it.remote_id, is_read: isRead ? "1" : "0" }));
const mailSetFlag = (it, status, due) => {
  const prev = (it.preview || {}).flag;
  const q = { account: App.account, id: it.remote_id, status };
  if (due) { q.due = due; q.tz = (Intl.DateTimeFormat().resolvedOptions().timeZone) || "UTC"; }
  return mailManage(it, p => { p.flag = status; }, p => { p.flag = prev; }, "/api/v1/mail/flag?" + qs(q));
};
// inline-animated follow-up menu (no popup): plain flag · due date · complete · clear
function openFlagMenu(it, btn) {
  const existing = document.getElementById("flag-menu");
  if (existing) { existing.remove(); return; }                       // toggle off
  const cur = (it.preview || {}).flag, di = el("input", { type: "date", class: "flag-due" });
  const close = () => { const p = document.getElementById("flag-menu"); if (p) p.remove(); };
  const panel = el("div", { id: "flag-menu", class: "flag-menu" },
    el("button", { class: "btn ghost sm", onclick: () => { mailSetFlag(it, "flagged"); close(); } }, icon("flag", "icon-sm"), "Flag"),
    el("span", { class: "flag-due-wrap" }, el("span", { class: "dim", text: "Due" }), di,
      el("button", { class: "btn sm", onclick: () => { if (di.value) { mailSetFlag(it, "flagged", di.value + "T09:00:00"); close(); } else di.focus(); } }, "Set")),
    el("button", { class: "btn ghost sm", onclick: () => { mailSetFlag(it, "complete"); close(); } }, "Complete"),
    (cur && cur !== "notFlagged") ? el("button", { class: "btn ghost sm", onclick: () => { mailSetFlag(it, "notFlagged"); close(); } }, "Clear") : null,
  );
  (btn.closest(".mail-reader-head") || btn.parentElement).after(panel);   // full-width block below the header, not squeezed into the action row
  requestAnimationFrame(() => panel.classList.add("open"));
}
const mailSetCategories = (it, cats) => { const prev = (it.preview || {}).categories; return mailManage(
  it, p => { p.categories = cats; }, p => { p.categories = prev; },
  "/api/v1/mail/categories?" + qs({ account: App.account, id: it.remote_id, categories: cats.join(",") })); };

// Move changes the message id, so optimistically drop it from the current list;
// the SSE refresh (B1) brings the authoritative state.
async function mailMove(it, destination, label) {
  try {
    await post("/api/v1/mail/move?" + qs({ account: App.account, id: it.remote_id, destination }), CAP.mailwrite);
    toast(`Moved to ${label}`);
    Mail.all = Mail.all.filter(x => x.remote_id !== it.remote_id);
    if (Mail.selected && Mail.selected.remote_id === it.remote_id) mailBack();
    mailRender();
  } catch (e) { toast("Move failed: " + e.message, "err"); }
}
function mailDelete(it) {
  if (!confirm("Move this message to Deleted Items?")) return;
  mailMove(it, "deleteditems", "Deleted Items");
}

function openCategoryPicker(it) {
  if (!CAP.mailwrite) return;
  const cur = new Set((it.preview || {}).categories || []);
  const boxes = (Mail.cats || []).map(c => {
    const cb = el("input", { type: "checkbox", value: c.name });
    if (cur.has(c.name)) cb.checked = true;
    return el("label", { class: "pick-row" }, cb,
      el("span", { class: "nav-sub-dot", style: `background:${presetColor((c.preview || {}).color)}` }),
      el("span", { class: "grow truncate", text: c.name }));
  });
  const content = el("div", { class: "compose" },
    boxes.length ? el("div", { class: "pick-list" }, boxes) : el("p", { class: "dim", text: "No categories defined in this mailbox." }),
    el("div", { class: "cmp-footer" }, el("div", { class: "spacer", style: "flex:1" }),
      el("button", { class: "btn primary", type: "button", onclick: () => {
        const sel = boxes.map(b => b.querySelector("input")).filter(i => i.checked).map(i => i.value);
        closeSheet(); mailSetCategories(it, sel);
      } }, "Apply")));
  openSheet("Categories", content);
}

function openMovePicker(it) {
  if (!CAP.mailwrite) return;
  const folders = (Mail.folders || []).slice().sort((a, b) => (a.name || "").localeCompare(b.name || ""));
  const rows = folders.map(f => el("button", { class: "pick-row pick-btn", type: "button", onclick: () => { closeSheet(); mailMove(it, f.remote_id, f.name); } },
    icon("folder", "icon-sm"), el("span", { class: "grow truncate", text: f.name || "(folder)" })));
  const content = el("div", { class: "compose" },
    rows.length ? el("div", { class: "pick-list" }, rows) : el("p", { class: "dim", text: "No folders found." }));
  openSheet("Move to folder", content);
}
// restrained archive-vault illustration for the empty reading pane (trusted in-code SVG)
const VAULT_SVG = '<svg viewBox="0 0 260 180" xmlns="http://www.w3.org/2000/svg"><g fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="40" y="34" width="120" height="84" rx="10" opacity="0.35" transform="rotate(-7 100 76)"/><rect x="64" y="44" width="120" height="84" rx="10" opacity="0.6"/><path d="M64 60h120" opacity="0.6"/><circle cx="200" cy="120" r="38" fill="color-mix(in oklab, var(--accent) 10%, transparent)"/><circle cx="200" cy="120" r="38"/><circle cx="200" cy="120" r="14"/><path d="M200 92v10M200 138v10M172 120h10M218 120h10"/></g><path d="M191 119l6 6 12-13" fill="none" stroke="var(--accent)" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';
function renderMailReader(it, remoteImages = false) {
  const box = $("#mail-reader"); if (!box) return; clear(box);
  if (!it) {
    const archived = Mail.all.filter(x => x.has_body).length;
    const withAtt = Mail.all.filter(x => (x.preview || {}).attachments > 0).length;
    box.append(el("div", { class: "mail-flow", html: `<div class="con-flow-wave">${flowWave(680, 360)}</div>` }));
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
  const p = it.preview || {}, from = parseAddr(p.from);
  const subject = p.subject || it.name || "(no subject)", when = p.date || it.remote_mtime;
  const q = { account: App.account, service: "mail", id: it.remote_id };
  const viewQ = remoteImages ? { ...q, external: "1" } : q;
  const actions = el("div", { class: "mr-actions" });
  if (CAP.mailwrite) {
    actions.append(
      el("button", { class: "btn primary sm", title: "Reply", onclick: () => openInlineComposer(it, "reply") }, icon("corner-up-left", "icon-sm"), "Reply"),
      el("button", { class: "btn ghost sm icon-only", title: "Reply all", onclick: () => openInlineComposer(it, "replyAll") }, icon("users", "icon-sm")),
      el("button", { class: "btn ghost sm icon-only", title: "Forward", onclick: () => openInlineComposer(it, "forward") }, icon("corner-up-right", "icon-sm")),
    );
  }
  if (it.has_body) actions.append(remoteImages
    ? el("button", { class: "btn ghost sm", title: "Block external content again (privacy)", onclick: () => renderMailReader(it, false) }, icon("shield", "icon-sm"), "Hide external content")
    : el("button", { class: "btn ghost sm", title: "Load external content — images & web fonts (may notify the sender you opened it)", onclick: () => renderMailReader(it, true) }, icon("globe", "icon-sm"), "Load external content"));
  actions.append(el("a", { class: "btn ghost sm", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener", title: "Open the archived copy in a new tab" }, icon("external-link", "icon-sm")));
  if (CAP.mailwrite) {
    const read = (it.preview || {}).isRead !== false;
    const flagged = (it.preview || {}).flag === "flagged";
    actions.append(
      el("button", { class: "btn ghost sm icon-only", title: read ? "Mark unread" : "Mark read", onclick: () => mailSetRead(it, !read) }, icon(read ? "mail" : "mail-open", "icon-sm")),
      el("button", { class: "btn ghost sm icon-only" + (flagged ? " on" : ""), title: "Follow-up flag (status + due date)", onclick: (e) => openFlagMenu(it, e.currentTarget) }, icon("flag", "icon-sm")),
      el("button", { class: "btn ghost sm icon-only", title: "Categories", onclick: () => openCategoryPicker(it) }, icon("tag", "icon-sm")),
      el("button", { class: "btn ghost sm icon-only", title: "Move to folder", onclick: () => openMovePicker(it) }, icon("folder", "icon-sm")),
      el("button", { class: "btn ghost sm icon-only danger", title: "Delete (to Deleted Items)", onclick: () => mailDelete(it) }, icon("trash-2", "icon-sm")),
    );
  }
  if (CAP.restore) actions.append(el("button", { class: "btn sm", title: "Restore to cloud", onclick: (e) => doRestore(it, e.currentTarget) }, icon("rotate-ccw", "icon-sm"), "Restore"));
  box.append(
    el("header", { class: "mail-reader-head" },
      el("button", { class: "mail-back btn ghost sm", title: "Back", onclick: mailBack }, icon("chevron-left", "icon-sm")),
      el("div", { class: "grow", style: "min-width:0" },
        el("div", { class: "mr-tags" }, categoryChips(it),
          p.attachments > 0 ? el("span", { class: "mi-chip" }, icon("paperclip", "icon-sm"), p.attachments + (p.attachments === 1 ? " attachment" : " attachments")) : null,
          (p.importance === "high" || p.importance === "low") ? el("span", { class: "mi-chip imp-" + p.importance, title: "Importance: " + p.importance }, icon("flag", "icon-sm"), p.importance === "high" ? "High" : "Low") : null,
          p.flag === "flagged" ? el("span", { class: "mi-chip flag-on", title: "Flagged for follow-up" }, icon("flag", "icon-sm"), "Flagged") : null,
          p.isRead === false ? el("span", { class: "mi-chip unread-chip", title: "Unread" }, "Unread") : null,
          p.inferenceClassification === "other" ? el("span", { class: "mi-chip", title: "Arrived in Other (not the Focused inbox)" }, icon("inbox", "icon-sm"), "Other") : null,
          ((fn) => fn ? el("span", { class: "mi-chip", title: "In folder: " + fn + " — click to browse it", style: "cursor:pointer", onclick: () => setSvcFilter("mail", "folder:" + it.parent_remote_id) }, icon(mailFolderIcon(fn), "icon-sm"), fn) : null)(mailFolderName(it.parent_remote_id)),
          coverageBadge(it),
          verifyChip(it)),
        el("h2", { class: "mr-subject", text: subject }),
        el("div", { class: "mr-meta" },
          el("span", { class: "avatar mail-av", style: `--c:${mailAvatarColor(it)}`, text: initials(from.name || from.email || subject) }),
          el("div", { class: "grow", style: "min-width:0" },
            el("div", { class: "mr-from truncate" }, el("b", { text: from.name || from.email || "(unknown sender)" }),
              from.name && from.email ? el("span", { class: "dim", text: " <" + from.email + ">" }) : null),
            (p.to && p.to.length) ? el("div", { class: "mr-to dim truncate", text: "To: " + p.to.join(", ") }) : null,
            (p.cc && p.cc.length) ? el("div", { class: "mr-to dim truncate", text: "Cc: " + p.cc.join(", ") }) : null,
            (p.bcc && p.bcc.length) ? el("div", { class: "mr-to dim truncate", text: "Bcc: " + p.bcc.join(", ") }) : null),
          el("span", { class: "mr-date dim tnum", text: fmtFullDate(when) }))),
      actions));
  // Conversation strip (#563): if this message is part of a multi-message thread,
  // list every sibling (oldest → newest) so you can step through the conversation.
  const convId = p.conversationId;
  if (convId) {
    const sibs = Mail.all.filter(x => (x.preview || {}).conversationId === convId).sort((a, b) => mailDate(a) - mailDate(b));
    if (sibs.length > 1) {
      const strip = el("div", { class: "mr-thread" },
        el("div", { class: "mr-thread-head" }, icon("mail-open", "icon-sm"), el("span", { text: `Conversation · ${sibs.length} messages` })));
      sibs.forEach(s => {
        const sp = s.preview || {}, cur = s.remote_id === it.remote_id;
        strip.append(el("button", { class: "mr-thread-item" + (cur ? " cur" : ""), title: sp.subject || s.name || "", onclick: () => { if (!cur) mailSelect(s); } },
          el("span", { class: "mr-thread-dot", style: `background:${mailAvatarColor(s)}` }),
          el("span", { class: "grow truncate", text: addrLabel(sp.from) || "(unknown sender)" }),
          (sp.isRead === false) ? el("span", { class: "mr-thread-unread", title: "Unread" }) : null,
          el("span", { class: "dim tnum", style: "font-size:11px;flex:none", text: fmtDate(sp.date || s.remote_mtime) })));
      });
      box.append(strip);
    }
  }
  // The body is a same-origin sandboxed iframe. Size it to its own content on
  // load and let the OUTER pane scroll → the whole message scrolls naturally
  // (an internally-scrolling iframe in a flex column felt like "can't scroll").
  // sandbox: allow-same-origin only — scripts/forms/popups/top-navigation are
  // blocked by the sandbox (defense-in-depth beside the sanitizer + CSP), while
  // same-origin access stays open so fit() can measure the content height.
  const frame = el("iframe", { class: "mail-frame", src: `/api/v1/view?${qs(viewQ)}`, title: "Message body", sandbox: "allow-same-origin" });
  // Re-measure on EVERY content size change (images decode after load, fonts
  // reflow, etc.) so the iframe always matches its full content height and the
  // outer pane can scroll all the way to the end. The >2px guard avoids a
  // ResizeObserver feedback loop.
  const fit = () => {
    try {
      const d = frame.contentDocument; if (!d || !d.body) return;
      const h = Math.max(d.documentElement.scrollHeight, d.body.scrollHeight) + 4;
      const cur = parseInt(frame.style.height, 10) || 0;
      if (Math.abs(cur - h) > 2) frame.style.height = h + "px";
    } catch { /* cross-origin: leave default */ }
  };
  frame.addEventListener("load", () => {
    fit();
    try {
      const d = frame.contentDocument;
      if (d && window.ResizeObserver) { const ro = new ResizeObserver(fit); ro.observe(d.documentElement); if (d.body) ro.observe(d.body); }
      if (d) d.querySelectorAll("img").forEach(img => { if (!img.complete) { img.addEventListener("load", fit, { once: true }); img.addEventListener("error", fit, { once: true }); } });
    } catch { /* cross-origin */ }
    [120, 400, 1000, 2500].forEach(t => setTimeout(fit, t));   // fallback for late reflows
  });
  const atts = p.attachment_list || [];
  if (atts.length) {
    box.append(el("div", { class: "mr-attachments" },
      atts.map(a => el("a", {
        class: "mr-att", target: "_blank", rel: "noopener",
        href: `/api/v1/attachment?${qs({ account: App.account, service: "mail", id: it.remote_id, index: a.index })}`,
        download: a.filename || ("attachment-" + a.index),
        title: (a.filename || "attachment") + " · " + (a.content_type || ""),
      }, icon("paperclip", "icon-sm"),
        el("span", { class: "truncate", text: a.filename || "attachment" }),
        el("span", { class: "dim mr-att-size", text: fmtSize(a.size) })))));
  }
  if (it.has_body) {
    box.append(el("div", { class: "mail-frame-scroll" }, frame));
  } else {
    // No archived body (live-only on the mobile cache, or not downloaded yet):
    // show a graceful card — never the raw /api/v1/view 404 JSON (#89 CC-3).
    const card = el("div", { class: "empty mail-no-body" }, emptyArt("empty-mail"),
      el("h3", { text: MOBILE ? "Not cached on this device" : "Body not archived yet" }),
      el("p", { text: MOBILE
        ? "This message is in Microsoft 365 — its body isn't cached on this device."
        : "This message is indexed; its body isn't in your backup yet." }));
    if (p.webLink) card.append(el("a", { class: "btn sm primary", style: "margin-top:12px", href: p.webLink, target: "_blank", rel: "noopener" }, icon("external-link", "icon-sm"), "Open in Outlook"));
    box.append(card);
  }
}
function metricCard(icn, val, label) {
  return el("div", { class: "metric-card" }, el("span", { class: "mc-ico" }, icon(icn, "icon-sm")),
    el("div", {}, el("div", { class: "mc-val tnum", text: String(val) }), el("div", { class: "mc-lbl dim", text: label })));
}

/* ---------------------------------------------------------------- onedrive (file explorer) */
const Drive = { stack: [], layout: "grid", items: [], modes: { default_mode: "online", folder_modes: {} }, quota: null, modeFilter: "all", transfers: [], conflicts: [] };
// #652: per-folder mode resolvers — PURE reads of Drive.modes (the server folder-mode map is the
// single source of truth, re-fetched every driveLoad; no local override cache / optimistic state).
// Used only for the CURRENT folder (absent from its own child list); subfolder pills use the
// server-computed child.effective_mode directly.
function driveEffMode(folderId, ancestryIds) {          // mirrors OneDriveModes::effective_mode (deepest-first)
  const fm = Drive.modes.folder_modes || {};
  if (fm[folderId]) return fm[folderId];
  for (const pid of ancestryIds) if (fm[pid]) return fm[pid];
  return Drive.modes.default_mode || "online";
}
function driveExplicit(folderId) { return !!(Drive.modes.folder_modes || {})[folderId]; }
// F's ancestor folder ids deepest-first (immediate parent → toward root), excluding the root
// sentinel and F itself — exactly the `&ancestry=` the #651 children endpoint expects.
function driveAncestryIds() { return Drive.stack.slice(1, -1).map(s => s.id).reverse(); }
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

function driveSortSelect() {
  const sel = el("select", { class: "input", title: "Sort", onchange: (e) => { Drive.sort = e.target.value; driveRender(); } },
    el("option", { value: "name", text: "Name A–Z" }),
    el("option", { value: "recent", text: "Recently modified" }),
    el("option", { value: "size", text: "Largest first" }));
  sel.value = Drive.sort || "name";
  return sel;
}
async function renderOnedriveView(view) {
  Drive.stack = []; Drive.layout = Drive.layout || "grid"; Drive.items = []; Drive.stateFilter = "all"; Drive.modeFilter = "all"; Drive.sort = Drive.sort || "name";
  clear(view).append(
    el("div", { id: "drive-metrics-row", class: "con-metrics-row inset" }),
    el("div", { class: "drive-bar" },
      el("div", { id: "drive-crumbs", class: "drive-crumbs" }),
      el("div", { class: "spacer", style: "flex:1" }),
      el("label", { class: "tb-sort" }, icon("arrow-down-up", "icon-sm"), driveSortSelect()),
      CAP.onedrivewrite ? el("button", { class: "btn ghost sm", title: "Upload a file into this folder", onclick: driveUpload }, icon("upload", "icon-sm"), "Upload") : null,
      verifyButton(() => renderOnedriveView(view)),
      el("div", { class: "seg" },
        el("button", { id: "drive-grid", class: "seg-btn" + (Drive.layout === "grid" ? " active" : ""), title: "Grid view", onclick: () => setDriveLayout("grid") }, icon("layout-dashboard", "icon-sm")),
        el("button", { id: "drive-list", class: "seg-btn" + (Drive.layout === "list" ? " active" : ""), title: "List view", onclick: () => setDriveLayout("list") }, icon("list", "icon-sm")))),
    el("div", { id: "drive-modebar", style: "display:flex;align-items:center;gap:8px;flex-wrap:wrap;padding:2px 2px 4px" }),
    // #659 Conflict Center: a review banner (own element) shown when the account has unresolved
    // keep-both conflicts. Hidden until the store-driven conflicts poll finds any.
    el("div", { id: "drive-conflicts", style: "display:none;padding:0 2px 8px" }),
    el("div", { id: "drive-storage", style: "display:flex;align-items:center;gap:6px;padding:0 2px 8px;font-size:12px" }),
    // #656: live transfer-progress panel (own visible element — the desktop #drive-metrics-row
    // is display:none on mobile). Hidden until the poll finds an in-flight transfer.
    el("div", { id: "drive-transfers", style: "display:none;flex-direction:column;gap:8px;padding:0 2px 8px" }),
    el("div", { id: "drive-body" }),
  );
  driveLoadMetrics();
  startDriveTransfersPoll();
  await driveOpen(null, "OneDrive", true);
}
// account-wide OneDrive KPIs (flat item list, independent of the current folder)
async function driveLoadMetrics() {
  try {
    if (MOBILE) {
      // #652: online browse keeps no store, so file/archived counts are meaningless here. Show a
      // single storage line (used bytes + total-or-"unlimited") in its own visible header row —
      // the desktop #drive-metrics-row is display:none on mobile, so render into #drive-storage.
      const drv = await api("/api/v1/drive?" + qs({ account: App.account })).catch(() => null);
      Drive.quota = (drv && drv.quota) || null;
      const box = $("#drive-storage"); if (!box) return; clear(box);
      const q = Drive.quota;
      if (q && typeof q.used === "number") {
        const hasTotal = typeof q.total === "number" && q.total > 0;
        const sub = hasTotal ? `${Math.round((q.used || 0) / q.total * 100)}% of ${fmtSize(q.total)} · ${fmtSize(q.remaining || 0)} free` : "unlimited";
        box.append(icon("hard-drive", "icon-sm"),
          el("span", { style: "font-weight:700", text: fmtSize(q.used || 0) }),
          el("span", { class: "dim", text: "used · " + sub }));
      } else {
        box.append(icon("hard-drive", "icon-sm"), el("span", { class: "dim", text: "Storage unavailable" }));
      }
      return;
    }
    const [d, act, drv] = await Promise.all([
      api("/api/v1/items?" + qs({ account: App.account, service: "onedrive", limit: 2000 })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 30 })).catch(() => ({ runs: [] })),
      // live drive quota (#564) — best-effort: 404 on the read-only `serve` server
      api("/api/v1/drive?" + qs({ account: App.account })).catch(() => null),
    ]);
    const all = d.items || [];
    const files = all.filter(it => it.item_type !== "folder");
    const folders = all.filter(it => it.item_type === "folder").length;
    const archived = files.filter(it => it.has_body).length;
    App.counts.onedrive = all.length; updateNavCounts();
    const cards = [
      { icon: "file", value: files.length, label: "Files", sub: `${folders} folders` },
      { icon: "download", value: archived, label: "Archived", sub: "tracked with a copy", tone: archived ? "ok" : "" },
      integrityMetric(files),
      lastActivityMetric(act.runs || []),
    ];
    const q = drv && drv.quota;
    if (q && typeof q.total === "number" && q.total > 0) {
      const usedPct = Math.round((q.used || 0) / q.total * 100);
      cards.push({
        icon: "hard-drive", value: fmtSize(q.used || 0), label: "Storage used",
        sub: `${usedPct}% of ${fmtSize(q.total)} · ${fmtSize(q.remaining || 0)} free`,
        tone: usedPct >= 90 ? "warn" : "",
      });
    }
    fillMetrics($("#drive-metrics-row"), cards);
  } catch { /* metrics are best-effort; the explorer still loads */ }
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
    if (MOBILE) {
      // Mode-1 online (#649): browse live from Graph — the phone store is a cache and is
      // empty for OneDrive, so the store-based listing would show the empty placeholder.
      // #652: send the breadcrumb as `&ancestry=` (deepest-first) so children carry a correct
      // effective_mode, and fetch the folder-mode map alongside (explicit-vs-inherited + the
      // current folder). Drive.modes is the SSOT — re-read on every navigation.
      const anc = driveAncestryIds().join(",");
      const [d, modes] = await Promise.all([
        api("/api/v1/onedrive/children?" + qs({ account: App.account, folder: cur === "root" ? "" : cur, ancestry: anc })),
        api("/api/v1/onedrive/mode?" + qs({ account: App.account })).catch(() => Drive.modes),
      ]);
      Drive.modes = modes || Drive.modes;
      Drive.items = (d.children || []).map(driveMapChild);
    } else {
      const d = await api("/api/v1/items?" + qs({ account: App.account, service: "onedrive", parent: cur }));
      Drive.items = d.items || [];
    }
    driveRender();
    renderDriveModeBar();
    driveLoadConflicts(); // #659 store-driven conflict banner (independent of the live browse)
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load folder" }), el("p", { text: e.message }))); }
}
// #652: the current-folder mode control + a "partial" indicator. The current folder is not in its
// own child list, so its effective mode is resolved from Drive.modes (SSOT) + the breadcrumb.
// "Partial" = at least one listed subfolder resolves to a different mode than this folder → the
// folder's contents are not uniformly one mode (some sync/offline below). Mobile-only.
function renderDriveModeBar() {
  const bar = $("#drive-modebar"); if (!bar) return; clear(bar);
  if (!MOBILE || !CAP.onedriveMode) return;
  const cur = Drive.stack[Drive.stack.length - 1] || { id: "root", name: "OneDrive" };
  const isRoot = cur.id === "root";
  const curMode = isRoot ? (Drive.modes.default_mode || "online") : driveEffMode(cur.id, driveAncestryIds());
  bar.append(el("span", { class: "dim", style: "font-size:12px", text: "This folder:" }));
  if (isRoot) {
    // the drive root has no per-folder id to POST — show the account default, read-only.
    const c = MODE_COLOR[curMode];
    bar.append(el("span", { title: "Account default mode",
      style: "display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-size:11px;"
        + `font-weight:600;border:1px solid ${c};color:${c};opacity:0.72` },
      icon(MODE_ICON[curMode], "icon-sm"), el("span", { text: MODE_LABEL[curMode] + " · default" })));
  } else {
    bar.append(driveModePill(cur.id, curMode, driveExplicit(cur.id), cur.name));
  }
  const partial = Drive.items.some(it => it.item_type === "folder" && it.effective_mode && it.effective_mode !== curMode);
  if (partial) bar.append(el("span", { title: "Some subfolders have a different mode",
    style: "display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-size:11px;font-weight:600;"
      + "border:1px solid var(--warn,#f59e0b);color:var(--warn,#f59e0b);background:#f59e0b1a" },
    icon("info", "icon-sm"), el("span", { text: "Partial — mixed modes below" })));
}
// #649 Mode-1 online: normalize a live Graph child (from /api/v1/onedrive/children) onto the
// store-item shape driveRender/driveTile/driveRow expect. No local body/preview in online mode.
function driveMapChild(c) {
  return {
    item_type: c.folder ? "folder" : "file",
    remote_id: c.id,
    effective_mode: c.effective_mode,   // #652: per-item mode from the #651 children enrichment
    name: c.name,
    size: c.size,
    remote_mtime: c.lastModifiedDateTime,
    etag: c.eTag, // #657: If-Match token for in-place replace
    has_body: false,
    preview: c.folder ? { child_count: c.folder && c.folder.childCount } : undefined,
  };
}
// File-open URL: online (mobile) fetches the content on-demand via the open endpoint; desktop
// opens the archived copy via /view. Folders never reach here (they navigate).
function driveFileUrl(it) {
  return MOBILE
    ? `/api/v1/onedrive/open?${qs({ account: App.account, id: it.remote_id, name: it.name || "" })}`
    : `/api/v1/view?${qs({ account: App.account, service: "onedrive", id: it.remote_id })}`;
}
// Open a file's content. Desktop opens the archived copy in a new browser tab. The mobile
// WebView has no WebChromeClient (window.open is a no-op), so show the on-demand content in an
  // in-app full-screen viewer whose app-origin iframe is served by the trusted native asset path (#649/#721).
function driveOpenFile(it) {
  const url = driveFileUrl(it);
  if (!MOBILE) { window.open(url, "_blank", "noopener"); return; }
  const ov = el("div", { style: "position:fixed;inset:0;z-index:200;background:#0b0f17;display:flex;flex-direction:column" },
    el("div", { style: "display:flex;align-items:center;gap:8px;padding:12px 14px" },
      el("button", { class: "btn ghost sm", onclick: () => ov.remove() }, icon("arrow-left", "icon-sm"), "Back"),
      el("span", { class: "truncate", style: "flex:1;font-weight:600", text: it.name || "" })),
    el("iframe", { style: "flex:1;border:0;background:#fff", src: url, sandbox: "allow-same-origin", title: it.name || "File" }));
  document.body.append(ov);
}
function mtimeMs(it) { const t = Date.parse(it.remote_mtime || ""); return isNaN(t) ? 0 : t; }
function driveSort(items) {
  const mode = Drive.sort || "name";
  return items.slice().sort((a, b) => {
    const fa = a.item_type === "folder" ? 0 : 1, fb = b.item_type === "folder" ? 0 : 1;
    if (fa !== fb) return fa - fb; // folders always first
    if (mode === "recent") return mtimeMs(b) - mtimeMs(a); // newest first (#564)
    if (mode === "size") return (b.size || 0) - (a.size || 0);
    return (a.name || "").localeCompare(b.name || "");
  });
}
function driveRender() {
  const body = $("#drive-body"); if (!body) return; clear(body);
  if (!Drive.items.length) { body.append(el("div", { class: "empty" }, emptyArt("empty-files"), el("h3", { text: "Empty folder" }), el("p", { text: MOBILE ? "OneDrive isn't cached on this device — it stays in your backup on your computer." : "Nothing is archived here." }))); return; }
  // folders always navigate; the filter applies to files only. Mobile OneDrive filters by the
  // MODE axis (online/sync/offline, #656); desktop keeps the backup-coverage axis.
  const files = Drive.items.filter(it => it.item_type !== "folder");
  if (MOBILE) {
    body.append(driveModeFilterBar(files, Drive.modeFilter || "all", k => { Drive.modeFilter = k; driveRender(); }));
  } else {
    body.append(stateFilterBar(files, Drive.stateFilter, k => { Drive.stateFilter = k; driveRender(); }));
  }
  const items = driveSort(Drive.items.filter(it => it.item_type === "folder" || (MOBILE ? modeMatch(it, Drive.modeFilter || "all") : stateMatch(it, Drive.stateFilter))));
  if (!items.length) { body.append(el("div", { class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No matches" }), el("p", { text: MOBILE ? "No files here are in this mode." : "No files here have this backup status." }))); return; }
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
// #652: per-folder mode pill + picker sheet. The pill shows a folder's effective OneDrive mode
// (solid border+fill = explicit own override, dimmed = inherited); tap opens a sheet to set or
// clear it. Online = live/nothing stored, Sync = metadata cached, Offline = fully downloaded.
const MODE_LABEL = { online: "Online", sync: "Sync", offline: "Offline" };
const MODE_ICON = { online: "cloud", sync: "refresh-cw", offline: "hard-drive" };
const MODE_COLOR = { online: "var(--dim,#94a3b8)", sync: "var(--svc-onedrive,#0a84ff)", offline: "var(--ok,#34d399)" };
// #656: mobile OneDrive renders the per-item MODE axis (online/sync/offline), not the
// desktop backup-coverage badge — online browsing keeps no store, so `has_body` is always
// false there and coverage would read "Live only" for everything. A file inherits its
// folder's effective mode; the chip is read-only (folders keep the interactive driveModePill).
const MODE_KEYS = ["online", "sync", "offline"];
const modeOf = (it) => (MODE_KEYS.includes(it.effective_mode) ? it.effective_mode : "online");
const modeMatch = (it, f) => f === "all" || modeOf(it) === f;
// Read-only per-file mode chip. If the file is in the live transfers set (a materialization
// in flight), show a "Downloading N%" chip instead — ties the mode view to the progress panel.
function driveModeChip(it) {
  const dl = (Drive.transfers || []).find(t => t.id === it.remote_id);
  if (dl) {
    const pct = dl.bytes_total > 0 ? Math.round((dl.bytes_done || 0) / dl.bytes_total * 100) : null;
    const c = "var(--svc-onedrive,#0a84ff)";
    return el("span", { class: "mode-chip downloading", title: "Downloading" + (pct != null ? " " + pct + "%" : ""),
      style: "display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-size:11px;"
        + "font-weight:600;line-height:1.4;white-space:nowrap;" + `border:1px solid ${c};color:${c};background:${c}22;` },
      icon("download", "icon-sm"), el("span", { text: pct != null ? "Downloading " + pct + "%" : "Downloading" }));
  }
  const m = modeOf(it), c = MODE_COLOR[m];
  return el("span", { class: "mode-chip", title: "Mode: " + MODE_LABEL[m] + " (inherited from folder)",
    style: "display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;font-size:11px;"
      + "font-weight:600;line-height:1.4;white-space:nowrap;opacity:0.85;" + `border:1px solid ${c};color:${c};background:transparent;` },
    icon(MODE_ICON[m], "icon-sm"), el("span", { text: MODE_LABEL[m] }));
}
// Mode filter chips (mobile) — replaces the coverage stateFilterBar. Same look (.state-chips),
// counts + filters over effective_mode. `onPick(key)` re-renders the view.
function driveModeFilterBar(items, current, onPick) {
  const counts = { all: items.length };
  for (const k of MODE_KEYS) counts[k] = 0;
  items.forEach(it => { counts[modeOf(it)]++; });
  const mk = (key, label) => el("button", { class: "state-chip" + (key === current ? " active" : ""), onclick: () => onPick(key) },
    key === "all" ? null : icon(MODE_ICON[key], "icon-sm"), el("span", { text: label }), el("span", { class: "sc-count", text: String(counts[key] || 0) }));
  return el("div", { class: "state-chips" }, mk("all", "All"), mk("online", MODE_LABEL.online), mk("sync", MODE_LABEL.sync), mk("offline", MODE_LABEL.offline));
}
// #656: live transfer-progress panel. There is no SSE hook for transfers, so poll
// GET /api/v1/onedrive/transfers (bytes/file/retry-after) while the OneDrive view is open.
// Self-clearing: the timer stops when #drive-transfers leaves the DOM (view switched).
let driveTransfersTimer = null, driveTransferIdKey = "";
function startDriveTransfersPoll() {
  if (driveTransfersTimer) { clearInterval(driveTransfersTimer); driveTransfersTimer = null; }
  driveTransferIdKey = "";
  pollDriveTransfers();
  driveTransfersTimer = setInterval(pollDriveTransfers, 1500);
}
async function pollDriveTransfers() {
  const box = $("#drive-transfers");
  if (!box) { if (driveTransfersTimer) { clearInterval(driveTransfersTimer); driveTransfersTimer = null; } return; }
  try {
    const d = await api("/api/v1/onedrive/transfers");
    Drive.transfers = (d && d.transfers) || [];
  } catch { Drive.transfers = []; }
  renderTransfersPanel();
  // Re-render the item list only when the SET of transferring ids changes (not every byte
  // tick), so the per-file "Downloading" chip toggles without fighting the user mid-scroll.
  const idKey = Drive.transfers.map(t => t.id).sort().join(",");
  if (idKey !== driveTransferIdKey) { driveTransferIdKey = idKey; if ($("#drive-body")) driveRender(); }
}
function renderTransfersPanel() {
  const box = $("#drive-transfers"); if (!box) return;
  const list = Drive.transfers || [];
  clear(box);
  if (!list.length) { box.style.display = "none"; return; }
  box.style.display = "flex";
  box.append(el("div", { style: "display:flex;align-items:center;gap:6px;font-size:12px;font-weight:700" },
    icon("download", "icon-sm"), el("span", { text: "Transferring " + list.length + (list.length === 1 ? " file" : " files") })));
  list.forEach(t => box.append(transferRow(t)));
}
function transferRow(t) {
  const total = t.bytes_total || 0, done = t.bytes_done || 0;
  const pct = total > 0 ? Math.min(100, Math.round(done / total * 100)) : null;
  const fill = el("div", { style: "position:absolute;top:0;bottom:0;left:0;border-radius:999px;background:var(--svc-onedrive,#0a84ff);"
    + (pct != null ? `width:${pct}%;` : "width:40%;opacity:0.5;") + (t.paused ? "opacity:0.4;" : "") });
  const bar = el("div", { style: "position:relative;height:6px;border-radius:999px;overflow:hidden;background:var(--bg-3,#1e293b)" }, fill);
  const meta = (pct != null ? fmtSize(done) + " / " + fmtSize(total) + " · " + pct + "%" : fmtSize(done) + " transferred") + (t.paused ? " · paused" : "");
  const retry = (t.retry_after_secs && t.retry_after_secs > 0) ? el("span", { class: "dim", style: "font-size:11px", text: "· retry in " + t.retry_after_secs + "s" }) : null;
  // #659 pause/retry controls (queue-deep). Same cap as cancel (CAP.transfers). Paused → resume;
  // running → pause; backing off → a "retry now". Resume + retry both hit /transfers/retry (which
  // un-pauses + clears the 429 backoff); pause hits /transfers/pause.
  let pauseCtl = null;
  if (CAP.transfers) {
    pauseCtl = t.paused
      ? el("button", { class: "btn ghost sm", title: "Resume transfer", onclick: () => retryTransfer(t, "Resuming") }, icon("play", "icon-sm"))
      : el("button", { class: "btn ghost sm", title: "Pause transfer", onclick: () => pauseTransfer(t) }, icon("pause", "icon-sm"));
  }
  const retryBtn = (CAP.transfers && !t.paused && t.retry_after_secs && t.retry_after_secs > 0)
    ? el("button", { class: "btn ghost sm", title: "Retry now", onclick: () => retryTransfer(t, "Retrying") }, icon("rotate-ccw", "icon-sm")) : null;
  const cancelBtn = CAP.transfers ? el("button", { class: "btn ghost sm", title: "Cancel transfer", onclick: () => cancelTransfer(t) }, icon("x", "icon-sm")) : null;
  return el("div", { style: "display:flex;align-items:center;gap:8px" },
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "truncate", style: "font-size:12px;font-weight:600", text: t.name || t.id }),
      bar,
      el("div", { style: "display:flex;gap:6px;align-items:center" }, el("span", { class: "dim", style: "font-size:11px", text: meta }), retry)),
    ...[pauseCtl, retryBtn, cancelBtn].filter(Boolean));
}
async function cancelTransfer(t) {
  try {
    await post("/api/v1/onedrive/transfers/cancel?" + qs({ id: t.id }), CAP.transfers);
    // Optimistic: drop it from the panel now; the next poll confirms the engine skipped it.
    Drive.transfers = (Drive.transfers || []).filter(x => x.id !== t.id);
    renderTransfersPanel();
    toast("Cancelling " + (t.name || "transfer"));
  } catch (e) { toast("Could not cancel: " + e.message, "err"); }
}
async function pauseTransfer(t) {
  try {
    await post("/api/v1/onedrive/transfers/pause?" + qs({ id: t.id }), CAP.transfers);
    // Optimistic: mark paused now; the next poll confirms it from the engine's pause-set.
    const row = (Drive.transfers || []).find(x => x.id === t.id); if (row) row.paused = true;
    renderTransfersPanel();
    toast("Paused " + (t.name || "transfer"));
  } catch (e) { toast("Could not pause: " + e.message, "err"); }
}
async function retryTransfer(t, verb) {
  try {
    await post("/api/v1/onedrive/transfers/retry?" + qs({ id: t.id }), CAP.transfers);
    const row = (Drive.transfers || []).find(x => x.id === t.id); if (row) { row.paused = false; row.retry_after_secs = 0; }
    renderTransfersPanel();
    toast((verb || "Retrying") + " " + (t.name || "transfer"));
  } catch (e) { toast("Could not retry: " + e.message, "err"); }
}
function driveModePill(folderId, effMode, explicit, name) {
  const m = effMode || "online", c = MODE_COLOR[m];
  return el("button", {
    class: "mode-pill" + (explicit ? " explicit" : " inherited"),
    type: "button",
    title: (explicit ? "Mode: " : "Inherited: ") + MODE_LABEL[m] + " — tap to change",
    style: "display:inline-flex;align-items:center;gap:4px;padding:2px 8px;border-radius:999px;"
      + "font-size:11px;font-weight:600;line-height:1.4;white-space:nowrap;cursor:pointer;"
      + `border:1px solid ${c};color:${c};background:${explicit ? c + "22" : "transparent"};`
      + (explicit ? "" : "opacity:0.72;"),
    onclick: (e) => { e.stopPropagation(); openModeSheet(folderId, name, effMode, explicit); },
  }, icon(MODE_ICON[m], "icon-sm"), el("span", { text: MODE_LABEL[m] + (explicit ? "" : " · inherited") }));
}
function openModeSheet(folderId, name, effMode, explicit) {
  const choose = (mode, label, desc) => el("button", { class: "pick-row pick-btn", type: "button",
    onclick: () => { closeSheet(); setFolderMode(folderId, mode); } },
    icon(MODE_ICON[mode], "icon-sm"),
    el("div", { class: "grow" },
      el("div", { style: "font-weight:600", text: label }),
      el("div", { class: "dim", style: "font-size:12px", text: desc })),
    effMode === mode ? icon("check", "icon-sm") : null);
  const content = el("div", { class: "compose" }, el("div", { class: "pick-list" },
    choose("online", "Online", "Live — nothing stored on the device"),
    choose("sync", "Sync", "Metadata cached, files downloaded on demand"),
    choose("offline", "Offline", "Fully downloaded — works offline")));
  if (explicit) content.append(el("button", { class: "btn ghost sm", type: "button", style: "margin-top:10px",
    onclick: () => { closeSheet(); setFolderMode(folderId, null); } },
    icon("rotate-ccw", "icon-sm"), "Reset to inherited"));
  openSheet(name ? "Mode — " + name : "Folder mode", content);
}
// POST the folder mode (omit `mode` to clear → inherited); re-load so pills + the partial
// indicator reflect the fresh server state (Drive.modes SSOT). Mirrors doShare's template.
async function setFolderMode(folderId, mode) {
  try {
    const resp = await post("/api/v1/onedrive/mode?" + qs({ account: App.account, folder: folderId, ...(mode ? { mode } : {}) }), CAP.onedriveMode);
    toast(mode ? "Folder set to " + MODE_LABEL[mode] : "Folder reset to inherited");
    // #659 D1: switching a folder online runs the offline→online cleanup server-side; surface it.
    const c = resp && resp.cleanup;
    if (c && (c.freed || c.kept)) {
      const parts = [];
      if (c.freed) parts.push("freed " + c.freed + (c.freed === 1 ? " file" : " files"));
      if (c.kept) parts.push("kept " + c.kept + " unsynced");
      toast("Cleanup: " + parts.join(" · "));
    }
    await driveLoad();
    driveLoadConflicts(); // kept-unsynced items may include conflicts to review
  } catch (e) { toast("Could not change mode: " + e.message, "err"); }
}
// #659 Conflict Center. The conflicts are store-driven (the keep-both offline/upload sites persist
// `conflict_state`), so this works regardless of the live-browse mode. Mobile-only + cap-gated.
const RESOLUTION_LABEL = { "keep-both": "Kept both", "keep-mine": "Kept your version", "keep-cloud": "Kept cloud version" };
async function driveLoadConflicts() {
  if (!MOBILE || !CAP.onedriveManage) { Drive.conflicts = []; renderConflictBanner(); return; }
  try {
    const d = await api("/api/v1/onedrive/conflicts?" + qs({ account: App.account }));
    Drive.conflicts = (d && d.conflicts) || [];
  } catch { Drive.conflicts = []; }
  renderConflictBanner();
}
function renderConflictBanner() {
  const bar = $("#drive-conflicts"); if (!bar) return; clear(bar);
  const list = Drive.conflicts || [];
  if (!list.length) { bar.style.display = "none"; return; }
  bar.style.display = "block";
  bar.append(el("button", {
    class: "btn sm", type: "button",
    style: "display:flex;align-items:center;gap:8px;width:100%;justify-content:flex-start;"
      + "border:1px solid var(--warn,#f59e0b);color:var(--warn,#f59e0b);background:#f59e0b1a",
    onclick: () => openConflictCenter(),
  }, icon("flag", "icon-sm"),
    el("span", { class: "grow", style: "text-align:left;font-weight:600",
      text: list.length + (list.length === 1 ? " conflict" : " conflicts") + " — Review" }),
    icon("chevron-right", "icon-sm")));
}
function openConflictCenter() {
  const content = el("div", { class: "compose" });
  const list = Drive.conflicts || [];
  if (!list.length) content.append(el("div", { class: "dim", text: "No conflicts to resolve." }));
  list.forEach(cf => {
    const btn = (res, label) => el("button", { class: "btn ghost sm", type: "button", onclick: () => resolveConflict(cf, res) }, label);
    content.append(el("div", { class: "pick-row", style: "flex-direction:column;align-items:stretch;gap:8px" },
      el("div", { style: "display:flex;align-items:center;gap:8px" },
        icon("flag", "icon-sm"),
        el("div", { class: "grow", style: "min-width:0" },
          el("div", { class: "truncate", style: "font-weight:600", text: cf.name || cf.id }),
          cf.conflict_copy ? el("div", { class: "dim", style: "font-size:12px", text: "Kept copy: " + cf.conflict_copy }) : null)),
      el("div", { style: "display:flex;gap:6px;flex-wrap:wrap" },
        btn("keep-both", "Keep both"), btn("keep-mine", "Keep mine"), btn("keep-cloud", "Keep cloud"))));
  });
  openSheet("Conflicts", content);
}
async function resolveConflict(cf, resolution) {
  try {
    // keep-mine deletes the cloud copy → the mobile router raises the biometric gate, handled
    // automatically by request()'s confirmation_required flow.
    await post("/api/v1/onedrive/conflict/resolve?" + qs({ account: App.account, id: cf.id, resolution }), CAP.onedriveManage);
    toast(RESOLUTION_LABEL[resolution] || "Resolved");
    closeSheet();
    await driveLoadConflicts();
    driveLoad();
  } catch (e) { toast("Could not resolve: " + e.message, "err"); }
}
function driveActions(it) {
  const folder = it.item_type === "folder";
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const box = el("div", { class: "drive-actions" });
  box.append(el("button", { class: "act", title: "Details & access", onclick: (e) => { e.stopPropagation(); openDriveItem(it); } }, icon("info", "icon-sm")));
  if (!folder && it.has_body) box.append(el("a", { class: "act", href: `/api/v1/body?${qs(q)}`, download: it.name || "", title: "Download", onclick: (e) => e.stopPropagation() }, icon("download", "icon-sm")));
  if (!folder && CAP.share) box.append(el("button", { class: "act", title: "Share", onclick: (e) => { e.stopPropagation(); doShare(it, e.currentTarget); } }, icon("share2", "icon-sm")));
  // #657: in-app write actions (cap-gated; destructive ops are biometric-gated on mobile).
  if (!folder && CAP.onedrivewrite) box.append(el("button", { class: "act", title: "Replace contents", onclick: (e) => { e.stopPropagation(); driveReplace(it); } }, icon("refresh-cw", "icon-sm")));
  if (CAP.onedrivewrite) {
    box.append(el("button", { class: "act", title: "Rename", onclick: (e) => { e.stopPropagation(); driveRename(it); } }, icon("pencil", "icon-sm")));
    box.append(el("button", { class: "act", title: "Move to folder", onclick: (e) => { e.stopPropagation(); driveMovePicker(it); } }, icon("folder-input", "icon-sm")));
    box.append(el("button", { class: "act", title: "Delete", onclick: (e) => { e.stopPropagation(); driveDelete(it); } }, icon("trash-2", "icon-sm")));
  }
  return box;
}
// #657 in-app upload: pick a file and upload it into the CURRENT folder (read live at click
// time). Bytes → base64 → POST /onedrive/upload; execution lands with #655 (placeholder Err
// surfaces honestly until then). Cap-gated; biometric-gated on mobile.
function driveUpload() {
  if (!CAP.onedrivewrite) return;
  const cur = Drive.stack[Drive.stack.length - 1].id;
  const inp = el("input", { type: "file" });
  inp.addEventListener("change", async () => {
    const file = inp.files && inp.files[0]; if (!file) return;
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      await postBinary("/api/v1/onedrive/upload?" + qs({ account: App.account, parent: cur === "root" ? "" : cur, name: file.name }), CAP.onedrivewrite, bytes);
      toast(`Uploaded ${file.name}`); driveLoad(); driveLoadMetrics();
    } catch (e) { toast("Upload failed: " + e.message, "err"); }
  });
  inp.click();
}
// #657 in-app replace: overwrite a file's content in place (If-Match its eTag; a 412 conflict
// is surfaced, never clobbered). Bytes → base64 → POST /onedrive/replace.
function driveReplace(it) {
  if (!CAP.onedrivewrite) return;
  const inp = el("input", { type: "file" });
  inp.addEventListener("change", async () => {
    const file = inp.files && inp.files[0]; if (!file) return;
    try {
      const bytes = new Uint8Array(await file.arrayBuffer());
      await postBinary("/api/v1/onedrive/replace?" + qs({ account: App.account, id: it.remote_id, etag: it.etag || "" }), CAP.onedrivewrite, bytes);
      toast(`Replaced ${it.name || "file"}`); driveLoad(); driveLoadMetrics();
    } catch (e) { toast("Replace failed: " + e.message, "err"); }
  });
  inp.click();
}
// #657 rename: a small text sheet → POST /onedrive/rename (#654 ledger). Cap-gated.
function driveRename(it) {
  if (!CAP.onedrivewrite) return;
  const inp = el("input", { class: "input", value: it.name || "" });
  const submit = async () => {
    const name = inp.value.trim(); if (!name || name === it.name) { closeSheet(); return; }
    closeSheet();
    try { await post("/api/v1/onedrive/rename?" + qs({ account: App.account, id: it.remote_id, name }), CAP.onedrivewrite); toast(`Renamed to ${name}`); driveLoad(); }
    catch (e) { toast("Rename failed: " + e.message, "err"); }
  };
  inp.addEventListener("keydown", (e) => { if (e.key === "Enter") submit(); });
  const content = el("div", { class: "compose" }, inp,
    el("div", { class: "cmp-footer" }, el("div", { class: "spacer", style: "flex:1" }),
      el("button", { class: "btn primary", type: "button", onclick: submit }, "Rename")));
  openSheet(it.item_type === "folder" ? "Rename folder" : "Rename file", content);
  setTimeout(() => { inp.focus(); inp.select(); }, 0);
}
// #657 move: pick a destination folder from the live tree, then POST /onedrive/move (#654).
// Its own drill stack; skips the item itself as a target; the drive root is "".
function driveMovePicker(it) {
  if (!CAP.onedrivewrite) return;
  const stack = [{ id: "root", name: "OneDrive" }];
  const list = el("div", { class: "pick-list" });
  const foot = el("div", { class: "cmp-footer" });
  const crumb = el("div", { class: "dim", style: "margin-bottom:8px" });
  const content = el("div", { class: "compose" }, crumb, list, foot);
  async function load() {
    const cur = stack[stack.length - 1];
    crumb.textContent = "Into: " + stack.map(s => s.name).join(" / ");
    clear(list).append(el("div", { class: "dim", text: "Loading…" }));
    let folders = [];
    try {
      const d = await api("/api/v1/onedrive/children?" + qs({ account: App.account, folder: cur.id === "root" ? "" : cur.id }));
      folders = (d.children || []).filter(c => c.folder && c.id !== it.remote_id);
    } catch (e) { clear(list).append(el("div", { class: "dim", text: "Could not load: " + e.message })); return; }
    clear(list);
    if (stack.length > 1) list.append(el("button", { class: "pick-row pick-btn", type: "button", onclick: () => { stack.pop(); load(); } }, icon("corner-up-left", "icon-sm"), el("span", { class: "grow", text: "Up one level" })));
    if (!folders.length) list.append(el("div", { class: "dim", text: "No subfolders here." }));
    folders.forEach(c => list.append(el("button", { class: "pick-row pick-btn", type: "button", onclick: () => { stack.push({ id: c.id, name: c.name }); load(); } },
      icon("folder", "icon-sm"), el("span", { class: "grow truncate", text: c.name }))));
    clear(foot).append(el("div", { class: "spacer", style: "flex:1" }),
      el("button", { class: "btn primary", type: "button", onclick: () => { closeSheet(); driveMove(it, cur.id === "root" ? "" : cur.id, cur.name); } }, `Move here`));
  }
  openSheet("Move to folder", content);
  load();
}
async function driveMove(it, parent, parentName) {
  try { await post("/api/v1/onedrive/move?" + qs({ account: App.account, id: it.remote_id, parent, name: it.name || "" }), CAP.onedrivewrite); toast(`Moved to ${parentName}`); driveLoad(); driveLoadMetrics(); }
  catch (e) { toast("Move failed: " + e.message, "err"); }
}
// #657 delete: confirmDestructive (desktop confirm(); on mobile the biometric gate IS the
// confirmation) → POST /onedrive/delete (#654 ledger).
async function driveDelete(it) {
  if (!CAP.onedrivewrite) return;
  if (!confirmDestructive(`Delete "${it.name || "this item"}"? This removes it from OneDrive.`)) return;
  try { await post("/api/v1/onedrive/delete?" + qs({ account: App.account, id: it.remote_id }), CAP.onedrivewrite); toast(`Deleted ${it.name || "item"}`); driveLoad(); driveLoadMetrics(); }
  catch (e) { toast("Delete failed: " + e.message, "err"); }
}
// Item detail sheet (#564): facts + lazily-fetched "who has access" (one Graph
// call per item, only on open). #564 A5 enriches the facts with the rich
// metadata (mimeType / sha256 / created-by / EXIF / …) from the sidecar preview.
function openDriveItem(it) {
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const folder = it.item_type === "folder";
  const content = el("div");
  const ext = fileExt(it.name);
  content.append(kvList([
    ["Type", folder ? "Folder" : (ext ? ext.toUpperCase() + " file" : "File")],
    ["Size", folder ? null : fmtSize(it.size)],
    ["Modified", it.remote_mtime ? fmtFullDate(it.remote_mtime) : null],
  ]));
  driveItemMeta(content, it); // #564 A5 metadata rows (no-op until enrichment lands)
  driveManageSection(content, it); // #659 free-up / download-now (mobile, cap-gated)
  content.append(el("h4", { class: "od-sec dim", text: "Who has access" }));
  const perm = el("div", { class: "od-perms" }, el("div", { class: "dim", text: "Loading access…" }));
  content.append(perm);
  openSheet(it.name || "Item", content);
  api("/api/v1/permissions?" + qs(q)).then(d => {
    clear(perm);
    const list = d.permissions || [];
    if (!list.length) { perm.append(el("div", { class: "dim", text: "Private — not shared." })); return; }
    list.forEach(p => perm.append(el("div", { class: "pick-row" },
      el("span", { class: "grow truncate", text: p.grantee || (p.link ? "Shared link" : "(unknown)") }),
      el("span", { class: "dim", text: (p.roles || []).join(", ") || "—" }))));
  }).catch(e => { clear(perm); perm.append(el("div", { class: "dim", text: "Access unavailable (" + e.message + ")" })); });
}
// #659/#724 per-item local-body management (free-up / download-now). The mobile browse is live
// Graph, so read this id's store row and its effective mode before offering any local-body action.
// Store miss or effective online rows stay actionless; the engine remains the final authority.
function driveManageSection(content, it) {
  if (!MOBILE || !CAP.onedriveManage || it.item_type === "folder") return;
  const box = el("div", { style: "margin-top:8px" });
  content.append(box);
  api("/api/v1/item?" + qs({ account: App.account, service: "onedrive", id: it.remote_id }))
    .then(row => driveRenderManage(box, it, row))
    .catch(() => driveRenderManageUnavailable(box));
}
function driveManageMode(row) {
  return row && MODE_KEYS.includes(row.effective_mode) ? row.effective_mode : null;
}
function driveCanDownloadNow(row) {
  const mode = driveManageMode(row);
  return mode === "sync" || mode === "offline";
}
function driveRenderManageUnavailable(box) {
  clear(box);
  box.append(el("h4", { class: "od-sec dim", text: "On this device" }));
  box.append(el("div", { class: "dim", style: "font-size:12px", text: "Available online." }));
}
function driveRenderManage(box, it, row) {
  clear(box);
  if (!row) { driveRenderManageUnavailable(box); return; }
  const hasBody = row.has_body === true;
  box.append(el("h4", { class: "od-sec dim", text: "On this device" }));
  if (hasBody) {
    box.append(el("button", { class: "btn ghost sm", type: "button", style: "width:100%;justify-content:flex-start",
      onclick: () => freeUpItem(it) }, icon("hard-drive", "icon-sm"), "Free up space"));
    box.append(el("div", { class: "dim", style: "font-size:12px;margin-top:4px",
      text: "Removes the downloaded copy — the file stays listed and can be downloaded again." }));
  } else if (driveCanDownloadNow(row)) {
    box.append(el("button", { class: "btn ghost sm", type: "button", style: "width:100%;justify-content:flex-start",
      onclick: () => downloadNowItem(it) }, icon("download", "icon-sm"), "Download now"));
    if (row && row.last_download_error) box.append(el("div", { style: "font-size:12px;margin-top:4px;color:var(--danger,#f87171)",
      text: "Last attempt failed: " + row.last_download_error }));
  } else {
    box.append(el("div", { class: "dim", style: "font-size:12px", text: "Available online." }));
  }
}
async function freeUpItem(it) {
  try {
    await post("/api/v1/onedrive/free-up?" + qs({ account: App.account, id: it.remote_id }), CAP.onedriveManage);
    toast("Freed up " + (it.name || "file"));
    closeSheet();
    driveLoad();
  } catch (e) { toast("Could not free up: " + e.message, "err"); }
}
async function downloadNowItem(it) {
  try {
    const d = await post("/api/v1/onedrive/download-now?" + qs({ account: App.account, id: it.remote_id }), CAP.onedriveManage);
    toast(d && d.downloaded === false ? "Not downloaded (blocked by policy)" : "Downloaded " + (it.name || "file"));
    closeSheet();
    driveLoad();
  } catch (e) { toast("Could not download: " + e.message, "err"); }
}
// format a Graph media duration (milliseconds) → "m:ss" / "h:mm:ss".
function fmtDur(ms) {
  if (!ms || ms <= 0) return null;
  const s = Math.round(ms / 1000), h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
  const mm = String(m).padStart(2, "0"), ss = String(s % 60).padStart(2, "0");
  return h ? `${h}:${mm}:${ss}` : `${m}:${ss}`;
}
// #564 A5: render the rich metadata from the sidecar preview into the detail sheet.
// Every DriveItem facet OneDrive can return is surfaced (no silent drops): core
// facts, then per-medium sections (Photo/Video/Audio) and GPS, then hashes.
function driveItemMeta(content, it) {
  const p = it.preview; if (!p) return;
  const sec = (title) => content.append(el("h4", { class: "od-sec dim", text: title }));
  const rows = [];
  if (p.mime_type) rows.push(["Kind", p.mime_type]);
  if (p.description) rows.push(["Description", p.description]);
  if (p.created) rows.push(["Created", fmtFullDate(p.created)]);
  if (p.created_by) rows.push(["Created by", p.created_by]);
  if (p.last_modified_by) rows.push(["Modified by", p.last_modified_by]);
  if (p.child_count != null) rows.push(["Items", String(p.child_count)]);
  if (p.package_type) rows.push(["Package", p.package_type]);
  if (p.special_folder) rows.push(["Special folder", p.special_folder]);
  if (p.shared) rows.push(["Sharing", "Shared with others"]);
  if (p.malware) rows.push(["Security", "Malware flagged by Microsoft"]);
  if (rows.length) content.append(kvList(rows));
  // Photo / image — dimensions + EXIF (camera, aperture, focal length, ISO, exposure).
  const img = p.image || {}, ph = p.photo || {}, photo = [];
  if (img.width) photo.push(["Dimensions", `${img.width} × ${img.height} px`]);
  if (ph.takenDateTime) photo.push(["Taken", fmtFullDate(ph.takenDateTime)]);
  const cam = [ph.cameraMake, ph.cameraModel].filter(Boolean).join(" ");
  if (cam) photo.push(["Camera", cam]);
  if (ph.fNumber) photo.push(["Aperture", "ƒ/" + ph.fNumber]);
  if (ph.focalLength) photo.push(["Focal length", ph.focalLength + " mm"]);
  if (ph.iso) photo.push(["ISO", String(ph.iso)]);
  if (ph.exposureDenominator) photo.push(["Exposure", "1/" + Math.round(ph.exposureDenominator) + " s"]);
  if (photo.length) { sec("Photo"); content.append(kvList(photo)); }
  // Video — dimensions, duration, bitrate.
  const vid = p.video || {}, video = [];
  if (vid.width) video.push(["Dimensions", `${vid.width} × ${vid.height} px`]);
  if (vid.duration) video.push(["Duration", fmtDur(vid.duration)]);
  if (vid.bitrate) video.push(["Bitrate", Math.round(vid.bitrate / 1000) + " kbps"]);
  if (video.length) { sec("Video"); content.append(kvList(video)); }
  // Audio — track tags.
  const au = p.audio || {}, audio = [];
  if (au.title) audio.push(["Title", au.title]);
  if (au.artist) audio.push(["Artist", au.artist]);
  if (au.album) audio.push(["Album", au.album]);
  if (au.duration) audio.push(["Duration", fmtDur(au.duration)]);
  if (au.year) audio.push(["Year", String(au.year)]);
  if (audio.length) { sec("Audio"); content.append(kvList(audio)); }
  // GPS — coordinates + altitude + a map link (external navigation, not a fetch).
  const loc = p.location || {};
  if (loc.latitude != null && loc.longitude != null) {
    const lat = (+loc.latitude).toFixed(5), lon = (+loc.longitude).toFixed(5);
    sec("Location");
    const ll = [["Coordinates", `${lat}, ${lon}`]];
    if (loc.altitude != null) ll.push(["Altitude", Math.round(loc.altitude) + " m"]);
    content.append(kvList(ll));
    content.append(el("a", { class: "btn ghost sm", style: "margin-top:8px", href: `https://www.bing.com/maps?cp=${lat}~${lon}&lvl=16`, target: "_blank", rel: "noopener" }, icon("map-pin", "icon-sm"), "View on map"));
  }
  if (p.sha256) { sec("Integrity"); content.append(kvList([["SHA-256", p.sha256]])); }
  if (p.web_url) content.append(el("a", { class: "btn ghost sm", style: "margin-top:8px", href: p.web_url, target: "_blank", rel: "noopener" }, icon("external-link", "icon-sm"), "Open in OneDrive"));
}
function driveTile(it) {
  const folder = it.item_type === "folder";
  const ext = fileExt(it.name);
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const tile = el("div", { class: "card drive-tile rise" + (folder ? " is-folder" : ""), onclick: () => folder ? driveOpen(it.remote_id, it.name) : driveOpenFile(it) });
  let thumb;
  if (!folder && it.has_body && IMAGE_EXT.has(ext))
    thumb = el("img", { class: "drive-thumb-img", src: `/api/v1/body?${qs(q)}`, alt: "", loading: "lazy" });
  else
    thumb = el("div", { class: "drive-thumb", style: folder ? "" : `color:${fileColor(ext)}` }, icon(folder ? "folder" : fileIcon(ext), "icon-lg"));
  const pv = it.preview || {};
  const folderMeta = pv.child_count != null ? `${pv.child_count} ${pv.child_count === 1 ? "item" : "items"}` : "Folder";
  tile.append(...[thumb,
    el("div", { class: "drive-name truncate", text: it.name || "(no name)" }),
    el("div", { class: "drive-meta dim", text: folder ? folderMeta : [fmtSize(it.size), it.remote_mtime ? fmtDate(it.remote_mtime) : ""].filter(Boolean).join(" · ") }),
    (!folder && pv.malware) ? el("span", { class: "drive-flag", title: "Malware flagged by Microsoft", style: "color:var(--danger,#f87171)" }, icon("shield", "icon-sm")) : null,
    (!folder && pv.shared) ? el("span", { class: "drive-flag dim", title: "Shared with others" }, icon("share2", "icon-sm")) : null,
    folder ? null : (MOBILE ? driveModeChip(it) : coverageBadge(it)),
    syncBadge(it),
    (folder && MOBILE && CAP.onedriveMode) ? driveModePill(it.remote_id, it.effective_mode, driveExplicit(it.remote_id), it.name) : null,
    driveActions(it)].filter(Boolean)); // native append stringifies null → drop nulls
  return tile;
}
function driveRow(it) {
  const folder = it.item_type === "folder";
  const ext = fileExt(it.name);
  const q = { account: App.account, service: "onedrive", id: it.remote_id };
  const row = el("div", { class: "list-row", onclick: () => folder ? driveOpen(it.remote_id, it.name) : driveOpenFile(it) },
    el("span", { class: "drive-row-ico", style: folder ? "color:var(--svc-onedrive)" : `color:${fileColor(ext)}` }, icon(folder ? "folder" : fileIcon(ext))),
    el("div", { class: "grow" },
      el("div", { class: "truncate", text: it.name || "(no name)" }),
      el("div", { class: "dim", style: "font-size:12px", text: folder ? ((it.preview || {}).child_count != null ? `${it.preview.child_count} ${it.preview.child_count === 1 ? "item" : "items"}` : "Folder") : (fmtSize(it.size) || "—") })),
    (!folder && (it.preview || {}).malware) ? el("span", { class: "drive-flag", title: "Malware flagged by Microsoft", style: "color:var(--danger,#f87171)" }, icon("shield", "icon-sm")) : null,
    (!folder && (it.preview || {}).shared) ? el("span", { class: "drive-flag dim", title: "Shared with others" }, icon("share2", "icon-sm")) : null,
    folder ? null : (MOBILE ? driveModeChip(it) : coverageBadge(it)),
    syncBadge(it),
    el("span", { class: "dim tnum", style: "font-size:12px", text: fmtDate(it.remote_mtime) }));
  const acts = el("div", { style: "display:flex;gap:4px" });
  acts.append(el("button", { class: "btn ghost sm", title: "Details & access", onclick: (e) => { e.stopPropagation(); openDriveItem(it); } }, icon("info", "icon-sm")));
  if (!folder && it.has_body) acts.append(el("a", { class: "btn ghost sm", href: `/api/v1/body?${qs(q)}`, download: it.name || "", title: "Download", onclick: (e) => e.stopPropagation() }, icon("download", "icon-sm")));
  if (!folder && CAP.share) acts.append(el("button", { class: "btn ghost sm", title: "Share", onclick: (e) => { e.stopPropagation(); doShare(it, e.currentTarget); } }, icon("share2", "icon-sm")));
  if (folder && MOBILE && CAP.onedriveMode) acts.append(driveModePill(it.remote_id, it.effective_mode, driveExplicit(it.remote_id), it.name));
  // #657: in-app write actions (cap-gated; destructive ops biometric-gated on mobile).
  if (!folder && CAP.onedrivewrite) acts.append(el("button", { class: "btn ghost sm", title: "Replace contents", onclick: (e) => { e.stopPropagation(); driveReplace(it); } }, icon("refresh-cw", "icon-sm")));
  if (CAP.onedrivewrite) {
    acts.append(el("button", { class: "btn ghost sm", title: "Rename", onclick: (e) => { e.stopPropagation(); driveRename(it); } }, icon("pencil", "icon-sm")));
    acts.append(el("button", { class: "btn ghost sm", title: "Move to folder", onclick: (e) => { e.stopPropagation(); driveMovePicker(it); } }, icon("folder-input", "icon-sm")));
    acts.append(el("button", { class: "btn ghost sm", title: "Delete", onclick: (e) => { e.stopPropagation(); driveDelete(it); } }, icon("trash-2", "icon-sm")));
  }
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
const hhmm = (d) => d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false });

async function renderCalendarView(view) {
  Cal.events = []; Cal.cursor = new Date(); Cal.view = Cal.view || "agenda"; Cal.stateFilter = "all";
  clear(view).append(
    el("div", { id: "cal-metrics-row", class: "con-metrics-row inset" }),
    el("div", { class: "cal-bar" },
      el("div", { class: "cal-nav" },
        el("button", { class: "btn ghost sm icon-only", title: "Previous", onclick: () => calNav(-1) }, icon("chevron-left", "icon-sm")),
        el("button", { class: "btn ghost sm", onclick: () => { Cal.cursor = new Date(); calRender(); } }, "Today"),
        el("button", { class: "btn ghost sm icon-only", title: "Next", onclick: () => calNav(1) }, icon("chevron-right", "icon-sm"))),
      el("div", { id: "cal-label", class: "cal-label" }),
      el("div", { class: "spacer", style: "flex:1" }),
      CAP.calendarwrite ? el("button", { class: "btn sm primary", title: "Create a new event", onclick: () => openComposeEvent() }, icon("calendar", "icon-sm"), "New event") : null,
      verifyButton(() => renderCalendarView(view)),
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
    const [d, act] = await Promise.all([
      api("/api/v1/items?" + qs({ account: App.account, service: "calendar", limit: 1000 })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 30 })).catch(() => ({ runs: [] })),
    ]);
    const items = d.items || [];
    Cal.raw = items.filter(it => it.item_type === "event");
    // calendar colour map (#565 B5): calendar id -> hexColor, for colour-coding
    Cal.calColor = new Map(
      items.filter(it => it.item_type === "calendar")
        .map(it => [it.remote_id, (it.preview || {}).hex_color])
        .filter(([, c]) => c));
    Cal.runs = act.runs || [];
    Cal.events = items.filter(it => it.item_type === "event").map(it => {
      const p = it.preview || {};
      const start = evDate(p.start, p.start_tz) || (it.remote_mtime ? new Date(it.remote_mtime) : null);
      return {
        it, calId: it.parent_remote_id, recur: p.recurrence || null,
        subject: it.name || "(no title)", start, end: evDate(p.end, p.end_tz),
        allDay: !!p.all_day, location: p.location || "",
      };
    }).filter(e => e.start);
    App.counts.calendar = d.total ?? Cal.events.length; updateNavCounts();
    calRenderMetrics(); calRender();
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load calendar" }), el("p", { text: e.message }))); }
}
function calRenderMetrics() {
  const now = Date.now();
  const upcoming = Cal.events.filter(e => e.start && e.start.getTime() >= now).length;
  fillMetrics($("#cal-metrics-row"), [
    { icon: "calendar", value: Cal.events.length, label: "Total events", sub: "archived" },
    { icon: "clock", value: upcoming, label: "Upcoming", sub: "start in the future", tone: upcoming ? "ok" : "" },
    integrityMetric(Cal.raw || []),
    lastActivityMetric(Cal.runs),
  ]);
}
// colour of an event = its calendar's hexColor (#565 B5), else the service tint
const eventColor = (ev) => (Cal.calColor && Cal.calColor.get(ev.calId)) || "var(--svc-calendar)";
// human-readable recurrence summary for the detail sheet (#565 B5)
const DOW_MAP = { sunday: 0, monday: 1, tuesday: 2, wednesday: 3, thursday: 4, friday: 5, saturday: 6 };
// Graph reminderMinutesBeforeStart → human text ("15 minutes before" / "1 hour before").
function reminderText(m) {
  if (m == null) return null;
  if (m <= 0) return "At start";
  if (m % 1440 === 0) { const d = m / 1440; return `${d} day${d > 1 ? "s" : ""} before`; }
  if (m % 60 === 0) { const h = m / 60; return `${h} hour${h > 1 ? "s" : ""} before`; }
  return `${m} minutes before`;
}
function recurText(rec) {
  if (!rec) return null;
  const p = rec.pattern || {}, r = rec.range || {}, iv = p.interval || 1, t = p.type || "";
  let base;
  if (t === "daily") base = iv === 1 ? "Daily" : `Every ${iv} days`;
  else if (t === "weekly") {
    const ds = (p.daysOfWeek || []).map(d => d[0].toUpperCase() + d.slice(1, 3)).join(", ");
    base = (iv === 1 ? "Weekly" : `Every ${iv} weeks`) + (ds ? ` on ${ds}` : "");
  } else if (t.toLowerCase().includes("monthly")) base = iv === 1 ? "Monthly" : `Every ${iv} months`;
  else if (t.toLowerCase().includes("yearly")) base = iv === 1 ? "Yearly" : `Every ${iv} years`;
  else base = "Repeats";
  if (r.type === "endDate" && r.endDate) base += ` until ${r.endDate}`;
  else if (r.type === "numbered" && r.numberOfOccurrences) base += `, ${r.numberOfOccurrences} times`;
  return base;
}
// One occurrence instance derived from a recurring master (#565 B5).
function mkOcc(ev, when, dur) {
  const start = new Date(when);
  return { it: ev.it, calId: ev.calId, recur: ev.recur, subject: ev.subject, start, end: new Date(start.getTime() + dur), allDay: ev.allDay, location: ev.location, occurrence: true };
}
// Expand a recurring master into its occurrences within [rs, re] (best-effort:
// daily / weekly / absolute-monthly / absolute-yearly; honours count/until).
// Exceptions aren't captured (the /me/events model holds only the master).
function expandRecurrence(ev, rs, re) {
  const rec = ev.recur, start = ev.start;
  if (!rec || !start) return [];
  const p = rec.pattern || {}, r = rec.range || {}, iv = Math.max(1, p.interval || 1), t = p.type || "";
  const dur = ev.end && ev.end > start ? (ev.end - start) : 36e5;
  const until = r.type === "endDate" && r.endDate ? new Date(r.endDate + "T23:59:59Z") : null;
  const limit = r.type === "numbered" ? (r.numberOfOccurrences || 0) : 0;
  const days = (p.daysOfWeek || []).map(d => DOW_MAP[String(d).toLowerCase()]).filter(n => n != null);
  const out = []; let count = 0;
  const push = (d) => { // returns false to stop (limit reached)
    if (limit && count >= limit) return false;
    count++;
    if (d >= rs && d <= re && d >= start) out.push(mkOcc(ev, d, dur));
    return true;
  };
  if (t === "daily" || t === "weekly") {
    const wantDays = days.length ? days : [start.getDay()];
    for (let d = startOfDay(start), g = 0; g < 20000 && d <= re; g++, d = new Date(d.getTime() + DAY_MS)) {
      if (until && d > until) break;
      let emit;
      if (t === "daily") emit = Math.round((startOfDay(d) - startOfDay(start)) / DAY_MS) % iv === 0;
      else emit = wantDays.includes(d.getDay()) && Math.floor((startOfWeek(d) - startOfWeek(start)) / (7 * DAY_MS)) % iv === 0;
      if (!emit) continue;
      const occ = new Date(d); occ.setHours(start.getHours(), start.getMinutes(), start.getSeconds(), 0);
      if (!push(occ)) break;
    }
  } else if (t.toLowerCase().includes("monthly")) {
    const dom = p.dayOfMonth || start.getDate();
    for (let m = new Date(start.getFullYear(), start.getMonth(), 1), g = 0; g < 2400 && m <= re; g++, m.setMonth(m.getMonth() + iv)) {
      const occ = new Date(m); occ.setDate(dom); occ.setHours(start.getHours(), start.getMinutes(), start.getSeconds(), 0);
      if (until && occ > until) break;
      if (!push(occ)) break;
    }
  } else if (t.toLowerCase().includes("yearly")) {
    const mo = (p.month || start.getMonth() + 1) - 1, dom = p.dayOfMonth || start.getDate();
    for (let y = start.getFullYear(), g = 0; g < 400 && y <= re.getFullYear(); g++, y += iv) {
      const occ = new Date(start); occ.setFullYear(y, mo, dom);
      if (until && occ > until) break;
      if (!push(occ)) break;
    }
  }
  return out;
}
// All event instances overlapping [rs, re]: recurring masters expanded, singles
// passed through. Filtered by the active 4-state filter.
function expandRange(rs, re) {
  const out = [];
  for (const ev of Cal.events) {
    if (!stateMatch(ev.it, Cal.stateFilter)) continue;
    if (ev.recur) out.push(...expandRecurrence(ev, rs, re));
    else {
      const end = ev.end || new Date(ev.start.getTime() + 36e5);
      if (ev.start < re && end > rs) out.push(ev);
    }
  }
  return out;
}
// Pre-bucket the visible window's occurrences by day (so the per-cell render is
// cheap); month/week call this once before laying out cells.
function buildBucket(rs, re) {
  const map = new Map();
  for (const occ of expandRange(rs, re)) {
    const s = startOfDay(occ.start), e = startOfDay(occ.end || occ.start);
    for (let d = new Date(s), g = 0; d <= e && g < 90; g++, d = new Date(d.getTime() + DAY_MS)) {
      const k = ymd(d); let a = map.get(k); if (!a) { a = []; map.set(k, a); } a.push(occ);
    }
  }
  for (const a of map.values()) a.sort((x, y) => x.start - y.start);
  Cal.bucket = map;
}
function eventsForDay(day) { return (Cal.bucket && Cal.bucket.get(ymd(day))) || []; }
function calRender() {
  const body = $("#cal-body"); if (!body) return; clear(body);
  if (!Cal.events.length && Cal.view === "agenda") { body.append(el("div", { class: "empty" }, emptyArt("empty-calendar"), el("h3", { text: "No events archived" }), el("p", { text: "Run a backup to populate your calendar." }))); return; }
  if (Cal.events.length) body.append(stateFilterBar(Cal.events.map(e => e.it), Cal.stateFilter, k => { Cal.stateFilter = k; calRender(); }));
  if (Cal.view === "month") calRenderMonth(body);
  else if (Cal.view === "week") calRenderWeek(body);
  else calRenderAgenda(body);
}
function calRenderMonth(body) {
  const cur = Cal.cursor;
  $("#cal-label").textContent = MONTHS[cur.getMonth()] + " " + cur.getFullYear();
  const first = new Date(cur.getFullYear(), cur.getMonth(), 1);
  const gridStart = startOfWeek(first);
  buildBucket(gridStart, new Date(gridStart.getTime() + 42 * DAY_MS)); // #565: expand recurrences once
  const todayKey = ymd(new Date());
  const grid = el("div", { class: "cal-month" });
  DAY_NAMES.forEach(n => grid.append(el("div", { class: "cal-dow", text: n })));
  for (let i = 0; i < 42; i++) {
    const day = new Date(gridStart.getTime() + i * DAY_MS);
    const outside = day.getMonth() !== cur.getMonth();
    const cell = el("div", { class: "cal-cell" + (outside ? " outside" : "") + (ymd(day) === todayKey ? " today" : "") });
    cell.append(el("div", { class: "cal-daynum", text: String(day.getDate()) }));
    const evs = eventsForDay(day);
    evs.slice(0, 3).forEach(ev => cell.append(el("button", { class: "cal-chip", style: `--svc:${eventColor(ev)}`, title: ev.subject, onclick: () => openEventSheet(ev) },
      ev.allDay ? null : el("span", { class: "cal-chip-time", text: hhmm(ev.start) }), el("span", { class: "truncate", text: ev.subject }))));
    if (evs.length > 3) cell.append(el("div", { class: "cal-more", text: "+" + (evs.length - 3) + " more" }));
    grid.append(cell);
  }
  body.append(grid);
}
function calRenderWeek(body) {
  const ws = startOfWeek(Cal.cursor), days = Array.from({ length: 7 }, (_, i) => new Date(ws.getTime() + i * DAY_MS));
  buildBucket(ws, new Date(ws.getTime() + 7 * DAY_MS)); // #565: expand recurrences once
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
    eventsForDay(d).filter(e => e.allDay).forEach(ev => cell.append(el("button", { class: "cal-chip", style: `--svc:${eventColor(ev)}`, title: ev.subject, onclick: () => openEventSheet(ev) }, el("span", { class: "truncate", text: ev.subject }))));
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
      col.append(el("button", { class: "cal-event", style: `top:${top}px;height:${h}px;--svc:${eventColor(ev)}`, onclick: () => openEventSheet(ev) },
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
  const evs = Cal.events.filter(e => stateMatch(e.it, Cal.stateFilter)).sort((a, b) => a.start - b.start);
  if (!evs.length) { body.append(el("div", { class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No events" }), el("p", { text: "No events have this backup status." }))); return; }
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
      el("span", { class: "cal-dot", style: `background:${eventColor(ev)}` }),
      el("div", { class: "grow" }, el("div", { class: "truncate", text: ev.subject }),
        ev.location ? el("div", { class: "dim truncate", style: "font-size:12px" }, icon("map-pin", "icon-sm"), ev.location) : null),
      coverageBadge(ev.it)));
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
  sheet.prepend(sheetNet());
  sheetEl = el("div", {}, scrim, sheet); document.body.append(sheetEl);
  // structured detail rendered via textContent only (never innerHTML on cloud data)
  const kv = el("dl", { class: "kv" });
  const add = (k, v, ic) => { if (!v) return; kv.append(el("dt", {}, ic ? icon(ic, "icon-sm") : null, el("span", { text: k })), el("dd", { text: v })); };
  add("When", ev.allDay ? ev.start.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long", year: "numeric" }) + " · all day"
    : ev.start.toLocaleDateString([], { weekday: "long", day: "numeric", month: "long", year: "numeric" }) + " · " + hhmm(ev.start) + (ev.end ? " – " + hhmm(ev.end) : ""), "clock");
  add("Location", ev.location, "map-pin");
  try {
    const full = await api("/api/v1/body?" + qs(q));
    const org = ((full.organizer || {}).emailAddress || {});
    add("Organizer", org.name || org.address, "users");
    const att = (full.attendees || []).map(a => {
      const e = a.emailAddress || {}, r = (a.status || {}).response;
      const who = e.name || e.address; if (!who) return null;
      return who + (r && r !== "none" && r !== "notResponded" ? ` (${r})` : "");
    }).filter(Boolean);
    if (att.length) add("Attendees", att.join(", "), "users");
    // #565 B5: rich event detail
    add("Repeats", recurText(full.recurrence || ev.recur), "rotate-ccw");
    const resp = (full.responseStatus || {}).response;
    if (resp && resp !== "none" && resp !== "organizer") add("My response", resp[0].toUpperCase() + resp.slice(1), "check-square");
    if (full.showAs) add("Shown as", full.showAs, "clock");
    if (full.sensitivity && full.sensitivity !== "normal") add("Sensitivity", full.sensitivity, "shield");
    if (full.importance && full.importance !== "normal") add("Importance", full.importance, "shield");
    if (full.isCancelled) add("Status", "Cancelled", "x");
    if (full.hasAttachments) add("Attachments", "Backed up with the event", "paperclip");
    if (full.isReminderOn && full.reminderMinutesBeforeStart != null) add("Reminder", reminderText(full.reminderMinutesBeforeStart), "clock");
    if ((full.type === "occurrence" || full.type === "exception") && !full.recurrence) add("Series", "Part of a recurring series", "rotate-ccw");
    // event description is HTML → extract plain text safely (DOMParser runs no scripts, loads nothing)
    const html = (full.body || {}).content || "";
    const tail = [];
    // Teams / online-meeting join link
    const joinUrl = (full.onlineMeeting || {}).joinUrl || full.onlineMeetingUrl;
    if (joinUrl) tail.push(el("a", { class: "btn sm primary", style: "margin-top:12px", href: joinUrl, target: "_blank", rel: "noopener" }, icon("external-link", "icon-sm"), "Join online meeting"));
    // real category chips, coloured from the master-category map if present
    const cats = full.categories || [];
    if (cats.length) {
      const row = el("div", { class: "mr-chips", style: "margin-top:12px;display:flex;gap:6px;flex-wrap:wrap" });
      cats.forEach(c => row.append(el("span", { class: "chip", text: c })));
      tail.push(row);
    }
    // Full event description rendered inline in the APP's own design (dark card,
    // app typography) — not the white archive-styled /view, and no extra button.
    // Cloud HTML → plain text via DOMParser (no innerHTML on cloud data).
    const notes = [];
    if (html) {
      const txt = new DOMParser().parseFromString(html, "text/html").body.textContent.replace(/ /g, " ").replace(/\n{3,}/g, "\n\n").trim();
      if (txt) notes.push(el("div", { class: "sb-section", text: "Description" }), el("div", { class: "evt-desc", text: txt }));
    }
    clear(content).append(kv, ...notes, ...tail);
  } catch { clear(content).append(kv); }
  // #565 B7: live write actions (edit / respond / delete), cap-gated
  if (CAP.calendarwrite) {
    const acts = el("div", { style: "margin-top:16px;display:flex;gap:8px;flex-wrap:wrap" });
    acts.append(el("button", { class: "btn ghost sm", onclick: () => { closeSheet(); openComposeEvent({ id: ev.it.remote_id, subject: ev.subject, start: ev.start, end: ev.end, location: ev.location }); } }, icon("info", "icon-sm"), "Edit"));
    ["accept", "tentative", "decline"].forEach(r => acts.append(el("button", { class: "btn ghost sm", title: "Respond: " + r, onclick: () => eventRespondInline(ev, r, acts) }, r[0].toUpperCase() + r.slice(1))));
    acts.append(el("button", { class: "btn ghost sm", style: "color:var(--danger,#f87171)", onclick: () => deleteEvent(ev) }, icon("trash-2", "icon-sm"), "Delete"));
    content.append(acts);
  }
}
let sheetEl = null;
function closeSheet() { if (sheetEl) { sheetEl.remove(); sheetEl = null; } }

// #565 B7: create / edit an event. `opts.id` => edit (PATCH), else create (POST).
function openComposeEvent(opts = {}) {
  if (!CAP.calendarwrite) return;
  const o = opts || {};
  const field = (label, input) => el("label", { class: "cmp-field" }, el("span", { class: "cmp-label", text: label }), input);
  // a JS Date -> the value a <input type=datetime-local> expects (local wall-clock)
  const toLocal = (d) => { if (!d) return ""; const z = new Date(d.getTime() - d.getTimezoneOffset() * 60000); return z.toISOString().slice(0, 16); };
  const subjIn = el("input", { class: "input", id: "cev-subject", placeholder: "Title", value: o.subject || "" });
  const startIn = el("input", { class: "input", type: "datetime-local", id: "cev-start", value: toLocal(o.start) });
  const endIn = el("input", { class: "input", type: "datetime-local", id: "cev-end", value: toLocal(o.end) });
  const locIn = el("input", { class: "input", id: "cev-loc", placeholder: "Location", value: o.location || "" });
  const bodyIn = el("textarea", { class: "input cmp-textarea", id: "cev-body", placeholder: "Notes", rows: "5" });
  if (o.body) bodyIn.value = o.body;
  const content = el("div", { class: "compose" },
    field("Title", subjIn), field("Start", startIn), field("End", endIn),
    field("Location", locIn), field("Notes", bodyIn));
  // Inline in the calendar body (live.com-style) — the toolbar stays above; not
  // an overlay sheet. Discard re-renders the calendar.
  const box = $("#cal-body");
  const head = el("div", { class: "cmp-inline-head" },
    el("span", { class: "cmp-inline-title truncate" }, o.id ? "Edit event" : "New event"),
    el("div", { style: "flex:1" }),
    el("button", { class: "btn ghost sm", type: "button", onclick: () => calLoad() }, "Discard"),
    el("button", { class: "btn primary sm", type: "button", onclick: (e) => composeEventSubmit(e.currentTarget, o.id) }, icon("calendar", "icon-sm"), o.id ? "Save" : "Create"));
  if (box) {
    clear(box).append(head, content);
    setTimeout(() => subjIn.focus(), 60);
  } else {
    openSheet(o.id ? "Edit event" : "New event", content);
  }
}
async function composeEventSubmit(btn, id) {
  const subject = ($("#cev-subject").value || "").trim();
  const startV = $("#cev-start").value, endV = $("#cev-end").value;
  const loc = ($("#cev-loc").value || "").trim(), body = ($("#cev-body").value || "").trim();
  if (!subject) { toast("Add a title", "err"); return; }
  if (!id && !startV) { toast("Pick a start time", "err"); return; }
  const toUtc = (v) => v ? new Date(v).toISOString() : "";  // local wall-clock -> UTC instant
  const params = { account: App.account, subject, location: loc, body, tz: "UTC" };
  if (startV) params.start = toUtc(startV);
  if (endV) params.end = toUtc(endV);
  if (id) params.id = id;
  btn.disabled = true;
  try {
    await post((id ? "/api/v1/calendar/update?" : "/api/v1/calendar/create?") + qs(params), CAP.calendarwrite);
    toast(id ? "Event updated" : "Event created");
    closeSheet(); calLoad();
  } catch (e) { toast("Failed: " + e.message, "err"); btn.disabled = false; }
}
// Inline, animated response chooser (no popup): Accept/Tentative/Decline opens a
// smooth expander offering "Send now" or "Add a message" (Outlook-style). The
// daemon respond endpoint already takes an optional comment.
function eventRespondInline(ev, response, host) {
  host.parentNode.querySelectorAll(".evt-respond").forEach(p => p.remove()); // one at a time
  const cap = response[0].toUpperCase() + response.slice(1);
  const ta = el("textarea", { class: "input cmp-textarea evt-respond-msg", placeholder: "Add a message…", rows: "3" });
  const msgWrap = el("div", { class: "evt-respond-msg-wrap" }, ta);
  const doSend = async (withMsg) => {
    const comment = withMsg ? (ta.value || "").trim() : "";
    try {
      await post("/api/v1/calendar/respond?" + qs({ account: App.account, id: ev.it.remote_id, response, comment }), CAP.calendarwrite);
      toast(cap + " sent" + (comment ? " with a message" : "")); closeSheet(); calLoad();
    } catch (e) { toast("Failed: " + e.message, "err"); }
  };
  const sendWith = el("button", { class: "btn primary sm", style: "margin-top:8px", onclick: () => doSend(true) }, icon("send", "icon-sm"), "Send with message");
  const sendWithWrap = el("div", { class: "evt-respond-send2", style: "display:none" }, sendWith);
  const panel = el("div", { class: "evt-respond" },
    el("div", { class: "evt-respond-h", text: `${cap} — send your response?` }),
    el("div", { class: "evt-respond-actions" },
      el("button", { class: "btn primary sm", onclick: () => doSend(false) }, icon("send", "icon-sm"), "Send now"),
      el("button", { class: "btn ghost sm", onclick: () => { msgWrap.classList.add("open"); sendWithWrap.style.display = "block"; setTimeout(() => ta.focus(), 60); } }, icon("plus", "icon-sm"), "Add a message"),
      el("button", { class: "btn ghost sm", onclick: () => { panel.classList.remove("open"); setTimeout(() => panel.remove(), 220); } }, "Cancel")),
    msgWrap, sendWithWrap);
  host.after(panel);
  requestAnimationFrame(() => panel.classList.add("open"));
}
async function deleteEvent(ev) {
  if (!confirmDestructive("Delete this event? This removes it from your calendar.")) return;
  try {
    await post("/api/v1/calendar/delete?" + qs({ account: App.account, id: ev.it.remote_id }), CAP.calendarwrite);
    toast("Event deleted"); closeSheet(); calLoad();
  } catch (e) { toast("Failed: " + e.message, "err"); }
}

/* ---------------------------------------------------------------- contacts (avatar cards) */
const Contacts = { all: [], selected: null, filter: "all", q: "", sort: "name", lastSync: null, runs: [], retentionDays: null };
const conLetter = (it) => ((it.name || "#").trim()[0] || "#").toUpperCase();
const conPrev = (it) => it.preview || {};
async function renderContactsView(view) {
  Object.assign(Contacts, { all: [], selected: null, filter: "all", q: "", sort: "name", status: null });
  clear(view).append(el("div", { class: "con-page" },
    // top metric row (real data: counts + integrity from verify + sync health/activity).
    // The page title lives in the top-bar breadcrumb and the counts live in these
    // cards + the sidebar sub-nav, so no separate hero band is needed (saves 3 rows).
    el("div", { id: "con-metrics-row", class: "con-metrics-row top" }),
    // toolbar: search + filters + sort + verify + sync-log
    el("div", { class: "con-toolbar" },
      el("div", { class: "tb-search" }, icon("search", "icon-sm"),
        el("input", { id: "con-search", placeholder: "Search by name, email, or company…", oninput: () => { Contacts.q = ($("#con-search").value || "").trim().toLowerCase(); contactsRenderList(); } })),
      el("div", { class: "spacer", style: "flex:1" }),
      el("label", { class: "tb-sort" }, icon("arrow-down-up", "icon-sm"),
        el("select", { class: "input", onchange: (e) => { Contacts.sort = e.target.value; contactsRenderList(); } },
          el("option", { value: "name", text: "Name A–Z" }),
          el("option", { value: "company", text: "Company A–Z" }),
          el("option", { value: "recent", text: "Recently archived" }))),
      CAP.contactwrite ? el("button", { class: "btn sm primary", title: "Create a new contact", onclick: () => openComposeContact() }, icon("users", "icon-sm"), "New contact") : null,
      CAP.verify ? el("button", { class: "btn sm", title: "Re-hash every archived record and check integrity", onclick: (e) => contactsVerify(e.currentTarget) }, icon("shield-check", "icon-sm"), "Verify") : null,
      el("button", { class: "btn sm", title: "View sync log", onclick: () => go("overview") }, icon("clock", "icon-sm"), "Sync log")),
    // master–detail: directory list | record detail
    el("div", { id: "con-layout", class: "con-layout" },
      el("div", { class: "con-listwrap" },
        el("div", { id: "con-list", class: "con-list" }), el("div", { id: "con-az", class: "con-az" })),
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
    fillSubnavCounts("contacts", Contacts.all); contactsRenderMetrics(); contactsRenderList();
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
    fillSubnavCounts("contacts", Contacts.all); contactsRenderMetrics(); contactsRenderList();
    if (Contacts.selected) { const s = Contacts.all.find(x => x.remote_id === Contacts.selected.remote_id); if (s) { Contacts.selected = s; renderContactDetail(s); } }
  } catch (e) { toast("Verify failed: " + e.message, "err"); } finally { btn.disabled = false; }
}
// #566 A5: live contact write — create / edit (cap-gated). `opts.id` => edit.
async function openComposeContact(opts = {}) {
  if (!CAP.contactwrite) return;
  let o = opts || {};
  if (o.id) {                                  // editing: pull the full archived record so every field is prefilled
    try {
      const c = await api("/api/v1/body?" + qs({ account: App.account, service: "contacts", id: o.id }));
      o = Object.assign({}, contactFromBody(c), { id: o.id });
    } catch { toast("Could not load contact for editing", "err"); }
  }
  const field = (label, input) => el("label", { class: "cmp-field" }, el("span", { class: "cmp-label", text: label }), input);
  const inp = (id, ph, v, type) => el("input", { class: "input", id, placeholder: ph || "", value: v || "", type: type || "text" });
  const given = inp("ccon-given", "First name", o.given);
  const surname = inp("ccon-surname", "Last name", o.surname);
  const email = inp("ccon-email", "name@example.com", o.email);
  const mobile = inp("ccon-mobile", "Mobile", o.mobile);
  const bphone = inp("ccon-bphone", "Business phone", o.business_phone);
  const company = inp("ccon-company", "Company", o.company);
  const job = inp("ccon-job", "Job title", o.job);
  const bday = inp("ccon-bday", "", o.birthday, "date");
  const notes = el("textarea", { class: "input cmp-textarea", id: "ccon-notes", placeholder: "Notes", rows: "4" });
  if (o.notes) notes.value = o.notes;
  const content = el("div", { class: "compose" },
    field("First name", given), field("Last name", surname),
    field("Email", email), field("Mobile", mobile), field("Business phone", bphone),
    field("Company", company), field("Job title", job), field("Birthday", bday),
    field("Notes", notes));
  // Render INLINE in the detail pane (live.com-style), like the mail composer —
  // not an overlay sheet. Header carries the title + Discard/Save.
  const box = $("#con-detail");
  const discard = () => renderContactDetail(Contacts.selected || null);
  const head = el("header", { class: "con-detail-head cmp-inline-head" },
    el("span", { class: "cmp-inline-title truncate" }, o.id ? "Edit contact" : "New contact"),
    el("div", { style: "flex:1" }),
    el("button", { class: "btn ghost sm", type: "button", onclick: discard }, "Discard"),
    el("button", { class: "btn primary sm", type: "button", onclick: (e) => composeContactSubmit(e.currentTarget, o.id) }, icon("users", "icon-sm"), o.id ? "Save" : "Create"));
  if (box) {
    clear(box).append(head, content);
    setTimeout(() => given.focus(), 60);
  } else {
    openSheet(o.id ? "Edit contact" : "New contact", content); // fallback (no detail pane)
  }
}
// map an archived contact body JSON -> the compose form's field values
function contactFromBody(c) {
  return {
    given: c.givenName || "", surname: c.surname || "",
    email: ((c.emailAddresses || [])[0] || {}).address || "",
    mobile: c.mobilePhone || "", business_phone: (c.businessPhones || [])[0] || "",
    company: c.companyName || "", job: c.jobTitle || "",
    birthday: typeof c.birthday === "string" ? c.birthday.slice(0, 10) : "",
    notes: c.personalNotes || "",
  };
}
async function composeContactSubmit(btn, id) {
  const v = (s) => ($("#" + s).value || "").trim();
  const params = {
    account: App.account, given: v("ccon-given"), surname: v("ccon-surname"),
    email: v("ccon-email"), mobile: v("ccon-mobile"), business_phone: v("ccon-bphone"),
    company: v("ccon-company"), job: v("ccon-job"), notes: v("ccon-notes"),
  };
  const day = v("ccon-bday");
  if (day) params.birthday = day + "T00:00:00Z";
  const dn = [params.given, params.surname].filter(Boolean).join(" ");   // keep it identifiable when only one name set
  if (dn) params.display_name = dn;
  if (!params.given && !params.surname && !params.email && !params.company) { toast("Add at least a name, email or company", "err"); return; }
  if (id) params.id = id;
  btn.disabled = true;
  try {
    await post((id ? "/api/v1/contact/update?" : "/api/v1/contact/create?") + qs(params), CAP.contactwrite);
    toast(id ? "Contact updated" : "Contact created");
    await contactsReload();
    // create leaves nothing selected → clear the inline form back to the empty
    // detail state (edit is already re-rendered by contactsReload).
    if (!id) renderContactDetail(Contacts.selected || null);
  } catch (e) { toast("Failed: " + e.message, "err"); btn.disabled = false; }
}
async function deleteContact(it) {
  if (!confirmDestructive("Delete this contact? This removes it from your Microsoft 365 account.")) return;
  try {
    await post("/api/v1/contact/delete?" + qs({ account: App.account, id: it.remote_id }), CAP.contactwrite);
    toast("Contact deleted");
    if (Contacts.selected && Contacts.selected.remote_id === it.remote_id) contactBack();
    contactsReload();
  } catch (e) { toast("Failed: " + e.message, "err"); }
}
// re-fetch the directory after a live write and re-render list + metrics + detail
async function contactsReload() {
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "contacts", limit: 1000 }));
    Contacts.all = (d.items || []).filter(it => it.item_type !== "folder");
    App.counts.contacts = d.total ?? Contacts.all.length; updateNavCounts();
    fillSubnavCounts("contacts", Contacts.all); contactsRenderMetrics(); contactsRenderList();
    if (Contacts.selected) { const s = Contacts.all.find(x => x.remote_id === Contacts.selected.remote_id); if (s) { Contacts.selected = s; renderContactDetail(s); } else contactBack(); }
  } catch (e) { toast("Reload failed: " + e.message, "err"); }
}
function contactsFiltered() {
  let rows = Contacts.all;
  const f = svcFilter("contacts");
  if (f === "email") rows = rows.filter(it => conPrev(it).email);
  else if (f === "company") rows = rows.filter(it => conPrev(it).company);
  else if (f === "restore") rows = rows.filter(it => it.has_body);
  else if (STATE_KEYS.has(f)) rows = rows.filter(it => stateKey(it) === f);
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
/* ---- shared archive-mockup primitives (metric row + integrity + verify), reused by every view */
// per-view integrity from each item's real verify_status (no extra backend call).
// Denominator = records actually checked (verify_status set), matching the
// store's verify_counts: cloud-only OneDrive placeholders the pass skips never
// get a status, so they're correctly excluded (not counted as failures).
function integrityOf(items) {
  const checked = items.filter(it => it.verify_status).length;
  const verified = items.filter(it => it.verify_status === "verified").length;
  return { checked, verified, pct: checked ? Math.round((verified / checked) * 100) : null };
}
// integrity KPI card spec from a list of items (honest: "—" until verify has run)
function integrityMetric(items) {
  const ig = integrityOf(items);
  return { icon: "shield-check", value: ig.pct == null ? "—" : ig.pct + "%", label: "Integrity verified",
    sub: ig.pct == null ? "Run verify to check" : `${ig.verified} of ${ig.checked} records`,
    tone: ig.pct == null ? "" : ig.pct === 100 ? "ok" : "warn" };
}
// last archive activity KPI from runs (newest)
function lastActivityMetric(runs) {
  const r = (runs || [])[0];
  return { icon: "clock", value: r ? fmtDate(r.finished_at) : "—", label: "Last archive activity",
    sub: r ? `${r.kind} · ${fmtFullDate(r.finished_at)}` : "no runs recorded yet" };
}
// small green "Verified" / amber "Changed" / red "Check failed" chip from verify_status
function verifyChip(it) {
  if (it.verify_status === "verified") return el("span", { class: "chip ok" }, icon("shield-check", "icon-sm"), "Verified");
  if (it.verify_status === "changed") return el("span", { class: "chip warn" }, icon("shield", "icon-sm"), "Changed");
  if (it.verify_status === "failed") return el("span", { class: "chip err" }, icon("shield", "icon-sm"), "Check failed");
  return null;
}
// cap-gated Verify toolbar button (null when the server is read-only)
function verifyButton(refreshFn) {
  if (!CAP.verify) return null;
  return el("button", { class: "btn sm", title: "Re-hash every archived record and check integrity", onclick: (e) => runVerifyThen(e.currentTarget, refreshFn) }, icon("shield-check", "icon-sm"), "Verify");
}
async function runVerifyThen(btn, refreshFn) {
  btn.disabled = true;
  try {
    const r = await post("/api/v1/verify?" + qs({ account: App.account }), CAP.verify);
    toast(`Integrity: ${r.verified}/${r.checked} records verified`);
    await refreshFn();
  } catch (e) { toast("Verify failed: " + e.message, "err"); } finally { btn.disabled = false; }
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
    conMetric("rotate-ccw", restore, (MOBILE ? "Has content" : "Restore-ready"), restore === total ? "100% of archive" : `${restore} of ${total} archived`, "ok"),
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
// avatar that shows the archived contact photo when one exists, falling back to
// initials (also if the <img> fails to load — never a broken-image glyph).
function contactAvatar(it, extra) {
  const cls = "avatar con-av" + (extra ? " " + extra : "");
  if (conPrev(it).has_photo) {
    return el("img", {
      class: cls + " con-photo", alt: "", loading: "lazy",
      src: "/api/v1/contact/photo?" + qs({ account: App.account, id: it.remote_id }),
      onerror: (e) => e.currentTarget.replaceWith(el("span", { class: cls, text: initials(it.name) })),
    });
  }
  return el("span", { class: cls, text: initials(it.name) });
}
function contactRow(it) {
  const p = conPrev(it);
  const sub = [p.job, p.company].filter(Boolean).join(" · ") || p.email || "";
  const sel = Contacts.selected && Contacts.selected.remote_id === it.remote_id;
  return el("button", { class: "con-row" + (sel ? " active" : ""), dataset: { id: it.remote_id }, onclick: () => contactSelect(it) },
    contactAvatar(it),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "con-name truncate", text: it.name || "(no name)" }),
      sub ? el("div", { class: "con-sub truncate", text: sub }) : null),
    el("span", { class: "con-row-meta" },
      p.email ? el("span", { class: "con-dot", title: "Has email", style: "background:var(--svc-contacts)" }) : null,
      coverageBadge(it)));
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
  ["Address", (c) => { const a = c.businessAddress || c.homeAddress || c.otherAddress || {}; return !!(a.street || a.city || a.postalCode); }],
  ["Birthday", (c) => !!c.birthday],
  ["IM", (c) => (c.imAddresses || []).length > 0],
  ["Categories", (c) => (c.categories || []).length > 0],
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
  // #566 A5: live write actions (edit / delete), cap-gated
  if (CAP.contactwrite) {
    actions.append(el("button", { class: "btn ghost sm", title: "Edit this contact in your account", onclick: () => openComposeContact({ id: it.remote_id }) }, icon("info", "icon-sm"), "Edit"));
    actions.append(el("button", { class: "btn ghost sm", style: "color:var(--danger,#f87171)", title: "Delete from your account", onclick: () => deleteContact(it) }, icon("trash-2", "icon-sm"), "Delete"));
  }
  const verified = it.verify_status === "verified";
  box.append(el("header", { class: "con-detail-head" },
    el("button", { class: "con-back btn ghost sm", title: "Back", onclick: contactBack }, icon("chevron-left", "icon-sm")),
    contactAvatar(it, "lg"),
    el("div", { class: "grow", style: "min-width:0" },
      el("h2", { class: "con-detail-name truncate", text: it.name || "(no name)" }),
      p.email ? el("button", { class: "con-detail-email truncate", title: "Copy email", onclick: (e) => { navigator.clipboard?.writeText(p.email).then(() => toast("Email copied")).catch(() => {}); } }, el("span", { class: "truncate", text: p.email }), icon("share2", "icon-sm")) : (sub ? el("div", { class: "con-detail-sub truncate", text: sub }) : null),
      el("div", { class: "con-detail-chips" }, readonlyChip(),
        (CAP.restore && it.has_body) ? el("span", { class: "chip ok" }, icon("rotate-ccw", "icon-sm"), (MOBILE ? "Has content" : "Restore-ready"))
          : it.has_body ? el("span", { class: "chip muted" }, icon("check", "icon-sm"), "Body archived")
            : el("span", { class: "chip muted" }, "Header only"),
        verified ? el("span", { class: "chip ok" }, icon("shield-check", "icon-sm"), "Verified")
          : it.verify_status === "changed" ? el("span", { class: "chip warn" }, icon("shield", "icon-sm"), "Changed")
            : it.verify_status === "failed" ? el("span", { class: "chip err" }, icon("shield", "icon-sm"), "Check failed") : null,
        el("span", { class: "chip muted" }, icon("archive", "icon-sm"), "Microsoft 365"))),
    actions));
  // cards laid out in a clean 2-column grid, row-aligned to the top (CSS) — fixed
  // reading order: Contact details | Record completeness / Archive details |
  // Compliance / Notes (full width)
  const body = el("div", { class: "con-detail-body" });
  box.append(body);
  const fieldsBody = el("div", { class: "con-block-body" }, el("div", { class: "spinner" }));
  const fields = el("div", { class: "card con-block" },
    el("div", { class: "con-block-head" }, icon("users", "icon-sm"), el("span", { text: "Contact details" })), fieldsBody);
  const completeSlot = el("div", { class: "card con-block" }, el("div", { class: "con-block-body" }, el("div", { class: "spinner" })));
  body.append(fields, completeSlot, archiveMetaCard(it), complianceCard(it));
  try {
    const c = await api("/api/v1/body?" + qs({ account: App.account, service: "contacts", id: it.remote_id }));
    const kv = el("dl", { class: "kv" });
    const add = (k, v, ic) => { if (!v || (Array.isArray(v) && !v.length)) return; kv.append(el("dt", {}, ic ? icon(ic, "icon-sm") : null, el("span", { text: k })), el("dd", { text: Array.isArray(v) ? v.join(", ") : v })); };
    // Formal name parts (salutation / middle name / generation) the displayName drops.
    const fullName = [c.title, c.givenName, c.middleName, c.surname, c.generation].filter(Boolean).join(" ").trim();
    if (fullName && fullName !== (it.name || "").trim()) add("Full name", fullName, "users");
    add("Email", (c.emailAddresses || []).map(e => e.address).filter(Boolean), "mail");
    add("Mobile", c.mobilePhone, "phone");
    add("Business", c.businessPhones, "phone");
    add("Home", c.homePhones, "phone");
    add("Company", [c.companyName, c.department].filter(Boolean).join(" — "), "building");
    add("Title", c.jobTitle, "users");
    add("Nickname", c.nickName, "users");
    const fmtAddr = (a) => a ? [a.street, a.city, a.state, a.postalCode, a.countryOrRegion].filter(Boolean).join(", ") : "";
    add("Business address", fmtAddr(c.businessAddress), "map-pin");
    add("Home address", fmtAddr(c.homeAddress), "map-pin");
    add("Other address", fmtAddr(c.otherAddress), "map-pin");
    add("Birthday", typeof c.birthday === "string" ? c.birthday.slice(0, 10) : "", "calendar");
    add("IM", c.imAddresses, "share2");
    add("Categories", c.categories, "tag");
    add("Manager", c.manager, "users");
    add("Assistant", c.assistantName, "users");
    add("Spouse / partner", c.spouseName, "users");
    add("Children", c.children, "users");
    add("Profession", c.profession, "building");
    add("Office", c.officeLocation, "map-pin");
    add("Website", c.businessHomePage, "globe");
    clear(fieldsBody);
    if (kv.childElementCount) fieldsBody.append(kv);
    else fieldsBody.append(el("p", { class: "dim", style: "padding:2px", text: "No additional fields archived for this contact." }));
    if (!body.isConnected) return;          // contact switched away while loading
    completeSlot.replaceWith(completenessCard(c));
    if (c.personalNotes) body.append(el("div", { class: "card con-block con-block-full" },
      el("div", { class: "con-block-head" }, icon("file-text", "icon-sm"), el("span", { text: "Notes" })),
      el("p", { class: "con-notes", style: "white-space:pre-wrap", text: c.personalNotes })));
  } catch (e) { clear(fieldsBody).append(el("p", { class: "dim", text: "Could not load contact: " + e.message })); }
}


/* ---------------------------------------------------------------- todo (lists + checklists) */
const Todo = { lists: [], tasks: [], stateFilter: "all" };
const TODO_STATUS = { notStarted: { icon: "circle", cls: "" }, inProgress: { icon: "clock", cls: "prog" }, completed: { icon: "check-square", cls: "done" } };
async function renderTodoView(view) {
  clear(view).append(el("div", { id: "todo-metrics-row", class: "con-metrics-row inset" }));
  const acts = el("div", { class: "view-actions" });
  if (CAP.todowrite) acts.append(
    el("button", { class: "btn sm primary", title: "Create a new task", onclick: () => openComposeTask() }, icon("check-square", "icon-sm"), "New task"),
    el("button", { class: "btn sm", title: "Create a new list", onclick: () => newTodoList() }, icon("notebook", "icon-sm"), "New list"));
  if (CAP.verify) acts.append(verifyButton(() => renderTodoView(view)));
  if (acts.childElementCount) view.append(acts);
  const board = el("div", { id: "todo-board", class: "todo-board" });
  view.append(board);
  board.append(el("div", { class: "card", style: "min-width:280px" }, el("div", { class: "skel", style: "height:200px" })));
  try {
    const [d, act] = await Promise.all([
      api("/api/v1/items?" + qs({ account: App.account, service: "todo", limit: 1000 })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 30 })).catch(() => ({ runs: [] })),
    ]);
    const items = d.items || [];
    Todo.lists = items.filter(it => it.item_type === "list");
    Todo.tasks = items.filter(it => it.item_type === "task");
    App.counts.todo = d.total ?? items.length; updateNavCounts();
    const done = Todo.tasks.filter(t => (t.preview || {}).status === "completed").length;
    fillMetrics($("#todo-metrics-row"), [
      { icon: "check-square", value: Todo.tasks.length, label: "Tasks", sub: `${Todo.lists.length} lists` },
      { icon: "check", value: done, label: "Completed", sub: `${Todo.tasks.length - done} open`, tone: "ok" },
      integrityMetric(Todo.tasks),
      lastActivityMetric(act.runs || []),
    ]);
    todoRender();
  } catch (e) { clear(board).append(el("div", { class: "empty" }, el("h3", { text: "Could not load ToDo" }), el("p", { text: e.message }))); }
}
function todoRender() {
  const board = clear($("#todo-board"));
  // refresh the 4-state filter bar as a sibling just above the board
  const old = $("#todo-statebar"); if (old) old.remove();
  if (Todo.tasks.length) {
    const bar = stateFilterBar(Todo.tasks, Todo.stateFilter, k => { Todo.stateFilter = k; todoRender(); });
    bar.id = "todo-statebar"; board.parentNode.insertBefore(bar, board);
  }
  if (!Todo.lists.length && !Todo.tasks.length) { board.append(el("div", { class: "empty" }, emptyArt("empty-tasks"), el("h3", { text: "No tasks" }), el("p", { text: "Run a backup to populate your task lists." }))); return; }
  // group tasks by their parent list; tasks whose list is unknown go to "Tasks"
  const tasks = Todo.tasks.filter(t => stateMatch(t, Todo.stateFilter));
  const byList = new Map(Todo.lists.map(l => [l.remote_id, []]));
  const orphan = [];
  tasks.forEach(t => (byList.has(t.parent_remote_id) ? byList.get(t.parent_remote_id) : orphan).push(t));
  const order = ["notStarted", "inProgress", "completed"];
  const rank = (t) => order.indexOf((t.preview || {}).status || "notStarted");
  const WELLKNOWN = { flaggedEmails: "Flagged email", defaultList: null };
  const column = (title, tasks, list) => {
    const sorted = tasks.slice().sort((a, b) => rank(a) - rank(b) || (a.name || "").localeCompare(b.name || ""));
    const lp = (list && list.preview) || {};
    const head = el("div", { class: "todo-col-head" }, el("b", { text: title }));
    // system-list label (e.g. the flagged-email list) + a shared badge from the
    // list-level preview fields (wellknownListName / isShared).
    const wk = lp.wellknown_name ? (WELLKNOWN[lp.wellknown_name] !== undefined ? WELLKNOWN[lp.wellknown_name] : lp.wellknown_name) : null;
    if (wk) head.append(el("span", { class: "chip muted", style: "font-size:11px", title: "System list" }, wk));
    if (lp.is_shared) head.append(el("span", { class: "chip muted", style: "font-size:11px", title: "Shared with others" }, icon("share2", "icon-sm"), "Shared"));
    head.append(el("span", { class: "count tnum", text: String(tasks.length) }));
    const col = el("div", { class: "todo-col card" }, head);
    if (!sorted.length) col.append(el("div", { class: "dim", style: "padding:8px", text: "No tasks" }));
    sorted.forEach(t => col.append(taskRow(t)));
    return col;
  };
  Todo.lists.forEach(l => board.append(column(l.name || "List", byList.get(l.remote_id) || [], l)));
  if (orphan.length) board.append(column("Tasks", orphan));
}
function taskRow(t) {
  const p = t.preview || {};
  const st = TODO_STATUS[p.status] || TODO_STATUS.notStarted;
  const hasMeta = p.due || p.importance === "high" || p.steps_total > 0 || p.has_attachments;
  return el("button", { class: "todo-task" + (p.status === "completed" ? " done" : ""), onclick: () => openTaskSheet(t) },
    el("span", { class: "todo-check " + st.cls }, icon(st.icon, "icon-sm")),
    el("div", { class: "grow", style: "min-width:0" },
      el("div", { class: "todo-title truncate", text: t.name || "(untitled)" }),
      hasMeta ? el("div", { class: "todo-meta dim" },
        p.importance === "high" ? el("span", { class: "todo-flag", title: "High importance" }, icon("flag", "icon-sm")) : null,
        p.due ? el("span", { text: "Due " + fmtDate(evDate(p.due, "UTC")) }) : null,
        p.steps_total > 0 ? el("span", { class: "todo-steps", title: "Checklist progress" }, icon("check-square", "icon-sm"), el("span", { text: `${p.steps_done || 0}/${p.steps_total}` })) : null,
        p.has_attachments ? el("span", { class: "todo-att-dot", title: "Has attachments" }, icon("paperclip", "icon-sm")) : null) : null),
    coverageBadge(t));
}
async function openTaskSheet(t) {
  const q = { account: App.account, service: "todo", id: t.remote_id };
  const p = t.preview || {};
  const content = el("div", { class: "body" }, el("div", { class: "spinner" }));
  openSheet(t.name || "Task", content);
  const httpUrl = (u) => (typeof u === "string" && /^https?:\/\//i.test(u)) ? u : null; // block javascript:/data: from cloud data
  const dt = (o) => (o && o.dateTime) ? fmtFullDate(evDate(o.dateTime, o.timeZone)) : "";
  try {
    const full = await api("/api/v1/body?" + qs(q));
    const kv = el("dl", { class: "kv" });
    const add = (k, v, ic) => { if (!v) return; kv.append(el("dt", {}, ic ? icon(ic, "icon-sm") : null, el("span", { text: k })), el("dd", { text: v })); };
    add("Status", (full.status || "").replace(/([A-Z])/g, " $1").replace(/^./, c => c.toUpperCase()), "check-square");
    add("Importance", full.importance, "flag");
    add("Start", dt(full.startDateTime), "clock");
    add("Due", dt(full.dueDateTime), "clock");
    if (full.isReminderOn) add("Reminder", dt(full.reminderDateTime), "clock");
    add("Completed", dt(full.completedDateTime), "check");
    add("Created", full.createdDateTime ? fmtFullDate(full.createdDateTime) : "", "clock");
    add("Categories", (full.categories || []).join(", "), "tag");
    if (full.recurrence && full.recurrence.pattern) add("Repeats", (full.recurrence.pattern.type || "").replace(/^./, c => c.toUpperCase()), "refresh-cw");
    clear(content).append(kv);
    // #567 B6: live write actions (edit / complete / delete), cap-gated
    if (CAP.todowrite && t.parent_remote_id) {
      const acts = el("div", { style: "display:flex;gap:8px;flex-wrap:wrap;margin:4px 0 8px" });
      acts.append(el("button", { class: "btn ghost sm", onclick: () => { closeSheet(); openComposeTask(t); } }, icon("info", "icon-sm"), "Edit"));
      acts.append(full.status === "completed"
        ? el("button", { class: "btn ghost sm", onclick: () => reopenTask(t) }, icon("rotate-ccw", "icon-sm"), "Reopen")
        : el("button", { class: "btn ghost sm", onclick: () => completeTask(t) }, icon("check", "icon-sm"), "Complete"));
      acts.append(el("button", { class: "btn ghost sm", style: "color:var(--danger,#f87171)", onclick: () => deleteTask(t) }, icon("trash-2", "icon-sm"), "Delete"));
      content.append(acts);
    }
    const note = (full.body || {}).content || "";
    if (note.trim()) {
      const txt = (full.body.contentType === "html") ? new DOMParser().parseFromString(note, "text/html").body.textContent : note;
      content.append(el("h3", { class: "sb-section", text: "Notes" }), el("p", { class: "muted", style: "white-space:pre-wrap", text: txt.trim().slice(0, 4000) }));
    }
    // Checklist steps (from the _checklist_<id> sub-resource sidecar, #567 B2);
    // when CAP.todowrite, each step toggles/deletes live + an inline add (#567 B6).
    const cl = await api("/api/v1/body?" + qs({ account: App.account, service: "todo", id: "_checklist_" + t.remote_id })).catch(() => null);
    const steps = (cl && cl.value) || [];
    const canWrite = CAP.todowrite && t.parent_remote_id;
    if (steps.length || canWrite) {
      const head = el("h3", { class: "sb-section" }, el("span", { text: "Checklist" }), el("span", { id: "todo-cl-count", class: "dim", style: "margin-left:6px;font-size:12px" }));
      const box = el("div", { class: "todo-checklist" });
      const updateCount = () => { const c = $("#todo-cl-count"); if (c) c.textContent = steps.length ? `${steps.filter(s => s.isChecked).length}/${steps.length}` : ""; };
      const renderSteps = () => {
        clear(box);
        steps.forEach(s => {
          const row = el("div", { class: "todo-step" + (s.isChecked ? " done" : "") });
          if (canWrite && s.id) row.append(el("button", { class: "todo-step-btn", title: s.isChecked ? "Mark not done" : "Mark done", onclick: () => toggleStep(t, s, renderSteps, updateCount) }, icon(s.isChecked ? "check-square" : "circle", "icon-sm")));
          else row.append(icon(s.isChecked ? "check-square" : "circle", "icon-sm"));
          row.append(el("span", { class: "grow truncate", text: s.displayName || "(step)" }));
          if (canWrite && s.id) row.append(el("button", { class: "todo-step-btn del", title: "Delete step", onclick: () => deleteStep(t, s, steps, renderSteps, updateCount) }, icon("x", "icon-sm")));
          box.append(row);
        });
        updateCount();
      };
      renderSteps();
      content.append(head, box);
      if (canWrite) {
        const inp = el("input", { class: "input", placeholder: "Add a step…" });
        const submit = () => addStep(t, inp, steps, renderSteps, updateCount);
        inp.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); submit(); } });
        content.append(el("div", { class: "todo-cl-add" }, inp, el("button", { class: "btn sm", title: "Add step", onclick: submit }, icon("plus", "icon-sm"))));
      }
    }
    // Linked resources (from the _linked_<id> sidecar)
    const lr = await api("/api/v1/body?" + qs({ account: App.account, service: "todo", id: "_linked_" + t.remote_id })).catch(() => null);
    const links = (lr && lr.value) || [];
    if (links.length) {
      const box = el("div", { class: "todo-links" });
      links.forEach(r => {
        const url = httpUrl(r.webUrl);
        const label = r.displayName || r.applicationName || "Linked resource";
        box.append(url
          ? el("a", { class: "todo-link", href: url, target: "_blank", rel: "noopener noreferrer" }, icon("external-link", "icon-sm"), el("span", { class: "truncate", text: label }))
          : el("div", { class: "todo-link" }, icon("external-link", "icon-sm"), el("span", { class: "truncate", text: label })));
      });
      content.append(el("h3", { class: "sb-section", text: "Linked resources" }), box);
    }
    // Attachments — gated on the preview's has_attachments; download via the route
    if (p.has_attachments) {
      const att = await api("/api/v1/attachment?" + qs(q)).catch(() => null);
      const list = (att && att.attachments) || [];
      if (list.length) {
        const box = el("div", { class: "todo-atts" });
        list.forEach(a => box.append(el("a", { class: "todo-att",
          href: "/api/v1/attachment?" + qs({ account: App.account, service: "todo", id: t.remote_id, index: a.index }),
          target: "_blank", rel: "noopener", download: a.filename || "attachment" },
          icon("paperclip", "icon-sm"), el("span", { class: "grow truncate", text: a.filename || "attachment" }),
          a.size ? el("span", { class: "dim", text: fmtSize(a.size) }) : null)));
        content.append(el("h3", { class: "sb-section", text: "Attachments" }), box);
      }
    }
  } catch (e) { clear(content).append(el("p", { class: "dim", text: "Could not load task: " + e.message })); }
}
// #567 B6: live ToDo write — task create/edit, list create, complete/delete, checklist ops (cap-gated)
async function openComposeTask(t) {
  if (!CAP.todowrite) return;
  const editing = !!t;
  const field = (label, input) => el("label", { class: "cmp-field" }, el("span", { class: "cmp-label", text: label }), input);
  const title = el("input", { class: "input", id: "ctask-title", placeholder: "Title", value: editing ? (t.name || "") : "" });
  let listSel = null;
  if (!editing) {
    listSel = el("select", { class: "input", id: "ctask-list" });
    Todo.lists.forEach(l => listSel.append(el("option", { value: l.remote_id, text: l.name || "List" })));
  }
  const imp = el("select", { class: "input", id: "ctask-imp" }, el("option", { value: "normal", text: "Normal" }), el("option", { value: "high", text: "High" }), el("option", { value: "low", text: "Low" }));
  const start = el("input", { class: "input", type: "date", id: "ctask-start" });
  const due = el("input", { class: "input", type: "date", id: "ctask-due" });
  const reminder = el("input", { class: "input", type: "datetime-local", id: "ctask-rem" });
  const cats = el("input", { class: "input", id: "ctask-cats", placeholder: "Comma-separated" });
  const note = el("textarea", { class: "input cmp-textarea", id: "ctask-note", placeholder: "Notes", rows: "4" });
  if (editing) {
    try {
      const full = await api("/api/v1/body?" + qs({ account: App.account, service: "todo", id: t.remote_id }));
      imp.value = full.importance || "normal";
      if (full.startDateTime) start.value = (full.startDateTime.dateTime || "").slice(0, 10);
      if (full.dueDateTime) due.value = (full.dueDateTime.dateTime || "").slice(0, 10);
      if (full.isReminderOn && full.reminderDateTime) reminder.value = (full.reminderDateTime.dateTime || "").slice(0, 16);
      cats.value = (full.categories || []).join(", ");
      note.value = (full.body || {}).content || "";
    } catch { toast("Could not load task for editing", "err"); }
  }
  const content = el("div", { class: "compose" },
    field("Title", title),
    !editing ? field("List", listSel) : null,
    field("Importance", imp), field("Start", start), field("Due", due), field("Reminder", reminder),
    field("Categories", cats), field("Notes", note));
  // Inline in the todo board (live.com-style), not an overlay sheet. Discard
  // re-renders the board.
  const box = $("#todo-board");
  const head = el("div", { class: "cmp-inline-head" },
    el("span", { class: "cmp-inline-title truncate" }, editing ? "Edit task" : "New task"),
    el("div", { style: "flex:1" }),
    el("button", { class: "btn ghost sm", type: "button", onclick: () => todoRender() }, "Discard"),
    el("button", { class: "btn primary sm", type: "button", onclick: (e) => composeTaskSubmit(e.currentTarget, t) }, icon("check-square", "icon-sm"), editing ? "Save" : "Create"));
  if (box) {
    clear(box).append(head, content);
    setTimeout(() => title.focus(), 60);
  } else {
    openSheet(editing ? "Edit task" : "New task", content);
  }
}
async function composeTaskSubmit(btn, t) {
  const v = (s) => ($("#" + s).value || "").trim();
  const title = v("ctask-title");
  if (!title) { toast("Add a title", "err"); return; }
  const list = t ? t.parent_remote_id : ($("#ctask-list") && $("#ctask-list").value);
  if (!list) { toast("No list available — create a list first", "err"); return; }
  const params = { account: App.account, list, title, importance: v("ctask-imp"), categories: v("ctask-cats"), body: v("ctask-note") };
  const start = v("ctask-start"); if (start) params.start = start + "T00:00:00";
  const due = v("ctask-due"); if (due) params.due = due + "T00:00:00";
  const rem = v("ctask-rem"); if (rem) params.reminder = new Date(rem).toISOString();
  if (t) params.id = t.remote_id;
  btn.disabled = true;
  try {
    await post((t ? "/api/v1/todo/update?" : "/api/v1/todo/create?") + qs(params), CAP.todowrite);
    toast(t ? "Task updated" : "Task created"); closeSheet(); todoReload();
  } catch (e) { toast("Failed: " + e.message, "err"); btn.disabled = false; }
}
async function newTodoList() {
  const name = prompt("New list name:");
  if (!name || !name.trim()) return;
  try { await post("/api/v1/todo/list-create?" + qs({ account: App.account, name: name.trim() }), CAP.todowrite); toast("List created"); todoReload(); }
  catch (e) { toast("Failed: " + e.message, "err"); }
}
async function completeTask(t) {
  try { await post("/api/v1/todo/complete?" + qs({ account: App.account, list: t.parent_remote_id, id: t.remote_id }), CAP.todowrite); toast("Task completed"); closeSheet(); todoReload(); }
  catch (e) { toast("Failed: " + e.message, "err"); }
}
async function reopenTask(t) {
  try { await post("/api/v1/todo/update?" + qs({ account: App.account, list: t.parent_remote_id, id: t.remote_id, status: "notStarted" }), CAP.todowrite); toast("Task reopened"); closeSheet(); todoReload(); }
  catch (e) { toast("Failed: " + e.message, "err"); }
}
async function deleteTask(t) {
  if (!confirmDestructive("Delete this task from your Microsoft 365 account?")) return;
  try { await post("/api/v1/todo/delete?" + qs({ account: App.account, list: t.parent_remote_id, id: t.remote_id }), CAP.todowrite); toast("Task deleted"); closeSheet(); todoReload(); }
  catch (e) { toast("Failed: " + e.message, "err"); }
}
// checklist ops use optimistic UI (the daemon doesn't re-sync on a self-write)
async function toggleStep(t, s, renderSteps, updateCount) {
  try {
    await post("/api/v1/todo/checklist-toggle?" + qs({ account: App.account, list: t.parent_remote_id, task: t.remote_id, item: s.id, checked: s.isChecked ? "0" : "1" }), CAP.todowrite);
    s.isChecked = !s.isChecked; renderSteps(); updateCount();
  } catch (e) { toast("Failed: " + e.message, "err"); }
}
async function deleteStep(t, s, steps, renderSteps, updateCount) {
  try {
    await post("/api/v1/todo/checklist-delete?" + qs({ account: App.account, list: t.parent_remote_id, task: t.remote_id, item: s.id }), CAP.todowrite);
    const i = steps.indexOf(s); if (i >= 0) steps.splice(i, 1); renderSteps(); updateCount();
  } catch (e) { toast("Failed: " + e.message, "err"); }
}
async function addStep(t, inp, steps, renderSteps, updateCount) {
  const title = (inp.value || "").trim();
  if (!title) return;
  try {
    const r = await post("/api/v1/todo/checklist-add?" + qs({ account: App.account, list: t.parent_remote_id, task: t.remote_id, title }), CAP.todowrite);
    steps.push({ id: r.id, displayName: title, isChecked: false }); inp.value = ""; renderSteps(); updateCount(); inp.focus();
  } catch (e) { toast("Failed: " + e.message, "err"); }
}
async function todoReload() {
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "todo", limit: 1000 }));
    const items = d.items || [];
    Todo.lists = items.filter(it => it.item_type === "list");
    Todo.tasks = items.filter(it => it.item_type === "task");
    App.counts.todo = d.total ?? items.length; updateNavCounts();
    todoRender();
  } catch (e) { toast("Reload failed: " + e.message, "err"); }
}

/* ---------------------------------------------------------------- onenote (notebook→section→page tree + reader) */
const Note = { items: [], stateFilter: "all", expanded: new Set(), selected: null };
const NOTE_ORDER = { notebook: 0, "section-group": 1, section: 2, page: 3 };
const NOTE_ICON = { notebook: "notebook", "section-group": "folder", section: "folder", page: "file-text" };
async function renderOnenoteView(view) {
  clear(view);
  const tree = el("div", { id: "note-tree", class: "note-tree" });
  const reader = el("div", { id: "note-reader", class: "note-reader" });
  const acts = el("div", { class: "view-actions" });
  if (CAP.onenotewrite) acts.append(el("button", { class: "btn sm primary", title: "Create a new page", onclick: () => openComposePage() }, icon("notebook", "icon-sm"), "New page"));
  if (CAP.verify) acts.append(verifyButton(() => renderOnenoteView(view)));
  view.append(el("div", { class: "note-page" },
    el("div", { id: "note-metrics-row", class: "con-metrics-row top" }),
    acts.childElementCount ? acts : null,
    el("div", { class: "note-layout" }, tree, reader)));
  renderNoteReader(null);
  for (let i = 0; i < 5; i++) tree.append(el("div", { class: "note-item" }, el("div", { class: "skel grow", style: "height:30px" })));
  try {
    const [d, act] = await Promise.all([
      api("/api/v1/items?" + qs({ account: App.account, service: "onenote", limit: 1000 })),
      api("/api/v1/activity?" + qs({ account: App.account, limit: 30 })).catch(() => ({ runs: [] })),
    ]);
    Note.items = d.items || [];
    const pages = Note.items.filter(it => it.item_type === "page");
    const notebooks = Note.items.filter(it => it.item_type === "notebook");
    App.counts.onenote = d.total ?? pages.length; updateNavCounts();
    fillMetrics($("#note-metrics-row"), [
      { icon: "notebook", value: pages.length, label: "Pages", sub: `${notebooks.length} notebooks` },
      integrityMetric(pages),
      lastActivityMetric(act.runs || []),
    ]);
    // expand notebooks + section groups by default so the structure is visible
    Note.expanded = new Set(Note.items.filter(it => it.item_type === "notebook" || it.item_type === "section-group" || it.item_type === "section").map(it => it.remote_id));
    Note.stateFilter = "all";
    noteRenderTree();
  } catch (e) { clear(tree).append(el("div", { class: "empty" }, el("h3", { text: "Could not load OneNote" }), el("p", { text: e.message }))); }
}
// recursively count pages under a node that match the active state filter
function noteVisiblePages(it, byParent) {
  if (it.item_type === "page") return stateMatch(it, Note.stateFilter) ? 1 : 0;
  return (byParent.get(it.remote_id) || []).reduce((n, c) => n + noteVisiblePages(c, byParent), 0);
}
function noteSortKids(kids) {
  return kids.slice().sort((a, b) => (NOTE_ORDER[a.item_type] ?? 9) - (NOTE_ORDER[b.item_type] ?? 9) || (a.name || "").localeCompare(b.name || ""));
}
function noteRenderTree() {
  const host = $("#note-tree"); if (!host) return; clear(host);
  if (!Note.items.length) { host.append(el("div", { class: "empty" }, emptyArt("empty-notes"), el("h3", { text: "No notes" }), el("p", { text: "Run a backup to populate OneNote." }))); return; }
  const pages = Note.items.filter(it => it.item_type === "page");
  host.append(stateFilterBar(pages, Note.stateFilter, k => { Note.stateFilter = k; noteRenderTree(); }));
  const byParent = new Map();
  Note.items.forEach(it => { const k = it.parent_remote_id || "__root__"; (byParent.get(k) || byParent.set(k, []).get(k)).push(it); });
  const ids = new Set(Note.items.map(it => it.remote_id));
  // roots: notebooks (parent null) + orphans (parent not a tracked item)
  const roots = noteSortKids(Note.items.filter(it => !it.parent_remote_id || !ids.has(it.parent_remote_id)));
  const treeBox = el("div", { class: "note-tree-body" });
  roots.forEach(it => { const node = noteRenderNode(it, byParent, 0); if (node) treeBox.append(node); });
  if (!treeBox.childElementCount) { treeBox.append(el("div", { class: "empty" }, icon("search", "icon-lg"), el("h3", { text: "No matches" }), el("p", { text: "No pages have this backup status." }))); }
  host.append(treeBox);
  if (!Note.selected) { const first = pages.find(p => stateMatch(p, Note.stateFilter)); if (first) setTimeout(() => noteSelect(first), 0); }
}
function noteRenderNode(it, byParent, depth) {
  const pad = `padding-left:${8 + depth * 14}px`;
  if (it.item_type === "page") {
    if (!stateMatch(it, Note.stateFilter)) return null;
    const sel = Note.selected && Note.selected.remote_id === it.remote_id;
    return el("button", { class: "note-leaf" + (sel ? " active" : ""), style: pad, dataset: { id: it.remote_id }, onclick: () => noteSelect(it) },
      icon("file-text", "icon-sm"),
      el("span", { class: "grow truncate", text: it.name || "(untitled)" }),
      coverageBadge(it));
  }
  // container (notebook / section-group / section)
  if (Note.stateFilter !== "all" && noteVisiblePages(it, byParent) === 0) return null;
  const open = Note.expanded.has(it.remote_id);
  const kids = noteSortKids(byParent.get(it.remote_id) || []);
  const head = el("button", { class: "note-node-head" + (open ? " open" : ""), style: pad, onclick: () => { if (open) Note.expanded.delete(it.remote_id); else Note.expanded.add(it.remote_id); noteRenderTree(); } },
    el("span", { class: "note-chev" }, icon("chevron-right", "icon-sm")),
    icon(NOTE_ICON[it.item_type] || "folder", "icon-sm"),
    el("span", { class: "grow truncate", text: it.name || "(untitled)" }),
    // mark the default notebook/section (isDefault from the flank sidecar) — was
    // captured in the preview but never shown.
    (it.preview && it.preview.is_default) ? el("span", { class: "note-node-badge dim", title: "Default " + it.item_type, style: "font-size:10px;opacity:.6;text-transform:uppercase;letter-spacing:.04em;margin-right:4px" }, "Default") : null,
    el("span", { class: "note-node-count tnum dim", text: String(noteVisiblePages(it, byParent)) }));
  const node = el("div", { class: "note-node" }, head);
  if (open) {
    const body = el("div", { class: "note-node-kids" });
    kids.forEach(c => { const cn = noteRenderNode(c, byParent, depth + 1); if (cn) body.append(cn); });
    if (!body.childElementCount) body.append(el("div", { class: "dim", style: `${pad};padding-top:4px;font-size:12px`, text: "(empty)" }));
    node.append(body);
  }
  return node;
}
function noteSelect(it) {
  Note.selected = it;
  document.querySelectorAll(".note-leaf").forEach(r => r.classList.toggle("active", r.dataset.id === it.remote_id));
  renderNoteReader(it);
}
function renderNoteReader(it) {
  const box = $("#note-reader"); if (!box) return; clear(box);
  if (!it) { box.append(el("div", { class: "empty", style: "margin:auto" }, logoGlyph(64), el("h3", { text: "Select a page" }))); return; }
  const q = { account: App.account, service: "onenote", id: it.remote_id };
  const p = it.preview || {};
  const actions = el("div", { class: "note-reader-actions" },
    el("a", { class: "btn ghost sm", href: `/api/v1/view?${qs(q)}`, target: "_blank", rel: "noopener", title: "Open in new tab" }, icon("external-link", "icon-sm")));
  if (CAP.onenotewrite) {
    actions.append(el("button", { class: "btn ghost sm", title: "Append a paragraph (best-effort)", onclick: () => appendPage(it) }, icon("plus", "icon-sm"), "Append"));
    actions.append(el("button", { class: "btn ghost sm", style: "color:var(--danger,#f87171)", title: "Delete this page", onclick: () => deletePage(it) }, icon("trash-2", "icon-sm"), "Delete"));
  }
  // metadata strip from the page preview (created / section / notebook / tags / link)
  const meta = el("div", { class: "note-meta dim" });
  const chip = (ic, txt) => txt ? meta.append(el("span", { class: "note-meta-chip" }, icon(ic, "icon-sm"), el("span", { text: txt }))) : null;
  chip("archive", p.notebook_name ? `${p.notebook_name}${p.section_name ? " / " + p.section_name : ""}` : (p.section_name || ""));
  chip("clock", p.created ? "Created " + fmtDate(p.created) : "");
  if (Array.isArray(p.user_tags) && p.user_tags.length) chip("tag", p.user_tags.join(", "));
  if (p.has_resources) chip("paperclip", "Has embedded resources");
  meta.append(coverageBadge(it));
  if (p.web_url && /^https?:\/\//i.test(p.web_url)) meta.append(el("a", { class: "note-meta-chip", href: p.web_url, target: "_blank", rel: "noopener noreferrer" }, icon("external-link", "icon-sm"), el("span", { text: "Open in OneNote" })));
  // has_body=false (live_only) pages have no archived content — show a native dark
  // card instead of pointing the iframe at /view (which would 404 with raw JSON).
  let bodyEl;
  if (it.has_body) {
    // Same pattern as the mail reader: size the iframe to its OWN content and let
    // the OUTER pane scroll, so the whole page scrolls naturally (an internally-
    // scrolling iframe in a flex column "can't scroll to the end", esp. on touch).
    const scroll = el("div", { class: "note-frame-scroll" });
    const frame = el("iframe", { class: "note-frame", src: `/api/v1/view?${qs(q)}`, title: "Note", loading: "lazy", sandbox: "allow-same-origin" });
    const fit = () => {
      try {
        const d = frame.contentDocument; if (!d || !d.body) return;
        const h = Math.max(d.documentElement.scrollHeight, d.body.scrollHeight) + 4;
        if (Math.abs((parseInt(frame.style.height, 10) || 0) - h) > 2) frame.style.height = h + "px";
      } catch { /* cross-origin */ }
    };
    frame.addEventListener("load", () => {
      fit();
      try {
        const d = frame.contentDocument;
        if (d && window.ResizeObserver) { const ro = new ResizeObserver(fit); ro.observe(d.documentElement); if (d.body) ro.observe(d.body); }
        if (d) d.querySelectorAll("img").forEach(img => { if (!img.complete) { img.addEventListener("load", fit, { once: true }); img.addEventListener("error", fit, { once: true }); } });
      } catch { /* cross-origin */ }
      [120, 400, 1000, 2500].forEach(t => setTimeout(fit, t));
    });
    scroll.append(frame);
    bodyEl = scroll;
  } else {
    bodyEl = el("div", { class: "empty note-empty", style: "margin:auto;text-align:center;padding:48px" },
      icon("cloud", "icon-lg"),
      el("h3", { text: "Not backed up yet" }),
      el("p", { class: "dim", text: "This page is in the cloud but its content isn't archived yet. Run a backup to read it here." }));
  }
  box.append(
    el("header", { class: "note-reader-head" }, el("h2", { class: "grow truncate", text: it.name || "(untitled)" }), actions),
    meta,
    bodyEl);
}
// #568: live OneNote write — create page (section picker) / delete / best-effort append, cap-gated
async function openComposePage(presetSection) {
  if (!CAP.onenotewrite) return;
  const sections = Note.items.filter(it => it.item_type === "section");
  if (!sections.length) { toast("No section available — back up a notebook first", "err"); return; }
  const field = (label, input) => el("label", { class: "cmp-field" }, el("span", { class: "cmp-label", text: label }), input);
  const secSel = el("select", { class: "input", id: "cpage-section" });
  sections.forEach(s => secSel.append(el("option", { value: s.remote_id, text: s.name || "Section", selected: presetSection === s.remote_id })));
  const title = el("input", { class: "input", id: "cpage-title", placeholder: "Page title" });
  const body = el("textarea", { class: "input cmp-textarea", id: "cpage-body", placeholder: "Page text", rows: "8" });
  const content = el("div", { class: "compose" },
    field("Section", secSel), field("Title", title), field("Body", body),
    el("div", { class: "cmp-footer" }, el("div", { class: "spacer", style: "flex:1" }),
      el("button", { class: "btn primary", type: "button", onclick: (e) => composePageSubmit(e.currentTarget) }, icon("notebook", "icon-sm"), "Create")));
  openSheet("New page", content);
  setTimeout(() => title.focus(), 60);
}
async function composePageSubmit(btn) {
  const v = (s) => ($("#" + s).value || "").trim();
  const section = $("#cpage-section") && $("#cpage-section").value;
  const title = v("cpage-title");
  if (!section) { toast("Pick a section", "err"); return; }
  if (!title) { toast("Add a title", "err"); return; }
  btn.disabled = true;
  try {
    await post("/api/v1/onenote/create?" + qs({ account: App.account, section, title, body: v("cpage-body") }), CAP.onenotewrite);
    toast("Page created"); closeSheet(); noteReload();
  } catch (e) { toast("Failed: " + e.message, "err"); btn.disabled = false; }
}
async function deletePage(it) {
  if (!confirmDestructive("Delete this page from your Microsoft 365 account?")) return;
  try {
    await post("/api/v1/onenote/delete?" + qs({ account: App.account, id: it.remote_id }), CAP.onenotewrite);
    toast("Page deleted"); Note.selected = null; renderNoteReader(null); noteReload();
  } catch (e) { toast("Failed: " + e.message, "err"); }
}
// #65: inline in-reader append composer. Graph's page-content PATCH reliably
// supports appending a block (`target:body, action:append`); arbitrary in-place
// replace of existing elements is fragile on a personal MSA, so we scope the UX to
// append-only and say so. Optimistic toast; cap-gated.
function appendPage(it) {
  const box = $("#note-reader"); if (!box) return;
  const existing = box.querySelector(".note-append");
  if (existing) { existing.querySelector("textarea")?.focus(); return; }
  const ta = el("textarea", { class: "input note-append-ta", rows: "3", placeholder: "Type a paragraph to append to this page…" });
  const status = el("span", { class: "dim", style: "font-size:12px" });
  const submit = async (btn) => {
    const text = (ta.value || "").trim();
    if (!text) { ta.focus(); return; }
    btn.disabled = true; status.textContent = "Appending…";
    try {
      await post("/api/v1/onenote/append?" + qs({ account: App.account, id: it.remote_id, text }), CAP.onenotewrite);
      toast("Appended — OneNote may take a moment to reflect it");
      panel.remove();
    } catch (e) { status.textContent = ""; btn.disabled = false; toast("Append failed: " + e.message, "err"); }
  };
  const panel = el("div", { class: "note-append" },
    el("div", { class: "note-append-head dim" }, icon("plus", "icon-sm"), el("span", { text: "Append a paragraph" })),
    ta,
    el("div", { class: "note-append-foot" },
      el("button", { class: "btn sm primary", onclick: (e) => submit(e.currentTarget) }, icon("plus", "icon-sm"), "Append"),
      el("button", { class: "btn ghost sm", onclick: () => panel.remove() }, "Cancel"),
      status,
      el("span", { class: "dim", style: "font-size:11px;margin-left:auto;text-align:right", text: "Append-only — arbitrary in-place edits aren't reliable on personal OneNote" })));
  ta.addEventListener("keydown", (e) => { if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); submit(panel.querySelector(".btn.primary")); } });
  box.append(panel);
  ta.focus();
}
async function noteReload() {
  try {
    const d = await api("/api/v1/items?" + qs({ account: App.account, service: "onenote", limit: 1000 }));
    Note.items = d.items || [];
    App.counts.onenote = d.total ?? Note.items.filter(it => it.item_type === "page").length; updateNavCounts();
    if (Note.selected) { const s = Note.items.find(x => x.remote_id === Note.selected.remote_id); Note.selected = s || null; renderNoteReader(s || null); }
    noteRenderTree();
  } catch (e) { toast("Reload failed: " + e.message, "err"); }
}

/* shared detail sheet (used by calendar/contacts/todo) */
function openSheet(title, contentEl, leading) {
  closeSheet();
  const scrim = el("div", { class: "scrim", onclick: closeSheet });
  const sheet = el("aside", { class: "sheet" },
    el("header", {}, leading || null, el("h2", { class: "grow truncate", text: title }),
      el("button", { class: "btn ghost sm icon-only", onclick: closeSheet }, icon("x", "icon-sm"))),
    contentEl);
  sheet.prepend(sheetNet());
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
    archiveServices().forEach(s => {
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
const POLL_STEPS = [1, 5, 10, 30, 60, 300, 900, 1800, 3600];
const pollLabel = (s) => s < 60 ? s + "s" : (s / 60) + "min";
function nearestPollStep(secs) {
  let best = 0;
  for (let i = 0; i < POLL_STEPS.length; i++)
    if (Math.abs(POLL_STEPS[i] - secs) < Math.abs(POLL_STEPS[best] - secs)) best = i;
  return best;
}
async function renderSettingsView(view) {
  clear(view).append(el("h1", { class: "view-title", text: "Settings" }), el("p", { class: "view-sub", text: "Configuration, sync controls, and the live-update interval." }));
  const body = el("div", { class: "grid", style: "max-width:720px" }); view.append(body);
  body.append(el("div", { class: "card" }, el("div", { class: "spinner" })));
  try {
    const [cfg, st] = await Promise.all([api("/api/v1/settings").catch(() => ({})), api("/api/v1/sync/state").catch(() => ({}))]);
    const sy = cfg.sync || {}, acc = (cfg.accounts || []).find(a => a.id === App.account) || {};
    clear(body);
    const acctCard = el("div", { class: "card" }, el("h3", { class: "sb-section", text: "Account" }),
      kvList([["User", acc.username || App.account], ["Sync root", acc.sync_root], ["Archive root", acc.archive_root], ["Mount point", acc.mount_point || "—"]]));
    // The sidebar account chip (which opens sign-in / reconnect) is hidden in the phone
    // bottom-nav layout, so surface the same device-code account menu here too —
    // Settings is reachable on mobile. Without this a standalone phone (#89) would have
    // no way to sign in. Shown whenever account-auth is wired (mobile live + daemon).
    if (CAP.account) acctCard.append(el("button", { class: "btn", style: "margin-top:12px", onclick: openAccountSwitcher },
      icon("rotate-ccw", "icon-sm"), "Sign in / reconnect account"));
    body.append(acctCard);
    // Diagnostics: a live perf overlay flag (CPU/RAM/disk-IO of the whole app process).
    const perfLbl = el("span", { text: localStorage.getItem("isy_perf") === "1" ? "Hide performance overlay" : "Show performance overlay" });
    body.append(el("div", { class: "card" }, el("h3", { class: "sb-section", text: "Diagnostics" }),
      el("p", { class: "dim", style: "font-size:13px;margin:.2rem 0 .7rem", text: "Live overlay of the app's whole-process load — CPU, RAM and disk IO — for performance testing." }),
      el("button", { class: "btn", onclick: () => { const on = togglePerf(); perfLbl.textContent = on ? "Hide performance overlay" : "Show performance overlay"; } },
        icon("clock", "icon-sm"), perfLbl)));
    const syncCard = el("div", { class: "card" }, el("h3", { class: "sb-section", text: "Sync" }),
      kvList([["Scheduled", st.enabled ? (st.paused ? "paused" : "running") : "off"], ["Trash retention", (sy.trash_retention_days ?? "—") + " days"], ["Body index (FTS)", sy.body_index ? "on" : "off"], ["Change source", sy.change_source || "—"]]));
    if (st.enabled && CAP.sync) syncCard.append(el("div", { style: "display:flex;gap:8px;margin-top:12px" },
      el("button", { class: "btn", onclick: () => syncCmd("now") }, icon("refresh-cw", "icon-sm"), "Sync now"),
      st.paused ? el("button", { class: "btn", onclick: () => syncCmd("resume") }, icon("play", "icon-sm"), "Resume") : el("button", { class: "btn", onclick: () => syncCmd("pause") }, icon("pause", "icon-sm"), "Pause")));
    // live-update interval slider (log scale) — writable when the daemon enables it
    if (CAP.settings) {
      let idx = nearestPollStep(sy.poll_interval_secs || 5);
      const valLabel = el("span", { class: "tnum", style: "font-weight:700", text: pollLabel(POLL_STEPS[idx]) });
      const warn = el("div", { class: "dim", style: "font-size:12px;min-height:16px;color:var(--warn)" });
      const setWarn = (s) => { warn.textContent = s < 5 ? "very frequent — Microsoft may throttle; backoff still applies" : ""; };
      setWarn(POLL_STEPS[idx]);
      const slider = el("input", {
        type: "range", class: "poll-slider", min: "0", max: String(POLL_STEPS.length - 1), step: "1", value: String(idx),
        oninput: (e) => { const s = POLL_STEPS[+e.target.value]; valLabel.textContent = pollLabel(s); setWarn(s); },
        onchange: async (e) => {
          const s = POLL_STEPS[+e.target.value];
          try { await post("/api/v1/settings?" + qs({ poll_interval_secs: s }), CAP.settings); toast("Live-update interval: " + pollLabel(s)); }
          catch (err) { toast("Could not save interval: " + err.message, "err"); }
        },
      });
      syncCard.append(el("div", { style: "margin-top:16px" },
        el("div", { style: "display:flex;justify-content:space-between;align-items:center;margin-bottom:6px" },
          el("span", { class: "dim", style: "font-size:12px", text: "Live-update interval (how often the cloud is polled)" }), valLabel),
        slider,
        el("div", { style: "display:flex;justify-content:space-between;font-size:11px", class: "dim" },
          el("span", { text: "1s" }), el("span", { text: "60min" })),
        warn));
    }
    body.append(syncCard);
    body.append(el("div", { class: "card", style: "display:flex;align-items:center;gap:16px" }, logoGlyph(48),
      el("div", { onclick: eggTap }, el("div", { style: "font-size:16px;font-weight:700", html: "iSync<span style='background:var(--grad-accent);-webkit-background-clip:text;background-clip:text;color:transparent'>You</span>" }),
        el("div", { class: "dim", text: "Microsoft 365 personal backup & archive" }))));
  } catch (e) { clear(body).append(el("div", { class: "empty" }, el("h3", { text: "Could not load settings" }), el("p", { text: e.message }))); }
}

/* ---------------------------------------------------------------- actions */
async function doRestore(it, btn) {
  if (!confirm(`Restore this ${it.service} item to the cloud as a new copy?`)) return;
  btn.disabled = true;
  try {
    const d = await post("/api/v1/restore?" + qs({ account: App.account, service: it.service, id: it.remote_id }), CAP.restore);
    if (d.queued) {
      toast(`Restore queued (${String(d.job_id || "job").slice(0, 12)}…)`);
    } else {
      toast(`Restored (new id ${String(d.new_id || "").slice(0, 8)}…)`);
    }
  }
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
// Account menu (#68): switch between configured accounts, sign out (clear the
// cached token), and sign in / reconnect via the device-code flow (cap-gated).
let accountMenu = null, accountMenuPoll = null;
function closeAccountMenu() {
  if (accountMenuPoll) { clearInterval(accountMenuPoll); accountMenuPoll = null; }
  if (accountMenu) { accountMenu.remove(); accountMenu = null; }
}
function openAccountSwitcher() {
  if (accountMenu) { closeAccountMenu(); return; }
  const scrim = el("div", { class: "scrim", style: "background:transparent", onclick: closeAccountMenu });
  const body = el("div", { class: "acct-menu-body" });
  const panel = el("aside", { class: "acct-menu" }, el("div", { class: "acct-menu-head" }, icon("users", "icon-sm"), el("span", { text: "Accounts" })), body);
  accountMenu = el("div", { class: "acct-menu-wrap" }, scrim, panel);
  document.body.append(accountMenu);
  renderAccountMenu(body);
}
/* ---------------------------------------------------------------- assistant (S-AG.12: connect) */
// Begin OAuth/device-code login by asking the engine for a provider URL and handing it
// to the system browser. Mobile uses typed native bridge ops; desktop keeps browser
// navigation. The engine completes the callback/token exchange and the UI polls status.
function openDesktopExternal(url, newTab) {
  if (newTab && typeof window !== "undefined" && window.open) {
    const w = window.open(url, "_blank", "noopener");
    if (w) return;
  }
  location.href = url;
}
async function openExternalAuth(url, kind, opts) {
  if (!url) throw new Error("Missing auth URL");
  if (BRIDGE) {
    await nativeCall("openExternal", { url, kind }, NATIVE_TIMEOUT_MS);
    return;
  }
  openDesktopExternal(url, !!(opts && opts.newTab));
}
async function beginNetworkGuard() {
  if (!BRIDGE) return null;
  const d = await nativeCall("beginNetworkGuard", {}, NATIVE_TIMEOUT_MS);
  return d && d.guard_id ? d.guard_id : null;
}
async function endNetworkGuard(guardId) {
  if (!BRIDGE || !guardId) return;
  try { await nativeCall("endNetworkGuard", { guard_id: guardId }, NATIVE_TIMEOUT_MS); } catch (_) {}
}

let AGENT_GUARD_ID = null;
let CODEX_GUARD_ID = null;
async function finishAgentGuard() {
  const id = AGENT_GUARD_ID;
  AGENT_GUARD_ID = null;
  await endNetworkGuard(id);
}
async function finishCodexGuard() {
  const id = CODEX_GUARD_ID;
  CODEX_GUARD_ID = null;
  await endNetworkGuard(id);
}
function localCallbackRedirect(host) {
  return location.port ? `http://${host}:${location.port}/callback` : "";
}

async function startAiLogin(provider) {
  const consentProvider = agentProviderConsentId(provider);
  if (!agentPrivacyConsentAccepted(consentProvider)) {
    toast("Review privacy consent for " + agentProviderLabel(consentProvider), "err");
    renderAssistantView($("#view"));
    return;
  }
  let guardId = null;
  try {
    const params = { provider };
    const redirect = localCallbackRedirect("localhost");
    if (redirect) params.redirect = redirect;
    const manualCodeFlow = provider === "claude" && !redirect;
    const d = await post("/api/v1/agent/oauth/start?" + qs(params), CAP.agent);
    if (!d || !d.authorize_url) { toast("Could not start sign-in"); return; }
    if (manualCodeFlow) showCodeStep();
    else showWaitingStep();          // waiting UI + poll; completes when /callback fires
    toast("Opening sign-in in your browser…");
    guardId = await beginNetworkGuard();
    AGENT_GUARD_ID = guardId;
    await openExternalAuth(d.authorize_url, "agent_authorize");
  } catch (e) {
    if (guardId) await endNetworkGuard(guardId);
    if (AGENT_GUARD_ID === guardId) AGENT_GUARD_ID = null;
    toast("Sign-in unavailable: " + (e.message || e));
  }
}

// After the browser login the engine's /callback stores the token; poll status until
// connected, then switch to the chat.
let AGENT_POLL_ON = false;
function showWaitingStep() {
  const card = document.getElementById("asst-connect-card");
  if (card) {
    clear(card).append(
      el("div", { style: "width:64px;height:64px;border-radius:18px;margin:0 auto 1.1rem;display:flex;align-items:center;justify-content:center;background:linear-gradient(135deg,#6366f1,#a371f7);color:#fff" }, icon("sparkles")),
      el("h2", { style: "margin:.3rem 0 .5rem", text: "Finishing sign-in…" }),
      el("p", { class: "dim", style: "line-height:1.55;margin:0 auto 1.3rem;max-width:27rem", text: "Approve the login in your browser, then come back here — this connects automatically." }),
      el("div", { class: "skel", style: "height:8px;width:60%;margin:0 auto;border-radius:4px" }),
    );
  }
  if (!AGENT_POLL_ON) { AGENT_POLL_ON = true; pollAgentStatus(0); }
}
async function pollAgentStatus(n) {
  if (App.route !== "assistant") { AGENT_POLL_ON = false; await finishAgentGuard(); return; }
  try {
    const s = await api("/api/v1/agent/status");
    if (s && s.connected) { AGENT_POLL_ON = false; await finishAgentGuard(); toast("Connected!"); renderAssistantView($("#view")); return; }
  } catch (_) {}
  if (n < 90) { setTimeout(() => pollAgentStatus(n + 1), 2000); }
  else { AGENT_POLL_ON = false; await finishAgentGuard(); }
}

// Swap the connect card to the "paste your code" step.
function showCodeStep() {
  const card = document.getElementById("asst-connect-card");
  if (!card) return;
  clear(card).append(
    el("div", { style: "width:64px;height:64px;border-radius:18px;margin:0 auto 1.1rem;display:flex;align-items:center;justify-content:center;background:linear-gradient(135deg,#6366f1,#a371f7);color:#fff" }, icon("sparkles")),
    el("h2", { style: "margin:.3rem 0 .5rem", text: "Paste your code" }),
    el("p", { class: "dim", style: "line-height:1.55;margin:0 auto 1.3rem;max-width:27rem", text: "After you approve in the browser, copy the code it shows and paste it here to finish connecting." }),
    el("input", { id: "asst-code", class: "input", placeholder: "Paste the code…", style: "max-width:24rem;margin:0 auto .8rem;display:block;width:100%", onkeydown: (e) => { if (e.key === "Enter") completeAiLogin(); } }),
    el("div", { style: "display:flex;gap:.6rem;justify-content:center" },
      el("button", { class: "btn primary", onclick: completeAiLogin }, "Finish connecting"),
      el("button", { class: "btn", onclick: async () => { await finishAgentGuard(); renderAssistantView($("#view")); } }, "Cancel"),
    ),
  );
}

async function completeAiLogin() {
  const inp = document.getElementById("asst-code");
  const code = inp && inp.value.trim();
  if (!code) { toast("Paste the code first"); return; }
  try {
    await post("/api/v1/agent/oauth/complete?" + qs({ code }), CAP.agent);
    await finishAgentGuard();
    toast("Connected!");
    renderAssistantView($("#view"));   // re-fetch status -> switches to chat
  } catch (e) {
    await finishAgentGuard();
    toast("Couldn't connect: " + (e.message || e));
  }
}

const AssistantState = {
  status: null,
  transcript: [],       // [{role:'user'|'assistant', text, chips, stages, results}]
  activeTurnId: null,
  activeStream: null,
  pendingCardsById: new Map(),
  lastUsage: null,
  model: null,
  draft: "",
  busy: false,
  activeMessage: null,
};

function closeAssistantStream(_reason) {
  const stream = AssistantState.activeStream;
  AssistantState.activeStream = null;
  AssistantState.activeTurnId = null;
  AssistantState.activeMessage = null;
  AssistantState.busy = false;
  if (stream) {
    try { stream.close(); } catch (_) {}
  }
}

function rememberAssistantStatus(st) {
  AssistantState.status = st || {};
  AssistantState.lastUsage = st && st.usage ? st.usage : null;
  AssistantState.model = st && (st.provider || st.model)
    ? { provider: st.provider || "", model: st.model || "" }
    : null;
}

function assistantCanUse() {
  return !!CAP.agent && !!App.account;
}

const AGENT_PRIVACY_CONSENT_KEY = "isy_agent_privacy_consent_v1";
const AGENT_PRIVACY_CONSENT_VERSION = 1;
function agentProviderConsentId(provider) {
  if (provider === "claude") return "claude";
  if (provider === "codex") return "codex";
  return provider || "claude";
}
function agentActiveProvider(st) {
  if (st && st.provider) return agentProviderConsentId(st.provider);
  if (st && st.claude) return "claude";
  if (st && st.codex) return "codex";
  return "claude";
}
function readAgentPrivacyConsent() {
  try { return JSON.parse(localStorage.getItem(AGENT_PRIVACY_CONSENT_KEY) || "{}") || {}; }
  catch (_) { return {}; }
}
function agentPrivacyConsentAccepted(provider) {
  const c = readAgentPrivacyConsent();
  return c.version === AGENT_PRIVACY_CONSENT_VERSION
    && c.accepted === true
    && c.provider === agentProviderConsentId(provider);
}
function acceptAgentPrivacyConsent(provider) {
  const record = {
    version: AGENT_PRIVACY_CONSENT_VERSION,
    accepted: true,
    provider: agentProviderConsentId(provider),
    timestamp: new Date().toISOString(),
  };
  try { localStorage.setItem(AGENT_PRIVACY_CONSENT_KEY, JSON.stringify(record)); } catch (_) {}
  renderAssistantView($("#view"));
}
function resetAgentPrivacyConsent() {
  try { localStorage.removeItem(AGENT_PRIVACY_CONSENT_KEY); } catch (_) {}
  renderAssistantView($("#view"));
}

function renderAssistantConsentPanel(providers) {
  const ids = (providers || ["claude"]).map(agentProviderConsentId);
  return el("div", { class: "assistant-consent", "data-agent-consent": "1", "data-testid": "agent-consent" },
    el("div", { class: "assistant-consent-text" },
      el("b", { text: "Privacy consent" }),
      el("p", { class: "dim", text: "The assistant sends selected Microsoft 365 content to the selected provider to answer your question. Continue only if you want that provider to process the selected content." })),
    el("div", { class: "assistant-consent-actions" },
      ids.map(provider => el("button", { class: "btn sm", type: "button", onclick: () => acceptAgentPrivacyConsent(provider), "data-agent-consent-accept": provider },
        icon("shield-check", "icon-sm"), "Allow " + agentProviderLabel(provider))),
      el("button", { class: "btn ghost sm", type: "button", onclick: resetAgentPrivacyConsent, "data-agent-consent-reset": "1" },
        icon("x", "icon-sm"), "Reset")));
}

async function renderAssistantView(view) {
  clear(view).append(
    el("section", { id: "assistant-view", class: "assistant-view", "data-testid": "assistant-view" },
      el("h1", { class: "view-title", text: "Assistant" }),
      el("div", { id: "asst-body", class: "assistant-body" }),
    ),
  );
  const body = $("#asst-body");
  body.append(el("div", { class: "assistant-loading" },
    el("div", { class: "skel", style: "height:20px;width:50%" })));
  let st = {};
  try { st = await api("/api/v1/agent/status"); } catch (_) { st = {}; }
  rememberAssistantStatus(st);
  clear(body);
  if (st && st.connected) renderAssistantChat(body, st);
  else renderAssistantSetup(body, st);
}

// The connect card (shown until an AI account is connected).
function renderAssistantSetup(body, st) {
  const unavailable = !CAP.agent;
  const claudeAllowed = agentPrivacyConsentAccepted("claude");
  const codexAllowed = agentPrivacyConsentAccepted("codex");
  const hint = unavailable
    ? "Assistant is not available in this build."
    : "Sign in with your existing Claude or ChatGPT subscription. iSyncYou opens your device browser for the official login.";
  const claude = el("button", { id: "asst-connect-claude", class: "btn primary", onclick: () => startAiLogin("claude"), "data-testid": "agent-connect-claude" },
    icon("sparkles", "icon-sm"), "Connect Claude");
  const codex = el("button", { id: "asst-connect-codex", class: "btn", onclick: () => startAiLogin("codex"), "data-testid": "agent-connect-codex" },
    icon("sparkles", "icon-sm"), "Connect ChatGPT");
  if (unavailable || !claudeAllowed) {
    claude.setAttribute("disabled", "disabled");
  }
  if (unavailable || !codexAllowed) {
    codex.setAttribute("disabled", "disabled");
  }
  body.append(
    el("p", { class: "view-sub", text: "Ask questions about your Microsoft 365 archive and let the assistant act on it — all within iSyncYou." }),
    el("div", { id: "asst-connect-card", class: "assistant-setup", "data-agent-setup": "1", "data-testid": "agent-setup" },
      el("div", { class: "assistant-setup-icon" }, icon("sparkles")),
      el("h2", { style: "margin:.3rem 0 .5rem", text: "Connect your AI account" }),
      el("p", { class: "dim assistant-setup-copy", text: hint }),
      renderAssistantConsentPanel(["claude", "codex"]),
      el("div", { class: "assistant-setup-actions" }, claude, codex),
      st && st.error ? el("p", { class: "dim assistant-setup-note", text: "Status is temporarily unavailable." }) : null,
      el("p", { class: "dim assistant-setup-note", text: "Uses your Claude or ChatGPT subscription. You can disconnect any time." }),
    ),
  );
}

// The chat surface (shown once connected). Streams tokens over the per-turn SSE.
function renderAssistantChat(body, st) {
  const provider = agentActiveProvider(st);
  const hasConsent = agentPrivacyConsentAccepted(provider);
  const chatNodes = [
    el("div", { class: "assistant-toolbar" },
      el("span", { class: "chip ok" }, el("span", { class: "dot" }), "Connected"),
      agentModelSwitcher(st),
      renderAssistantUsageChip(st),
      el("button", { class: "btn ghost sm", type: "button", onclick: resetAgentPrivacyConsent, "data-agent-consent-reset": "1" },
        icon("shield", "icon-sm"), "Privacy"),
    ),
    el("div", { id: "asst-log", class: "assistant-transcript", "data-agent-transcript": "1", "data-testid": "agent-transcript" }),
    el("div", { class: "assistant-pending-host", "data-agent-pending": "1", "data-testid": "agent-pending-actions" }),
    renderAssistantComposer(st),
  ];
  if (!hasConsent) chatNodes.splice(1, 0, renderAssistantConsentPanel([provider]));
  body.append(...chatNodes);
  const log = $("#asst-log");
  if (!AssistantState.transcript.length) {
    log.append(el("div", { class: "dim", style: "text-align:center;padding:2.5rem 1rem", text: "Ask me anything about your Microsoft 365 — I'll read your archive and answer with sources." }));
  } else {
    AssistantState.transcript.forEach(m => log.append(renderAssistantMessage(m)));
    requestAnimationFrame(() => { log.scrollTop = log.scrollHeight; });
  }
}

function renderAssistantComposer(_st) {
  const provider = agentActiveProvider(_st || AssistantState.status);
  const disabledReason = !CAP.agent ? "Assistant unavailable"
    : (!App.account ? "Select an account first"
      : (!agentPrivacyConsentAccepted(provider) ? "Review privacy consent first" : ""));
  const input = el("textarea", {
    id: "asst-input",
    class: "input assistant-input",
    rows: "1",
    placeholder: disabledReason || "Ask about your mail, files, calendar…",
    onkeydown: agentKeydown,
    oninput: (e) => { AssistantState.draft = e.target.value; },
    "data-agent-input": "1",
    "data-testid": "agent-input",
  });
  input.value = AssistantState.draft || "";
  const send = el("button", { class: "btn primary", title: disabledReason || "Send", onclick: agentSendFromInput, "data-testid": "agent-send" },
    icon("send", "icon-sm"));
  if (disabledReason) {
    input.setAttribute("disabled", "disabled");
    send.setAttribute("disabled", "disabled");
  }
  return el("div", { class: "assistant-composer", "data-agent-composer": "1", "data-testid": "agent-composer" },
    el("div", { class: "asst-inputrow" }, input, send),
    disabledReason ? el("div", { class: "dim assistant-composer-note", text: disabledReason }) : null);
}

function formatAssistantRateLimit(rateLimit) {
  if (!rateLimit) return "";
  if (typeof rateLimit === "string" || typeof rateLimit === "number") return String(rateLimit);
  if (typeof rateLimit !== "object" || Array.isArray(rateLimit)) return "";
  const entries = Object.entries(rateLimit).filter(([, value]) => value != null && value !== "");
  if (!entries.length) return "";
  const status = entries.find(([key, value]) => /status$/i.test(key) && typeof value === "string");
  const utilization = entries.find(([key, value]) => /utilization$/i.test(key) && Number.isFinite(Number(value)));
  const parts = [];
  if (status) parts.push("limit " + status[1]);
  if (utilization) parts.push(Math.round(Number(utilization[1]) * 100) + "%");
  return parts.join(" · ");
}

function renderAssistantUsageChip(st) {
  const usage = st && st.usage ? st.usage : AssistantState.lastUsage;
  if (!usage) {
    return el("span", { class: "chip muted assistant-usage", "data-agent-usage": "1", "data-testid": "agent-usage", title: "Usage unavailable" },
      icon("info", "icon-sm"), "Usage unavailable");
  }
  const parts = [];
  if (usage.request_id) parts.push("Request " + agentCompactValue(usage.request_id, 28));
  if (usage.input_tokens != null) parts.push(`${usage.input_tokens} in`);
  if (usage.output_tokens != null) parts.push(`${usage.output_tokens} out`);
  const rateLimit = formatAssistantRateLimit(usage.rate_limit);
  if (rateLimit) parts.push(rateLimit);
  return el("span", { class: "chip assistant-usage", "data-agent-usage": "1", "data-testid": "agent-usage" },
    icon("info", "icon-sm"), parts.join(" · ") || "Usage");
}

// Model switcher: pick any available Claude/Codex model. The choice is persisted
// server-side (agent-settings) and applies to the next turn.
// Custom in-UI model picker (no native <select>): a glass panel that unfolds with a
// spring animation, models grouped by provider, active one highlighted. Falls back to a
// plain label when nothing is connected.
function agentProviderLabel(provider) {
  if (provider === "claude") return "Claude";
  if (provider === "codex") return "ChatGPT";
  return "Assistant";
}
function agentModelSwitcher(st) {
  const models = st.models || {};
  const cur = (st.provider || "") + "|" + (st.model || "");
  const curLabel = () => {
    for (const prov of ["claude", "codex"]) {
      const tag = agentProviderLabel(prov);
      const m = (models[prov] || []).find((x) => prov + "|" + x.id === cur);
      if (m) return tag + " · " + m.label;
    }
    return st.model ? agentProviderLabel(st.provider) + " · " + st.model : "Select model";
  };
  const rows = [];
  const addGroup = (prov, connected) => {
    const list = models[prov] || [];
    if (!connected || !list.length) return;
    const tag = agentProviderLabel(prov);
    rows.push(el("div", { class: "mdl-group" }, tag));
    list.forEach((m) => {
      const val = prov + "|" + m.id;
      rows.push(el("button",
        { class: "mdl-item" + (val === cur ? " active" : ""), type: "button", role: "option", "data-agent-model-option": val, onclick: () => pickModel(prov, m.id) },
        el("span", { class: "mdl-dot" }),
        el("span", { class: "mdl-lbl", text: tag + " · " + m.label })));
    });
  };
  addGroup("claude", st.claude);
  addGroup("codex", st.codex);
  if (!st.codex) {
    rows.push(el("button", { class: "mdl-item mdl-connect", type: "button", "data-agent-model-connect": "codex", onclick: () => connectCodex() },
      el("span", { class: "mdl-plus" }, "＋"),
      el("span", { class: "mdl-lbl", text: "Connect ChatGPT…" })));
  }
  if (!rows.length) {
    return el("span", {
      class: "dim mdl-empty",
      style: "font-size:.85rem",
      "data-agent-model-picker": "1",
      "data-testid": "agent-model-picker",
      text: st.model || "No model available",
    });
  }

  const wrap = el("div", { class: "mdl", "data-agent-model-picker": "1", "data-testid": "agent-model-picker" });
  const closeOutside = (ev) => { if (!wrap.contains(ev.target)) close(); };
  const close = () => { wrap.classList.remove("open"); document.removeEventListener("pointerdown", closeOutside, true); };
  const trigger = el("button",
    { class: "mdl-trigger", type: "button", "aria-haspopup": "listbox", title: "Switch model",
      onclick: (ev) => {
        ev.stopPropagation();
        if (wrap.classList.toggle("open")) document.addEventListener("pointerdown", closeOutside, true);
        else document.removeEventListener("pointerdown", closeOutside, true);
      } },
    el("span", { class: "mdl-cur", text: curLabel() }), icon("chevron-down", "mdl-caret"));
  wrap.append(trigger, el("div", { class: "mdl-panel", role: "listbox" }, ...rows));
  return wrap;
}
async function pickModel(provider, model) {
  if (!agentPrivacyConsentAccepted(provider)) {
    toast("Review privacy consent for " + agentProviderLabel(provider), "err");
    renderAssistantView($("#view"));
    return;
  }
  try {
    await post("/api/v1/agent/model?" + qs({ provider, model }), CAP.agent);
    const st = await api("/api/v1/agent/status");
    rememberAssistantStatus(st);
    toast("Model: " + agentProviderLabel(provider) + " · " + model);
    renderAssistantView($("#view"));
  } catch (err) {
    toast("Could not switch model: " + (err.message || err));
  }
}
async function connectCodex() {
  if (!agentPrivacyConsentAccepted("codex")) {
    toast("Review privacy consent for ChatGPT", "err");
    renderAssistantView($("#view"));
    return;
  }
  let guardId = null;
  try {
    const params = { provider: "codex" };
    const redirect = localCallbackRedirect("127.0.0.1");
    if (redirect) params.redirect = redirect;
    const d = await post("/api/v1/agent/oauth/start?" + qs(params), CAP.agent);
    if (!d || !d.authorize_url) { toast("Could not start ChatGPT sign-in"); return; }
    toast("Opening ChatGPT sign-in…");
    guardId = await beginNetworkGuard();
    CODEX_GUARD_ID = guardId;
    await openExternalAuth(d.authorize_url, "agent_authorize");
    pollCodexStatus(0);
  } catch (e) {
    if (guardId) await endNetworkGuard(guardId);
    if (CODEX_GUARD_ID === guardId) CODEX_GUARD_ID = null;
    toast("ChatGPT sign-in unavailable: " + (e.message || e));
  }
}
async function pollCodexStatus(n) {
  try {
    const s = await api("/api/v1/agent/status");
    if (s && s.codex) { await finishCodexGuard(); toast("ChatGPT connected!"); renderAssistantView($("#view")); return; }
  } catch (_) {}
  if (n < 90) setTimeout(() => pollCodexStatus(n + 1), 2000);
  else await finishCodexGuard();
}

// Progressive-search rendering (S-AG.18/#643, S-AG.19/#644). Module-level so BOTH the live
// stream (agentSend) and a re-render from AssistantState.transcript build
// identical cards — the transcript keeps its search stages + result cards, not just the text.
const ASST_STAGE_LABEL = { names: "Fast search — subject", bodies: "Full-text — bodies", deep: "AI deep-read" };
function asstSvcIcon(s) { return ({ mail: "mail", onedrive: "hard-drive", calendar: "calendar", contacts: "users", todo: "check-square", onenote: "notebook" })[s] || "file"; }
// The app's canonical item viewer — the SAME sandboxed, same-origin iframe the Mail reader
// uses (`/api/v1/view` renders sanitized-HTML mail / a rendered item; frame-src 'self' +
// frame-ancestors 'self' allow the shell to embed it). Reused so search shows the real,
// properly-formatted body — not a hand-rolled text extract.
function asstViewerFrame(q) {
  return el("iframe", { class: "asst-result-frame", src: `/api/v1/view?${qs(q)}`, title: "Item preview", sandbox: "allow-same-origin" });
}
function normalizeAgentSource(value) {
  if (!value || typeof value !== "object") return null;
  const raw = value.source && typeof value.source === "object" ? value.source : value;
  const service = String(raw.service || value.service || "").trim();
  if (!service || !archiveServices().some(s => s.id === service)) return null;
  const id = String(raw.id || raw.remote_id || value.id || value.remote_id || "").trim();
  const path = raw.path || value.path || "";
  if (!id && !path) return null;
  return {
    service,
    id,
    path: path ? String(path) : "",
    name: String(raw.name || value.name || value.displayName || id || service),
    item_type: String(raw.item_type || value.item_type || value.type || service),
  };
}
function sourceViewQuery(source) {
  if (!source || !App.account || !source.service || !source.id || !source.path) return null;
  return { account: App.account, service: source.service, id: source.id };
}
function sourceViewHref(source) {
  const q = sourceViewQuery(source);
  return q ? "/api/v1/view?" + qs(q) : null;
}
function agentSourceKey(source) {
  return JSON.stringify([source.service, source.id || "", source.path || ""]);
}
function dedupeAgentSources(sources) {
  const seen = new Set();
  const out = [];
  (sources || []).forEach((s) => {
    if (!s) return;
    const key = agentSourceKey(s);
    if (seen.has(key)) return;
    seen.add(key);
    out.push(s);
  });
  return out;
}
function extractAgentSources(event) {
  const found = [];
  const visit = (value, depth) => {
    if (depth > 6 || value == null) return;
    if (Array.isArray(value)) { value.forEach(v => visit(v, depth + 1)); return; }
    if (typeof value !== "object") return;
    const src = normalizeAgentSource(value);
    if (src) found.push(src);
    Object.entries(value).forEach(([k, v]) => {
      if (["source", "sources", "results", "items", "result", "data"].includes(k)) visit(v, depth + 1);
    });
  };
  if (!event || typeof event !== "object") return [];
  if (event.event === "partial_result") visit(event.items || [], 0);
  else if (event.event === "tool_result" && typeof event.content === "string") {
    try { visit(JSON.parse(event.content), 0); } catch (_) {}
  }
  return dedupeAgentSources(found);
}
function renderAgentCitation(source) {
  const label = source.name || source.id || source.service;
  const href = sourceViewHref(source);
  if (href) {
    return el("a", { class: "asst-citation", href, target: "_blank", rel: "noopener", "data-agent-citation": "view" },
      icon(asstSvcIcon(source.service), "icon-sm"),
      el("span", { text: label }));
  }
  return el("button", { class: "asst-citation", type: "button", onclick: () => go(source.service), "data-agent-citation": "route" },
    icon(asstSvcIcon(source.service), "icon-sm"),
    el("span", { text: label }));
}
function renderAgentCitationBar(sources) {
  const bar = el("div", { class: "asst-citations", "data-agent-citations": "1" });
  dedupeAgentSources(sources).forEach(source => bar.append(renderAgentCitation(source)));
  return bar;
}
// One typed result: header (name) + one-line preview; click → animated pull-down that
// lazily embeds the real viewer for the body + a link to open it full-screen.
function asstResultCard(it) {
  const source = normalizeAgentSource(it) || { service: it.service, id: it.id, path: it.path || "", name: it.name || "", item_type: it.item_type || it.service };
  const viewQ = sourceViewQuery(source);
  const snip = (it.snippet || "").trim();
  const head = el("div", { class: "asst-result-head" },
    el("span", { class: "asst-result-ic", style: `--svc:var(--svc-${it.service})` }, icon(asstSvcIcon(it.service), "icon-sm")),
    el("div", { class: "asst-result-main grow" },
      el("div", { class: "asst-result-name truncate", text: it.name || "(no name)" }),
      el("div", { class: "asst-result-sub truncate", text: snip || (it.item_type || it.service) })),
    el("span", { class: "asst-result-type", text: it.item_type || it.service }),
    el("span", { class: "asst-result-caret" }, icon("chevron-down", "icon-sm")));
  const panel = el("div", { class: "asst-result-panel" });
  const row = el("div", { class: "asst-result" }, head, panel);
  let loaded = false;
  head.addEventListener("click", () => {
    const opening = !row.classList.contains("open");
    row.classList.toggle("open");
    if (opening && !loaded) {   // lazy: only load the viewer when the user opens the card
      loaded = true;
      if (viewQ) {
        panel.append(
          asstViewerFrame(viewQ),
          el("a", { class: "asst-result-open", href: "/api/v1/view?" + qs(viewQ), target: "_blank", rel: "noopener" },
            icon("external-link", "icon-sm"), el("span", { text: "Open full " + (it.item_type || "item") })));
      } else {
        panel.append(el("button", { class: "asst-result-open", type: "button", onclick: () => go(source.service) },
          icon("chevron-right", "icon-sm"), el("span", { text: "Open " + (source.service || "service") })));
      }
    }
  });
  return row;
}
function asstStageRowDone(stage, hits) {
  return el("div", { class: "asst-stage done" },
    el("span", { class: "asst-stage-ic", text: "✓" }),
    el("span", { class: "grow", text: ASST_STAGE_LABEL[stage] || stage }),
    el("span", { class: "asst-stage-n dim", text: hits + (hits === 1 ? " hit" : " hits") }));
}
// Rebuild a turn's search block (final stages + result cards) from stored data.
function asstSearchBlock(stages, results) {
  const frag = document.createDocumentFragment();
  if (stages && stages.length) {
    const sb = el("div", { class: "asst-search" });
    stages.forEach(s => sb.append(asstStageRowDone(s.stage, s.hits)));
    frag.append(sb);
  }
  if (results && results.length) {
    const rb = el("div", { class: "asst-results" });
    results.forEach(it => rb.append(asstResultCard(it)));
    frag.append(rb);
  }
  return frag;
}

function agentCompactValue(v, max = 140) {
  if (v == null) return "";
  let s;
  try { s = typeof v === "string" ? v : JSON.stringify(v); } catch (_) { s = String(v); }
  s = s.replace(/\s+/g, " ").trim();
  s = s.replace(/("?)(token|session|capability|action_hash)\1\s*[:=]\s*"?[^,"\s}]+/ig, "$2=[redacted]");
  return s.length > max ? s.slice(0, max - 1) + "…" : s;
}

function summarizeAgentToolInput(input) {
  if (!input || typeof input !== "object") return "";
  const deny = new Set(["token", "session", "session_token", "capability", "capability_token", "action_hash"]);
  const bits = [];
  Object.entries(input).forEach(([k, v]) => {
    if (bits.length >= 4 || deny.has(String(k).toLowerCase())) return;
    if (v == null || typeof v === "object") return;
    bits.push(`${k}: ${agentCompactValue(v, 42)}`);
  });
  return bits.join(" · ");
}

function renderAgentToolRow(row) {
  return el("div", { class: "asst-tool-row " + row.kind, "data-agent-tool-row": row.kind },
    icon(row.kind === "tool_call" ? "corner-up-right" : "corner-up-left", "icon-sm"),
    el("span", { class: "asst-tool-title", text: row.title }),
    row.detail ? el("span", { class: "asst-tool-detail", text: row.detail }) : null,
    row.untrusted ? el("span", { class: "chip muted asst-tool-boundary", text: "Source content" }) : null);
}

function renderAgentError(message) {
  return el("div", { class: "asst-inline-error", "data-agent-stream-error": "1" },
    icon("shield", "icon-sm"),
    el("span", { text: message || "Stream error" }));
}

function pendingRecord(pendingId) {
  return pendingId ? AssistantState.pendingCardsById.get(pendingId) : null;
}

function rerenderPendingCards(pendingId) {
  document.querySelectorAll("[data-agent-pending-card]").forEach((node) => {
    if (node.getAttribute("data-agent-pending-card") !== pendingId) return;
    const record = pendingRecord(pendingId);
    if (record) node.replaceWith(renderAgentPendingCard(record));
  });
}

function pendingStatus(pending) {
  const status = pending.status || "pending";
  if (status === "pending" && pending.expires_at_ms && Date.now() >= Number(pending.expires_at_ms)) {
    pending.status = "expired";
    return "expired";
  }
  return status;
}

async function confirmAgentPending(pendingId) {
  const record = pendingRecord(pendingId);
  if (!record || !record.token || !record.action_hash) return;
  record.status = "confirming";
  record.error = "";
  rerenderPendingCards(pendingId);
  try {
    const d = await post("/api/v1/agent/confirm?" + qs({ pending: pendingId, token: record.token, action_hash: record.action_hash }), CAP.agent);
    record.status = "confirmed";
    record.result = d && d.result ? d.result : "Confirmed";
    record.token = "";
    record.action_hash = "";
  } catch (e) {
    record.status = "error";
    record.error = agentCompactValue(e.message || e, 180);
  }
  rerenderPendingCards(pendingId);
}

async function cancelAgentPending(pendingId) {
  const record = pendingRecord(pendingId);
  if (!record) return;
  const turn = record.turn_id || AssistantState.activeTurnId;
  if (!turn) {
    record.status = "error";
    record.error = "Missing turn id";
    rerenderPendingCards(pendingId);
    return;
  }
  record.status = "cancelling";
  record.error = "";
  rerenderPendingCards(pendingId);
  try {
    await post("/api/v1/agent/cancel?" + qs({ turn }), CAP.agent);
    record.status = "cancelled";
    record.token = "";
    record.action_hash = "";
    closeAssistantStream("pending-cancel");
  } catch (e) {
    record.status = "error";
    record.error = agentCompactValue(e.message || e, 180);
  }
  rerenderPendingCards(pendingId);
}

function renderAgentPendingCard(pending) {
  const status = pendingStatus(pending);
  const risk = pending.risk ? `Risk: ${pending.risk}` : "Review required";
  const done = status === "confirmed" || status === "cancelled";
  const waiting = status === "confirming" || status === "cancelling";
  const confirmDisabled = waiting || done || status === "expired";
  const cancelDisabled = waiting || done;
  const confirm = el("button", { class: "btn primary sm", type: "button", onclick: () => confirmAgentPending(pending.pending_id), "data-agent-pending-confirm": pending.pending_id || "" },
    icon("check", "icon-sm"), status === "confirming" ? "Confirming…" : "Confirm");
  const cancel = el("button", { class: "btn sm", type: "button", onclick: () => cancelAgentPending(pending.pending_id), "data-agent-pending-cancel": pending.pending_id || "" },
    icon("x", "icon-sm"), status === "cancelling" ? "Cancelling…" : "Cancel");
  if (confirmDisabled) confirm.setAttribute("disabled", "disabled");
  if (cancelDisabled) cancel.setAttribute("disabled", "disabled");
  return el("div", {
    class: "asst-pending-card " + status,
    "data-agent-pending-card": pending.pending_id || "",
  },
    el("div", { class: "asst-pending-head" },
      icon("shield-check", "icon-sm"),
      el("span", { class: "asst-pending-title", text: pending.preview || "Action requires confirmation" })),
    el("div", { class: "dim asst-pending-meta", text: risk }),
    pending.expires_at_ms ? el("div", { class: "dim asst-pending-meta", text: status === "expired" ? "Expired" : "Expires " + fmtDate(pending.expires_at_ms) }) : null,
    pending.result ? el("div", { class: "asst-pending-result", text: pending.result }) : null,
    pending.error ? el("div", { class: "asst-pending-error", text: pending.error }) : null,
    el("div", { class: "asst-pending-actions" }, confirm, cancel));
}

function renderAssistantMessage(m) {
  const isUser = m.role === "user";
  const bubble = el("div", {
    class: "card asst-bubble",
  }, el("div", { class: "asst-text", style: "white-space:pre-wrap;line-height:1.5", text: m.text || (isUser ? "" : "…") }));
  (m.chips || []).forEach(c => bubble.append(el("div", { class: "dim", dataset: { chip: "1" }, style: "font-size:.78rem;margin-top:.35rem", text: c })));
  (m.tools || []).forEach(t => bubble.append(renderAgentToolRow(t)));
  (m.errors || []).forEach(e => bubble.append(renderAgentError(e)));
  if (m.pending) bubble.append(renderAgentPendingCard(m.pending));
  if (m.citations && m.citations.length) bubble.append(renderAgentCitationBar(m.citations));
  // Re-render the turn's search stages + result cards (persisted in the message), so a view
  // switch brings the whole conversation back — not just the text (#644).
  if ((m.stages && m.stages.length) || (m.results && m.results.length)) bubble.append(asstSearchBlock(m.stages, m.results));
  return el("div", { class: "assistant-message" + (isUser ? " user" : ""), "data-agent-message": m.role || "assistant" }, bubble);
}

function handleAgentEvent(message, turnState) {
  const d = message || {};
  switch (d.event) {
    case "token":
      turnState.setText(turnState.message.text + (d.text || ""));
      break;
    case "tool_call":
      turnState.addToolRow({
        kind: "tool_call",
        title: d.name || "Tool call",
        detail: summarizeAgentToolInput(d.input),
      });
      break;
    case "tool_result":
      turnState.addToolRow({
        kind: "tool_result",
        title: "Tool result",
        detail: agentCompactValue(d.content, 120),
        untrusted: !!d.untrusted,
      });
      turnState.addCitations(extractAgentSources(d));
      break;
    case "search_stage":
      turnState.onSearchStage(d);
      break;
    case "partial_result":
      turnState.onPartialResult(d);
      turnState.addCitations(extractAgentSources(d));
      break;
    case "confirmation_required": {
      const pending = {
        pending_id: d.pending_id || d.id || d.tool_id || "",
        preview: d.preview || "Action requires confirmation",
        risk: d.risk || "",
        expires_at_ms: d.expires_at_ms || null,
        turn_id: AssistantState.activeTurnId || "",
        status: "pending",
        result: "",
        error: "",
        token: d.token || "",
        action_hash: d.action_hash || "",
      };
      if (pending.pending_id) {
        AssistantState.pendingCardsById.set(pending.pending_id, pending);
      }
      turnState.setPending(pending);
      break;
    }
    case "error":
      turnState.addError(d.message || "Stream error");
      break;
    case "done": {
      const reason = d.reason || "complete";
      const fallback = reason === "pending_confirmation" ? "Waiting for confirmation"
        : reason === "cancelled" ? "Cancelled"
        : reason === "error" ? "Turn ended with an error"
        : "(no response)";
      turnState.message.doneReason = reason;
      turnState.finish(fallback, reason);
      break;
    }
  }
}

function agentKeydown(e) {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); agentSendFromInput(); }
}
function agentSendFromInput() {
  const inp = $("#asst-input"); if (!inp) return;
  const text = inp.value.trim(); if (!text) return;
  inp.value = "";
  agentSend(text);
}

// Start a turn and stream its tokens into a fresh assistant bubble.
async function agentSend(text) {
  const log = $("#asst-log"); if (!log) return;
  if (!assistantCanUse()) { toast("Assistant is unavailable for this account", "err"); return; }
  const provider = agentActiveProvider(AssistantState.status);
  if (!agentPrivacyConsentAccepted(provider)) {
    toast("Review privacy consent for " + agentProviderLabel(provider), "err");
    renderAssistantView($("#view"));
    return;
  }
  closeAssistantStream("new-turn");
  if (!AssistantState.transcript.length) clear(log);   // drop the empty-state hint

  AssistantState.transcript.push({ role: "user", text });
  log.append(renderAssistantMessage(AssistantState.transcript[AssistantState.transcript.length - 1]));
  const asst = { role: "assistant", text: "", chips: [], stages: [], results: [], tools: [], errors: [], citations: [], pending: null, doneReason: null };
  AssistantState.transcript.push(asst);
  AssistantState.busy = true;
  AssistantState.draft = "";
  AssistantState.activeMessage = asst;
  const asstEl = renderAssistantMessage(asst);
  log.append(asstEl);
  const textEl = asstEl.querySelector(".asst-text");
  const bubble = asstEl.querySelector(".asst-bubble");
  // Immediate animated "working" ack (#644) instead of a bare "…" while the model thinks
  // before its first token / search stage. Removed on the first real content.
  textEl.textContent = "";
  const thinkingEl = el("div", { class: "asst-thinking" },
    el("span", { class: "asst-thinking-dot" }), el("span", { class: "asst-thinking-dot" }), el("span", { class: "asst-thinking-dot" }),
    el("span", { class: "asst-thinking-label dim", text: "Searching your Microsoft 365…" }));
  bubble.insertBefore(thinkingEl, textEl);
  let thinkingDone = false;
  const clearThinking = () => { if (!thinkingDone) { thinkingDone = true; thinkingEl.remove(); } };
  log.scrollTop = log.scrollHeight;
  const setText = (t) => { if (t) clearThinking(); asst.text = t; textEl.textContent = t || ""; log.scrollTop = log.scrollHeight; };
  const addToolRow = (row) => {
    clearThinking();
    asst.tools.push(row);
    bubble.append(renderAgentToolRow(row));
    log.scrollTop = log.scrollHeight;
  };
  const addError = (message) => {
    clearThinking();
    asst.errors.push(message);
    bubble.append(renderAgentError(message));
    log.scrollTop = log.scrollHeight;
  };
  const setPending = (pending) => {
    clearThinking();
    asst.pending = pending;
    const old = bubble.querySelector("[data-agent-pending-card]");
    if (old) old.remove();
    bubble.append(renderAgentPendingCard(pending));
    log.scrollTop = log.scrollHeight;
  };
  let citationsBox = null;
  const addCitations = (sources) => {
    const merged = dedupeAgentSources([...(asst.citations || []), ...(sources || [])]);
    if (merged.length === (asst.citations || []).length) return;
    asst.citations = merged;
    if (citationsBox) citationsBox.remove();
    citationsBox = renderAgentCitationBar(asst.citations);
    bubble.append(citationsBox);
    log.scrollTop = log.scrollHeight;
  };

  // Progressive-search UI (S-AG.18/#643): a small plan with a live checkmark per stage
  // and a result list that grows as PartialResult events arrive.
  const STAGE_LABEL = { names: "Fast search — subject", bodies: "Full-text — bodies", deep: "AI deep-read" };
  let searchBox = null, resultsBox = null; const stageRow = {};
  const ensureSearchUI = () => {
    clearThinking();   // the search plan replaces the generic "working" indicator
    if (searchBox) return;
    searchBox = el("div", { class: "asst-search" });
    resultsBox = el("div", { class: "asst-results" });
    bubble.append(searchBox, resultsBox);
    log.scrollTop = log.scrollHeight;
  };
  const onSearchStage = (d) => {
    ensureSearchUI();
    let row = stageRow[d.stage];
    if (!row) {
      row = el("div", { class: "asst-stage" }, el("span", { class: "asst-stage-ic" }), el("span", { class: "grow", text: STAGE_LABEL[d.stage] || d.stage }), el("span", { class: "asst-stage-n dim" }));
      stageRow[d.stage] = row; searchBox.append(row);
    }
    const done = d.status === "done";
    row.classList.toggle("done", done);
    row.querySelector(".asst-stage-ic").textContent = done ? "✓" : "";
    if (done) {
      row.querySelector(".asst-stage-n").textContent = d.hits + (d.hits === 1 ? " hit" : " hits");
      const e = asst.stages.find(s => s.stage === d.stage);   // persist final stage state
      if (e) e.hits = d.hits; else asst.stages.push({ stage: d.stage, hits: d.hits });
    }
    log.scrollTop = log.scrollHeight;
  };
  const onPartialResult = (d) => {
    ensureSearchUI();
    (d.items || []).forEach((it) => {
      asst.results.push(it);                 // persist so the cards survive a view switch
      resultsBox.append(asstResultCard(it));  // module-level builder (shared with re-render)
    });
    log.scrollTop = log.scrollHeight;
  };

  let turn;
  try {
    const r = await post("/api/v1/agent/turn?" + qs({ account: App.account, prompt: text }), CAP.agent);
    turn = r && r.turn;
  } catch (e) {
    AssistantState.busy = false;
    AssistantState.activeMessage = null;
    setText("Error: " + (e.message || e));
    return;
  }
  if (!turn) {
    AssistantState.busy = false;
    AssistantState.activeMessage = null;
    setText("Error: could not start the turn");
    return;
  }
  AssistantState.activeTurnId = turn;

  const url = "/api/v1/agent/stream?" + qs({ turn });
  // Transport-abstracted (#0A): the native bridge push channel on the phone, EventSource on
  // desktop. The agent's events arrive as `message` (a data line to JSON-parse); a `done`
  // event or a stream drop ends the turn.
  let stream;
  const finish = (msg) => {
    clearThinking();
    if (AssistantState.activeStream === stream) closeAssistantStream("turn-finish");
    else { try { stream.close(); } catch (_) {} }
    if (!asst.text && msg) setText(msg);
  };
  const turnState = {
    message: asst,
    setText,
    addToolRow,
    addError,
    setPending,
    addCitations,
    onSearchStage,
    onPartialResult,
    finish,
  };
  stream = openEventStream(url, (name, data) => {
    if (name === "done") { handleAgentEvent({ event: "done", reason: "complete" }, turnState); return; }
    if (name !== "message") return; // ignore ping heartbeats
    let d;
    try { d = JSON.parse(data); }
    catch (_) {
      addError("Invalid stream payload");
      finish("Stream error");
      return;
    }
    handleAgentEvent(d, turnState);
  }, () => finish("⚠ connection lost"));
  AssistantState.activeStream = stream;
}

function renderAccountMenu(body) {
  if (accountMenuPoll) { clearInterval(accountMenuPoll); accountMenuPoll = null; }
  clear(body);
  App.accounts.forEach(a => {
    const active = a.id === App.account;
    const row = el("div", { class: "acct-row" + (active ? " active" : "") },
      el("span", { class: "avatar mail-av", style: "--c:var(--accent)", text: initials(a.username || a.id) }),
      el("button", { class: "acct-pick grow", title: "Switch to this account", onclick: () => { if (!active) { App.account = a.id; toast("Switched to " + (a.username || a.id)); closeAccountMenu(); onRoute(); } } },
        el("div", { class: "truncate", text: a.username || a.id }),
        el("div", { class: "dim", style: "font-size:11px", text: active ? "Active" : a.id })),
      active ? icon("check", "icon-sm") : null);
    if (CAP.account) {
      const acts = el("div", { class: "acct-acts" });
      acts.append(el("button", { class: "btn ghost sm icon-only", title: "Sign in / reconnect (device code)", onclick: () => startDeviceLogin(a, body) }, icon("rotate-ccw", "icon-sm")));
      acts.append(el("button", { class: "btn ghost sm icon-only", style: "color:var(--danger,#f87171)", title: "Sign out — clear cached token", onclick: () => accountSignOut(a, body) }, icon("trash-2", "icon-sm")));
      row.append(acts);
    }
    body.append(row);
  });
  body.append(el("div", { class: "acct-note dim" }, CAP.account
    ? "Reconnect re-runs device-code sign-in. Adding a brand-new account still needs `isyncyou setup` (live add is the remaining backend work)."
    : "Read-only server — account switching only."));
}
async function accountSignOut(a, body) {
  try { const d = await post("/api/v1/account/signout?account=" + encodeURIComponent(a.id), CAP.account); toast(d.message || "Signed out"); }
  catch (e) { toast("Sign-out failed: " + e.message, "err"); }
  renderAccountMenu(body);
}
async function startDeviceLogin(a, body) {
  clear(body).append(el("div", { class: "acct-dc" }, el("div", { class: "spinner" }), el("div", { class: "dim", text: "Starting sign-in…" })));
  let dc;
  try { dc = await post("/api/v1/account/login/start?account=" + encodeURIComponent(a.id), CAP.account); }
  catch (e) { toast("Sign-in failed: " + e.message, "err"); renderAccountMenu(body); return; }
  const openDeviceLogin = async () => {
    try {
      await openExternalAuth(dc.verification_uri, "account_device_code", { newTab: true });
    } catch (e) {
      toast("Could not open sign-in page: " + (e.message || e), "err");
    }
  };
  const status = el("div", { class: "acct-dc-status dim", text: "Waiting for you to sign in…" });
  clear(body).append(el("div", { class: "acct-dc" },
    el("div", { class: "acct-dc-title", text: "Sign in to " + (a.username || a.id) }),
    el("p", { class: "dim", text: "Open the page and enter this code:" }),
    el("div", { class: "acct-dc-code", text: dc.user_code || "—" }),
    el("button", { class: "btn sm primary", type: "button", onclick: openDeviceLogin }, icon("external-link", "icon-sm"), "Open sign-in page"),
    status,
    el("button", { class: "btn ghost sm", style: "margin-top:8px", onclick: () => renderAccountMenu(body) }, "Cancel")));
  accountMenuPoll = setInterval(async () => {
    let r;
    try { r = await post("/api/v1/account/login/poll?id=" + encodeURIComponent(dc.login_id), CAP.account); }
    catch (e) { clearInterval(accountMenuPoll); accountMenuPoll = null; status.textContent = "Poll error: " + e.message; return; }
    if (r.state === "done") { clearInterval(accountMenuPoll); accountMenuPoll = null; toast("Signed in to " + (a.username || a.id)); closeAccountMenu(); onRoute(); }
    else if (r.state === "error") { clearInterval(accountMenuPoll); accountMenuPoll = null; status.textContent = "Sign-in failed: " + (r.error || "unknown"); }
  }, 3000);
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
    ...visibleServices().map(s => ({ label: "Go to " + s.label, icon: s.icon, run: () => { closePalette(); go(s.id); } })),
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
let _bdT, _evtT;
// Subscribe to the daemon's SSE change stream: on a cloud/sync change, refetch the
// active view (near-real-time). EventSource auto-reconnects if the daemon restarts.
function subscribeEvents() {
  if (!BRIDGE && !window.EventSource) return;
  // Transport-abstracted (#0A): the native bridge push channel on the phone, EventSource on desktop.
  const path = "/api/v1/events";
  openEventStream(path, (name) => {
    if (name !== "change") return; // ignore ping heartbeats
    clearTimeout(_evtT);
    _evtT = setTimeout(() => {
      // Soft, in-place refresh — NO shell/view teardown, no filter/search/scroll/selection
      // reset, so a background sync tick never "reloads the whole screen". A view that
      // supports live updates registers App.liveUpdate (mail does); it re-fetches and
      // patches only what changed. Views without one keep the old full refresh on desktop
      // and refresh on the next navigation on the phone (never a jarring full reload).
      if (App.liveUpdate) { try { App.liveUpdate(); } catch (_) {} return; }
      if (MOBILE) return;
      onRoute();
    }, 150);
  // EventSource auto-reconnects natively; the bridge push channel doesn't, so re-subscribe
  // after a short backoff when a bridge stream drops.
  }, BRIDGE ? () => setTimeout(subscribeEvents, 3000) : undefined);
}
// Mobile touch navigation (#77): a horizontal swipe on the content navigates —
// a right-swipe with an open detail goes back to the list; otherwise swipe
// left/right moves to the next/previous service tab (bottom-nav order). Vertical
// scrolls, slow drags, overlay-open and swipes inside a horizontally-scrollable
// element (e.g. the ToDo board) are ignored so they keep their native behaviour.
function mobileBack() {
  if (App.route === "mail" && Mail.selected) { mailBack(); return true; }
  if (App.route === "contacts" && Contacts.selected) { contactBack(); return true; }
  return false;
}
function setupSwipe() {
  const isMobile = () => window.matchMedia("(max-width: 720px)").matches;
  let x0 = null, y0 = null, t0 = 0, lockH = false;
  document.addEventListener("touchstart", (e) => {
    if (!isMobile() || e.touches.length !== 1) { x0 = null; return; }
    const t = e.touches[0];
    x0 = t.clientX; y0 = t.clientY; t0 = Date.now(); lockH = false;
    for (let n = e.target; n && n !== document.body; n = n.parentElement) {
      if (n.scrollWidth > n.clientWidth + 4) {
        const ox = getComputedStyle(n).overflowX;
        if (ox === "auto" || ox === "scroll") { lockH = true; break; }
      }
    }
  }, { passive: true });
  document.addEventListener("touchend", (e) => {
    if (x0 == null || lockH) { x0 = null; return; }
    const t = e.changedTouches[0];
    const dx = t.clientX - x0, dy = t.clientY - y0, dt = Date.now() - t0;
    x0 = null;
    if (dt > 600 || Math.abs(dx) < 64 || Math.abs(dx) < Math.abs(dy) * 1.6) return;
    if (sheetEl || palette) return;                       // overlay owns the gesture
    if (dx > 0 && mobileBack()) return;                   // right-swipe → back from detail
    const order = visibleServices().map((s) => s.id);
    const i = order.indexOf(App.route);
    if (i < 0) return;
    if (dx < 0 && i < order.length - 1) go(order[i + 1]); // left → next tab
    else if (dx > 0 && i > 0) go(order[i - 1]);           // right → previous tab
  }, { passive: true });
}
// Performance overlay (test flag): a live HUD of the app's whole-process load — CPU%, the
// GPU/render-thread cost, RAM, disk IO, disk-wait and the system IO-queue depth — from
// /api/v1/debug/stats (embedded engine + WebView are one process, so it's the total the app
// causes). Toggled from Settings → Diagnostics, or window.togglePerf().
//
// "GPU/Rend" is a proxy: Android exposes no per-process GPU%, so we surface the CPU spent by the
// WebView render/compositor/GPU threads — the render work GPU-bound animation drives.
let _perfTimer = null, _perfPrev = null;
function perfRate(bps) {
  if (!isFinite(bps) || bps < 1) return "0";
  const u = ["B/s", "KB/s", "MB/s"]; let i = 0;
  while (bps >= 1024 && i < 2) { bps /= 1024; i++; }
  return bps.toFixed(i ? 1 : 0) + " " + u[i];
}
function startPerfOverlay() {
  if (document.getElementById("perf-hud")) return;
  const mk = (k, id) => el("div", { class: "perf-row" }, el("span", { class: "perf-k", text: k }), el("span", { id, class: "perf-v", text: "…" }));
  document.body.append(el("div", { id: "perf-hud", class: "perf-hud" },
    mk("CPU", "perf-cpu"), mk("GPU/Rend", "perf-gpu"), mk("RAM", "perf-ram"),
    mk("Disk R", "perf-ior"), mk("Disk W", "perf-iow"),
    mk("Disk wait", "perf-iowait"), mk("IO queue", "perf-ioq")));
  _perfPrev = null;
  const tick = async () => {
    let s; try { s = await api("/api/v1/debug/stats"); } catch { return; }
    const now = performance.now();
    const cores = s.cores || 1;
    if (_perfPrev) {
      const dt = (now - _perfPrev.t) / 1000;
      const pct = (cur, prev) => dt > 0 ? Math.max(0, (cur - prev) / (dt * 1000) * 100) : 0;
      const cpu = pct(s.cpu_ms, _perfPrev.cpu_ms);
      const cpuEl = $("#perf-cpu");
      if (cpuEl) { cpuEl.textContent = cpu.toFixed(0) + "%"; cpuEl.classList.toggle("hot", cpu > cores * 55); }
      const gpu = pct(s.render_ms || 0, _perfPrev.render_ms || 0);
      const gpuEl = $("#perf-gpu");
      if (gpuEl) { gpuEl.textContent = gpu.toFixed(0) + "%"; gpuEl.classList.toggle("hot", gpu > 60); }
      if ($("#perf-ior")) $("#perf-ior").textContent = perfRate(dt > 0 ? (s.io_read - _perfPrev.io_read) / dt : 0);
      if ($("#perf-iow")) $("#perf-iow").textContent = perfRate(dt > 0 ? (s.io_write - _perfPrev.io_write) / dt : 0);
      const wait = pct(s.blkio_ms || 0, _perfPrev.blkio_ms || 0);
      const waitEl = $("#perf-iowait");
      if (waitEl) { waitEl.textContent = wait.toFixed(0) + "%"; waitEl.classList.toggle("hot", wait > 20); }
    }
    const q = s.io_inflight || 0;
    const qEl = $("#perf-ioq");
    if (qEl) { qEl.textContent = String(q); qEl.classList.toggle("hot", q >= 4); }
    if ($("#perf-ram")) $("#perf-ram").textContent = (s.rss_kb / 1024).toFixed(0) + " MB";
    _perfPrev = { t: now, cpu_ms: s.cpu_ms, render_ms: s.render_ms || 0, io_read: s.io_read, io_write: s.io_write, blkio_ms: s.blkio_ms || 0 };
  };
  tick();
  _perfTimer = setInterval(tick, 1000);
}
function stopPerfOverlay() {
  if (_perfTimer) { clearInterval(_perfTimer); _perfTimer = null; }
  document.getElementById("perf-hud")?.remove();
}
function togglePerf() {
  const on = localStorage.getItem("isy_perf") === "1";
  if (on) { localStorage.removeItem("isy_perf"); stopPerfOverlay(); }
  else { localStorage.setItem("isy_perf", "1"); startPerfOverlay(); }
  return !on;
}
window.togglePerf = togglePerf;

async function init() {
  document.body.append(el("div", { id: "toasts", class: "toasts" }));
  if (localStorage.getItem("isy_perf") === "1") startPerfOverlay();
  paintBackdrop();
  window.addEventListener("resize", () => { clearTimeout(_bdT); _bdT = setTimeout(paintBackdrop, 200); });
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
  subscribeEvents();
  setupSwipe();
  // Register this device's push token. The native FCM token is fetched async, so
  // retry a few times before giving up (no-op in a plain browser / when disabled).
  let pushRegistered = false;
  registerPushToken().then((ok) => { pushRegistered = !!ok; });
  let _pushTries = 0;
  const _pushTimer = setInterval(async () => {
    if (++_pushTries > 5 || pushRegistered) {
      clearInterval(_pushTimer);
      return;
    }
    pushRegistered = await registerPushToken();
    if (pushRegistered) clearInterval(_pushTimer);
  }, 2000);
}
init();
