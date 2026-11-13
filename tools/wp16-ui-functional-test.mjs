import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";

const source = fs.readFileSync(new URL("../gui/webui/src/app.js", import.meta.url), "utf8");
const part = (start, end) => source.slice(source.indexOf(start), source.indexOf(end, source.indexOf(start)));
class Element {
  constructor(tag, props = {}, children = []) {
    this.tag = tag; this.children = []; this.handlers = {}; this.isConnected = true;
    Object.assign(this, props); this.value ||= ""; this.style = {};
    this.append(...children);
  }
  append(...children) { this.children.push(...children.flat().filter(Boolean)); }
  addEventListener(name, fn) { this.handlers[name] = fn; }
  fire(name) { this.handlers[name]?.({ currentTarget: this }); }
  get childNodes() { return this.children; }
}
const walk = root => [root, ...root.children.flatMap(x => typeof x === "object" ? walk(x) : [])];
const el = (tag, props, ...children) => new Element(tag, props || {}, children);
const clear = e => { e.children = []; return e; };
const App = { account:"a", accounts:[], liveUpdate:null };
const CAP = { agent:true };
const pending = [];
const api = path => {
  const query = new URL(path, "https://fixture.invalid").searchParams;
  if (!query.has("collection")) return Promise.resolve({ collections:["A", "B"] });
  return new Promise((resolve, reject) => pending.push({ collection:query.get("collection"), resolve, reject }));
};
const ctx = vm.createContext({ App, CAP, el, clear, api, qs:o => new URLSearchParams(o).toString(), Date, String, Number, Array, JSON, Set, Map,
  openSingleCaptureStatus(){}, openProviderCreate(){}, openProviderCollectionAccess(){}, openProviderRecovery(){}, toast(){},
  prepareSingleCapture(){}, openProviderTaskAttributes(){}, openProviderTaskParent(){}, openProviderCalendarText(){}, openProviderCalendarTime(){},
  openProviderContactName(){}, openProviderContactField(){}, prepareProviderContactRemove(){}, prepareProviderDelete(){}, chooseProviderContactPhoto(){},
  openProviderSeriesRule(){}, openProviderAlarm(){}, openProviderInvitationResponse(){}, prepareProviderCancellation(){},
  crypto:{randomUUID:()=>"id"}, PROVIDER_NATIVE_TIMEOUT_MS:1000, AssistantState:{pendingCardsById:new Map()},
  ensureAgentSession:async()=>"session", postJson:async()=>({}), openSheet(){}, renderAgentPendingCard(){},
});
vm.runInContext(part("function projectLiveMail(", "async function loadLiveMailFolder(") +
  part("async function renderProviderCalendarView(", "async function renderCalendarView(") +
  part("async function renderProviderContactsView(", "async function renderContactsView(") +
  "\nthis.calendar = renderProviderCalendarView; this.contacts = renderProviderContactsView; this.project = projectLiveMail;", ctx);
const tick = () => new Promise(resolve => setImmediate(resolve));
const row = (service, title) => service === "contacts"
  ? { href:"/item", collection:"A", names:[{values:[title]}] }
  : { href:"/item", view:{components:[{kind:service === "todo" ? "VTODO" : "VEVENT", uid:"u", properties:{SUMMARY:[{value:title}]}}]} };
for (const service of ["calendar", "todo", "contacts"]) {
  const view = el("view");
  await (service === "contacts" ? ctx.contacts(view) : ctx.calendar(view, service));
  const select = walk(view).find(x => x.tag === "select");
  const list = walk(view).find(x => x.class === "pick-list");
  const page = (collection, title, accessible = true) => ({account:"a", origin:"o", collection, accessible, cursor_digest:"v", items:[row(service,title)]});
  select.value = "A"; select.fire("change"); pending.shift().resolve(page("A", "Old")); await tick();
  assert.ok(walk(list).some(x => x.text === "Old"));
  select.value = "B"; select.fire("change"); const slowB = pending.shift();
  assert.equal(list.children.length, 0, `${service}: scope switch clears synchronously`);
  select.value = "A"; select.fire("change"); const laterA = pending.shift();
  slowB.resolve(page("B", "Late")); await tick(); assert.equal(list.children.length, 0);
  laterA.reject(Error("failed")); await tick(); assert.equal(list.children.length, 0);
  select.value = "B"; select.fire("change"); pending.shift().resolve(page("B", "Revoked", false)); await tick();
  assert.ok(walk(list).some(x => x.text === "Revoked"));
  assert.equal(walk(list).filter(x => x.tag === "button").length, 0, `${service}: revoked cache is read-only`);
}
const projected = ctx.project([{id:"m", mailboxIds:{a:true,b:true,c:false}, keywords:{$flagged:true}}], "");
assert.equal(projected[0].preview.flag, "flagged");
assert.deepEqual(Array.from(projected[0].mailbox_ids), ["a","b"]);
assert.equal(ctx.project([{id:"m"}], "")[0].preview.flag, "unknown");
assert.equal(ctx.project([{id:"m", keywords:{}}], "")[0].preview.flag, "notFlagged");
assert.equal(ctx.project([{id:"m", keywords:{$flagged:"bad"}}], "")[0].preview.flag, "unknown");
let sheet, action;
Object.assign(ctx, { Mail:{folders:[{remote_id:"a",name:"A"},{remote_id:"b",name:"B"},{remote_id:"c",name:"C"}]},
  svcFilter:()=>"all", openSheet:(title, content)=>{ sheet = {title,content}; }, closeSheet(){},
  prepareProviderMailAction:(_account,_id,value)=>{ action = value; } });
vm.runInContext(part("function openProviderMailMove(", "async function openProviderRelatedDraft(") + "\nthis.move = openProviderMailMove;", ctx);
ctx.move("a", projected[0]);
assert.equal(sheet.title, "Choose source folder");
walk(sheet.content).find(x => x.tag === "button" && x.children.includes("B")).onclick();
assert.equal(sheet.title, "Move message");
walk(sheet.content).find(x => x.tag === "button" && x.children.includes("C")).onclick({currentTarget:{}});
assert.equal(action.source_mailbox_id, "b");
assert.equal(action.destination_mailbox_id, "c");
console.log("WP16 UI functional checks passed: 3 services, scope races, failure, access loss, mail projection and membership selection");
