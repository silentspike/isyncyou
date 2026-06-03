#!/usr/bin/env python3
# Phase -1 spike (#35): OneDrive Graph capability probe against the throwaway
# test account testuser@example.com, using the existing /backup dev token
# caches (~/.config/m365-{read,write}/). Throwaway research artifact, NOT part
# of the iSyncYou build. No secrets (client IDs are public app registrations).

import msal, json, urllib.request, urllib.error
from pathlib import Path
AUTH="https://login.microsoftonline.com/consumers"
def tok(app_type, cid, scopes):
    cp=Path.home()/(".config/m365-%s/token_cache.json"%app_type)
    c=msal.SerializableTokenCache(); c.deserialize(cp.read_text())
    app=msal.PublicClientApplication(cid,authority=AUTH,token_cache=c)
    a=[x for x in app.get_accounts() if x.get("username")=="testuser@example.com"][0]
    r=app.acquire_token_silent(scopes,account=a); return r["access_token"]
RT=tok("read","cee80dd9-c13e-4dbb-9d4c-73eb4987d447",["Files.Read"])
WT=tok("write","a90d9140-3a62-46d0-907b-f2b7b61a573a",["Files.ReadWrite"])
def req(method,url,token,data=None,headers=None,raw=False):
    h={"Authorization":"Bearer "+token}
    if headers: h.update(headers)
    if data is not None and not raw and not isinstance(data,(bytes,bytearray)):
        data=json.dumps(data).encode(); h["Content-Type"]="application/json"
    r=urllib.request.Request(url,data=data,headers=h,method=method)
    try:
        resp=urllib.request.urlopen(r,timeout=30)
        body=resp.read()
        try: return resp.status, json.loads(body)
        except: return resp.status, body
    except urllib.error.HTTPError as e:
        return e.code, e.read()[:300].decode(errors="ignore")
G="https://graph.microsoft.com/v1.0"

print("### 1) DRIVE")
s,d=req("GET",G+"/me/drive?$select=driveType,quota",RT); print("  /me/drive:",s, (d.get('driveType') if isinstance(d,dict) else d))

print("### 2) INITIAL DELTA")
s,d=req("GET",G+"/me/drive/root/delta",RT)
cnt=len(d.get("value",[])) if isinstance(d,dict) else "?"
dl=d.get("@odata.deltaLink") if isinstance(d,dict) else None
print("  delta:",s,"items=",cnt,"deltaLink?",bool(dl))

print("### 3) SIMPLE UPLOAD + fileSystemInfo mtime")
content=b"isyncyou spike file\n"
s,d=req("PUT",G+"/me/drive/root:/iSyncYou-spike/test1.txt:/content",WT,data=content,raw=True,
        headers={"Content-Type":"text/plain"})
fid=d.get("id") if isinstance(d,dict) else None
print("  upload:",s,"id?",bool(fid),"eTag",(d.get('eTag') if isinstance(d,dict) else d))
if isinstance(d,dict): print("    hashes:",json.dumps(d.get("file",{}).get("hashes",{})))
mt="2021-06-15T10:00:00Z"
s,d=req("PATCH",G+"/me/drive/items/%s"%fid,WT,data={"fileSystemInfo":{"lastModifiedDateTime":mt}})
got=d.get("fileSystemInfo",{}).get("lastModifiedDateTime") if isinstance(d,dict) else d
print("  set mtime -> got:",s,got,"=> preserved:", got and got.startswith("2021-06-15"))
etag=d.get("eTag") if isinstance(d,dict) else None

print("### 4) UPLOAD SESSION (chunked + nextExpectedRanges)")
s,d=req("POST",G+"/me/drive/root:/iSyncYou-spike/big.bin:/createUploadSession",WT,
        data={"item":{"@microsoft.graph.conflictBehavior":"replace"}})
up=d.get("uploadUrl") if isinstance(d,dict) else None
print("  createUploadSession:",s,"uploadUrl?",bool(up))
if up:
    total=655360; chunk=327680  # 2 x 320KiB
    blob=b"\0"*total
    # chunk 1 (NO auth header on uploadUrl)
    r1=urllib.request.Request(up,data=blob[:chunk],method="PUT",
        headers={"Content-Length":str(chunk),"Content-Range":"bytes 0-%d/%d"%(chunk-1,total)})
    try:
        resp=urllib.request.urlopen(r1,timeout=30); st1=resp.status; b1=json.loads(resp.read())
    except urllib.error.HTTPError as e: st1=e.code; b1=e.read()[:200].decode(errors="ignore")
    print("  chunk1:",st1,"nextExpectedRanges:",(b1.get('nextExpectedRanges') if isinstance(b1,dict) else b1))
    # chunk 2 (final)
    r2=urllib.request.Request(up,data=blob[chunk:],method="PUT",
        headers={"Content-Length":str(total-chunk),"Content-Range":"bytes %d-%d/%d"%(chunk,total-1,total)})
    try:
        resp=urllib.request.urlopen(r2,timeout=30); st2=resp.status; b2=json.loads(resp.read())
    except urllib.error.HTTPError as e: st2=e.code; b2=e.read()[:200].decode(errors="ignore")
    print("  chunk2(final):",st2,"id?",(bool(b2.get('id')) if isinstance(b2,dict) else b2))

print("### 5) ETAG CONFLICT (If-Match stale -> expect 412)")
# change file to bump eTag
req("PUT",G+"/me/drive/root:/iSyncYou-spike/test1.txt:/content",WT,data=b"changed\n",raw=True,headers={"Content-Type":"text/plain"})
s,d=req("PATCH",G+"/me/drive/items/%s"%fid,WT,data={"name":"test1-renamed.txt"},headers={"If-Match":etag or '"stale"'})
print("  PATCH with stale If-Match:",s,"=> 412 expected:", s==412)

print("### 6) INCREMENTAL DELTA (deltaLink picks up new items)")
if dl:
    s,d=req("GET",dl,RT); newc=len(d.get("value",[])) if isinstance(d,dict) else "?"
    print("  incremental delta:",s,"new items=",newc)
print("\nDONE")
