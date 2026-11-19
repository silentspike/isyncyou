import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";

const source = fs.readFileSync(new URL("../gui/webui/src/app.js", import.meta.url), "utf8");
const start = source.indexOf("function renderNoteReader(it) {");
const end = source.indexOf("// #568: live OneNote write", start);
assert.ok(start >= 0 && end > start);
class Node {
  constructor(tag, props = {}, children = []) { this.tag = tag; Object.assign(this, props); this.children = children.flat().filter(Boolean); this.style = {}; }
  append(...items) { this.children.push(...items.filter(Boolean)); }
  addEventListener() {}
}
const box = new Node("div");
const context = vm.createContext({
  $: () => box, clear: node => { node.children = []; },
  el: (tag, props, ...children) => new Node(tag, props, children),
  icon: () => new Node("icon"), coverageBadge: () => new Node("badge"),
  App: {account:"fixture"}, CAP: {}, qs: () => "fixture", fmtDate: () => "date",
});
vm.runInContext(source.slice(start, end) + "\nthis.render = renderNoteReader;", context);
for (const [state, expected] of [["partial", "Partial archive"], ["unverified", "unverified"], [undefined, "unverified"], ["complete", null]]) {
  context.render({remote_id:"page", has_body:true, preview:{archive_state:state}});
  const notice = box.children.find(child => child.class === "note-archive-notice");
  if (expected) assert.ok(notice?.text.includes(expected), `missing notice for ${state}`);
  else assert.equal(notice, undefined);
}
context.render({remote_id:"page", has_body:false, preview:{archive_state:"unverified"}});
assert.equal(box.children.find(child => child.class === "note-archive-notice"), undefined);
context.render({remote_id:"page", has_body:false, preview:{archive_state:"partial"}});
assert.ok(box.children.find(child => child.class === "note-archive-notice")?.text.includes("page content"));
console.log("OneNote archive state: partial, unverified, missing, complete, no body passed");
