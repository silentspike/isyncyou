import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
const source=readFileSync(new URL('../gui/webui/src/app.js',import.meta.url),'utf8');
const section=(a,b)=>{const start=source.indexOf(a),end=source.indexOf(b,start);assert.ok(start>=0&&end>start);return source.slice(start,end)};
class Node {
 constructor(tag,props={},...children){this.tag=tag;Object.assign(this,props);this.children=[];this.isConnected=true;this.append(...children)}
 append(...children){this.children.push(...children.flat().filter(x=>x!=null));return this}
 contains(x){return this===x||this.children.some(c=>c instanceof Node&&c.contains(x))}
}
const view=new Node('div',{id:'view'});
const find=(node,id)=>node.id===id?node:node.children.filter(c=>c instanceof Node).map(c=>find(c,id)).find(Boolean);
const text=node=>node instanceof Node?[node.text||'',...node.children.map(text)].join(' '):String(node);
const pending=[];
const defer=(path)=>new Promise((resolve,reject)=>pending.push({path,resolve,reject}));
const take=path=>{const i=pending.findIndex(p=>p.path.startsWith(path));assert.notEqual(i,-1,`missing ${path}`);return pending.splice(i,1)[0]};
const App={account:'a',accounts:[{id:'a',provider:'self_hosted'}],route:'overview',counts:{}};
const api=path=>defer(path);
const ctx=vm.createContext({App,api,qs:o=>new URLSearchParams(o).toString(),Date,Number,Array,Map,Set,URLSearchParams,
 providerAccountsRefreshing:0,clearProviderMailReads(){},MailSnapshot:{epoch:0,entries:new Map(),inflight:new Map()},clearMailSnapshots(){},accountRoleStatus:()=>({state:'connected'}),
 $:selector=>selector==='#view'?view:find(view,selector.slice(1)),el:(tag,props,...children)=>new Node(tag,props,...children),clear:n=>{n.children=[];return n},
 MOBILE:false,CAP:{},icon:()=>null,updateNavCounts(){},activityChart:()=>null,fmtFullDate:()=>'',fmtDate:()=>'',accountDisplayLabel:()=>'',connItem:()=>null,
 overviewSignature:()=>'',overviewLiveUpdate(){},syncCmd(){},go(){}});
vm.runInContext(section('const SVC_COLOR =', 'function overviewSignature(')+section('async function reloadAccounts()', 'function renderAccountMenu')+'\nthis.render=renderOverview;this.reload=reloadAccounts;this.snapshot=cachedOverviewSnapshot;',ctx);
const tick=()=>new Promise(r=>setImmediate(r));
const overviewResponses=(items)=>{
 take('/api/v1/status').resolve({totals:{items},services:[{service:'mail',items,archived:0}]});
 take('/api/v1/activity').resolve({runs:[]});take('/api/v1/settings').resolve({});take('/api/v1/sync/state').resolve({});
};
// A render already in flight is invalidated by overlapping account reloads.
const initial=ctx.render(view);const first=ctx.reload();const second=ctx.reload();
const laterIndex=pending.map(x=>x.path).lastIndexOf('/api/v1/accounts');
assert.ok(laterIndex>=0); pending.splice(laterIndex,1)[0].resolve({accounts:[{id:'a',provider:'self_hosted'}]});
await tick(); assert.equal(App.account,'a');
// A second overview started while reloads are pending neither fetches nor caches.
await ctx.render(view);assert.match(text(view),/Refreshing account/);assert.equal(pending.filter(x=>x.path.startsWith('/api/v1/status')).length,1);
overviewResponses(99);await initial;assert.equal(ctx.snapshot('a'),null);assert.match(text(view),/Refreshing account/);
take('/api/v1/accounts').resolve({accounts:[{id:'obsolete',provider:'self_hosted'}]});
await Promise.all([first,second]);await tick();
assert.equal(App.account,'a','late obsolete account response must be ignored');
assert.equal(pending.filter(x=>x.path.startsWith('/api/v1/status')).length,1,'exactly one replacement render');
overviewResponses(7);await tick();
assert.match(text(view),/Archive healthy/);assert.equal(ctx.snapshot('a').items,7);
// An inactive route is never rebuilt on reload completion.
const last=ctx.reload();App.route='mail';take('/api/v1/accounts').resolve({accounts:[{id:'a',provider:'self_hosted'}]});await last;
assert.equal(pending.length,0,'no overview request on inactive route');
console.log('Overview reload races: pending gating, overlapping reloads, fresh DOM, snapshot isolation and inactive route passed');
