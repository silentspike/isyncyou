# Risk Register

A living, honest list of the risks this project carries, what is done about each,
and its current status. It is the inverse of a marketing page: it states where the
sharp edges are. "Status" is one of **mitigated**, **in progress**, or **accepted**
(a deliberate, documented trade-off).

This register is referenced from [`SECURITY.md`](../../SECURITY.md) and from the
README's [Known limitations](../../README.md#known-limitations).

---

## R1 — Cloud restore could duplicate a user's mailbox item

| | |
|---|---|
| **Risk** | Re-creating an archived item in the cloud is a Graph `POST` followed by a local record. The two are not atomic; a crash, network drop, or token expiry between them can make a naive retry `POST` again and create a **duplicate** in the user's real mailbox. |
| **Impact** | High — silent, user-visible data pollution that is tedious to undo. |
| **Mitigation** | (a) Cloud-mutating restore is **off by default** (`restore.cloud_restore_enabled = false`), enforced at the engine entry point before any store or network access. (b) The crash-safe design — operation ledger, explicit state machine, content-derived `HMAC-SHA256` idempotency key, reconcile-on-recovery, crash-injection matrix — is implemented and, for **mail**, now **wired into the live `restore_cloud` path with recovery on daemon boot**; it is proven against a non-idempotent fake cloud. The other services are **refused** (no direct, non-crash-safe POST) until each is ledger-migrated (tracked in `docs/requirements/restore.yml`). (c) Local-file restore is unaffected and always available. |
| **Status** | **Mitigated for mail** — gate + ledger + daemon recovery in place and proven in isolation; the marker probe is **live-confirmed** against the test account (`tools/live_restore_probe.py`: Graph preserves a posted `Message-ID` and the `internetMessageId` `$filter` finds it). Cloud restore stays **off by default** as a deliberate opt-in for a destructive operation. Other services' ledger migration is tracked in `docs/requirements/restore.yml`. Design: [ADR-001](../adr/001-restore-semantics.md). |

## R2 — Data at rest can fall back to plaintext

| | |
|---|---|
| **Risk** | The SQLite store (metadata, mail-body index) may live in plaintext on disk when the operator does not configure SQLCipher. (OAuth tokens are now always encrypted at rest — see Mitigation.) |
| **Impact** | Medium — a local attacker or a home-directory backup exposes indexed content; cached tokens are no longer plaintext. |
| **Mitigation** | Tokens are kept out of logs; `isyncyou login --keyring` stores token JSON in the desktop Secret Service / KDE Wallet compatible keyring and leaves only a non-secret marker file on disk; file-cache writes are owner-only on Unix (`0600`) and AES-256-GCM encrypted with a secret supplied via `ISYNCYOU_TOKEN_CACHE_KEY_FILE`, systemd credential `isyncyou-token-cache-key`, or `ISYNCYOU_TOKEN_CACHE_KEY`. **When none of those and no keyring are configured, the token cache is still encrypted at rest with an auto-generated, owner-only local key kept beside it — it is never written in the clear.** That local key guards the cache file against being copied, synced or logged on its own; it does not protect against an attacker with read access to the whole config directory (use the keyring or an out-of-band secret for that). SQLite stores are SQLCipher-encrypted when a key is supplied via `ISYNCYOU_STORE_KEY_FILE`, systemd credential `isyncyou-store-key`, or `ISYNCYOU_STORE_KEY`; encrypted stores and their `VACUUM INTO` snapshots fail closed without the correct key. **Existing plaintext stores migrate in place with `isyncyou migrate --account <id> --encrypt-store`** — atomic (temp + rename + fsync; a crash mid-migration leaves the plaintext store fully usable), preserves all rows + rebuilds the FTS indexes, and refuses to run without a configured key (no silent plaintext copy). Documented in the README and SECURITY.md so no user is misled. |
| **Status** | **Mitigated** — tokens are never written in the clear (encrypted-at-rest by default, keyring/credential options on top), store encryption is available with fail-closed semantics, and a guided in-place migration closes the legacy-plaintext-store gap. Residual (accepted, documented): an operator who never configures a store key keeps a plaintext **store** (`isyncyou-doctor` warns); the auto local token key does not defend against full config-directory read access — the keyring or an out-of-band secret does. |

## R3 — Mass deletion from a desynced cursor

| | |
|---|---|
| **Risk** | A bad delta cursor, a `410 Gone`, or a reconcile bug could be interpreted as "everything was deleted" and propagate mass deletions in either direction. |
| **Impact** | High — potential large-scale data loss. |
| **Mitigation** | A two-direction mass-delete guard halts a run that would delete/replace more than a configurable threshold; a `410` triggers reconciliation (diff + apply differences), never a blind delete; conflicts default to keep-both with `If-Match`/ETag preconditions. |
| **Status** | **Mitigated** — guard and `410` reconciliation are implemented and exercised by the acceptance/chaos harness. |

## R4 — Token theft / over-broad scope

| | |
|---|---|
| **Risk** | The tool holds Microsoft 365 access/refresh tokens; an over-broad scope or a leaked token widens the blast radius. |
| **Impact** | High. |
| **Mitigation** | Separate Reader and Writer app registrations use least-privilege scopes and independent encrypted caches. Product UI connects them explicitly per account, verifies the same stable Graph object identity before pairing them, never substitutes Writer for Reader cache refresh, and never logs tokens. Public-client OAuth uses PKCE in the desktop/mobile product flow and retains device-code only for the explicit headless CLI workflow; neither path uses a client secret. |
| **Status** | **Mitigated** for handling; at-rest protection tracked under R2. |

## R5 — Malicious mail content in the viewer

| | |
|---|---|
| **Risk** | Archived mail can contain hostile HTML, tracking pixels, and active content; rendering it could leak that the message was opened or execute script. |
| **Impact** | Medium. |
| **Mitigation** | The mail viewer sanitizes HTML, runs **no JavaScript**, blocks external resource loads (tracking pixels), maps `cid:` references locally, and never auto-opens links or attachments. External `http(s)` links are rewritten to a CSP-locked local confirmation page before the browser can leave the viewer. |
| **Status** | **Mitigated** — see `docs/html-viewer-security.md`. |

## R6 — Local API exposure

| | |
|---|---|
| **Risk** | The daemon serves a local API/web UI; if reachable beyond the intended boundary it could allow unauthorized control or destructive actions. |
| **Impact** | Medium–High. |
| **Mitigation** | TCP binds are loopback-only at runtime (`serve()` rejects `0.0.0.0`, `[::]`, LAN addresses, and arbitrary hostnames before opening a listener); the optional Unix-domain socket is owner-only (`0600`); destructive operations are `POST` only and require action-scoped `X-Capability-Token` values (restore and scheduled-sync control are separate); restore POSTs append durable per-account audit entries before and after the handler runs; the TCP adapter rejects non-loopback `Host` and non-local `Origin` before routing. **Remote access is local-only by design**: no remote listener exists or is planned — headless operators tunnel via SSH (`ssh -L 8765:127.0.0.1:8765 host`) or a self-hosted VPN, which is better-audited than any home-grown mTLS/pairing stack would be. |
| **Status** | **Mitigated / accepted by design** — fully mitigated for the local desktop posture; the absence of a remote surface is a deliberate architecture decision (story S-P3.1 closed as not-planned, 2026-06-11), not an open gap. "No open port" is the strongest available posture; building remote auth would create the attack surface it then must defend. See `docs/local-api-security.md`. |

## R7 — Supply chain (dependencies)

| | |
|---|---|
| **Risk** | A compromised or vulnerable crate could ship in a release. |
| **Impact** | Medium–High. |
| **Mitigation** | `cargo deny` runs in the gate (advisories + licenses + bans); Dependabot tracks updates. The release workflow generates a CycloneDX SBOM from the locked Cargo graph and requests GitHub artifact attestations for the release archives, AppImage, Windows zip, SBOM, and checksum file. |
| **Status** | **In progress** — `cargo deny`, Dependabot, SBOM generation, and signed GitHub artifact attestations are wired; deployed staging/full live-E2E evidence is still open. |

## R8 — Claude/Codex OAuth provider compatibility and local fallback risk

| | |
|---|---|
| **Risk** | The in-app agent uses app-authorized Claude/Codex OAuth and provider-compatible request envelopes. Required auth, billing, subscription identity, stream, and usage fields can drift. The default-off #627 local CLI fallback/capture path can leak credentials or ship accidentally, and an incorrect harness transformation can retain default-client behavior or remove a required provider field. |
| **Impact** | Medium — wire drift can break provider access; credential or account leakage is security-sensitive; an incorrect retained/removed boundary can violate the product harness contract. |
| **Mitigation** | #623 product builds load app OAuth credentials from the encrypted iSyncYou CredentialStore. The owner-approved compliance model begins with official provider OAuth, preserves the required auth/billing/subscription identity envelope unchanged and in its provider-required position, removes every other default-client prompt/tool/skill/plugin/MCP/rule/memory/history/context component, and installs only the iSyncYou M365 prompt and single `isyncyou` tool. Exact-position/absence tests freeze that boundary. Product sources ignore local CLI auth and reject OAuth endpoint/client/scope overrides. #627's desktop-only fallback is explicit, default-off, never satisfies product readiness, never persists local credentials, and is absent from mobile/release feature graphs and built artifacts. Its allowlist reducer commits only bounded summaries; raw captures are deleted. See [the experimental boundary](../experimental/agent-local-cli-fallback.md) and [#627 evidence](../evidence/issue-627-manifest.json). |
| **Status** | **Accepted / monitored** — #623 product OAuth and StoreArchive evidence is complete. Residual technical risk remains for provider wire drift, credential leakage, accidental experimental-feature shipping, and defects in the retained-versus-removed harness boundary. The #627 version/capture process monitors those risks without treating local CLI auth as product evidence. Design: [ADR-007](../adr/007-agent-architecture.md). |

## R9 — Android network-critical Agent flow interruption

| | |
|---|---|
| **Risk** | Android backgrounds the app during provider OAuth or an active streamed turn; connectivity failure is misdiagnosed or leaks transport details; refresh failure silently changes credential origin. |
| **Impact** | High — sign-in or turns can fail silently, and unsafe diagnostics or fallback can expose provider/network data or misrepresent authentication state. |
| **Mitigation** | #640 uses reason-bound, acknowledged, bounded `dataSync` foreground leases; a session/capability-gated provider-purpose preflight emits only closed codes; mobile snapshots are single-use and bound to the active engine session and guard; status is network-free; explicit refresh atomically persists only complete credentials and fails closed. Codex callbacks are loopback-only, cancellable, one-shot, and leave no callback diagnostics or fixed-routing residue. Default artifacts exclude the separate diagnostic hook. |
| **Status** | **Mitigated / monitored** — host, UI, artifact, official OAuth, guarded refresh, and streamed-turn evidence is recorded in [the #640 manifest](../evidence/issue-640-manifest.json). Residual Android scheduling, provider, and network-policy drift remains monitored; a foreground service mitigates priority loss but does not guarantee reachability. Design: [ADR-007](../adr/007-agent-architecture.md). |

---

## R10 — First-run OAuth handoff / product-readiness spoofing

| | |
|---|---|
| **Risk** | A turn runs on a credential that never completed the official OAuth → custom-harness handoff (mere credential presence, a stale/forged onboarding journal, an experimental local-CLI credential, a policy/contract-mismatched or half-written credential), or a default-client harness component reaches the wire, or the pasted manual code leaks via URL/DOM/log. |
| **Impact** | High — an unverified or spoofed provider identity could act on the user's Microsoft 365 domain, or full-power default-client capabilities could be exercised under the product path, or a subscription credential could leak. |
| **Mitigation** | #639 makes readiness a durable, authenticated `ProductActivationV1` (generation + official policy fingerprint + harness contract) plus a valid Active V2 bundle plus a passing static harness attestation; the TTL'd onboarding journal is evidence/recovery only and never the authority. Every provider round re-attests the actually-sent request against a positive allowlist; the transport accepts only an attested request. One product-runtime gate spans selection + readiness + build before any turn-id/stream/archive and returns a closed 409 with no cross-provider fallback; an experimental local-CLI credential never confers readiness and FakeProvider is never an unconfigured product turn. Crash windows are defined and recovered without re-exchange; interrupted attempts are `error_redacted` and never resumed; refresh is a V2-lifecycle event only. Status carries `no-store` and no secrets; `/oauth/complete` is strict-JSON, attempt-state-bound, Claude-only; the pasted code is transient (type=password, cleared, never URL/DOM-attr/log/storage). |
| **Status** | **Mitigated / monitored** — host, UI, boundary, default-artifact, and live official Claude + Codex OAuth handoff evidence is recorded in [the #639 manifest](../evidence/issue-639-manifest.json), and REQ-AGENT-014 is implemented. Residual provider wire drift and platform scheduling remain monitored. Design: [ADR-007](../adr/007-agent-architecture.md). |

---

## R11 — Provider account lifecycle grant loss or orphaning

| | |
|---|---|
| **Risk** | Disconnect deletes only local state while the provider grant remains live; Reconnect overwrites an older grant; a crash loses revocation authority or a newly exchanged candidate; or Switch accepts an unverified/same identity. |
| **Impact** | High — an apparently disconnected account can remain authorized, credentials can become unrecoverable, duplicate grants can survive, or the UI can misrepresent which account is active. |
| **Mitigation** | #645 uses provider-scoped operation leases, a durable cross-process fence, stable installation-bound idempotency, encrypted lifecycle journals/candidates, separate active/candidate revoke legs, and fail-closed recovery. Provider revoke success is persisted before provider-scoped local cleanup. Ambiguous outcomes and grant-bearing candidates are retained non-ready. Reconnect revokes the prior generation before OAuth; Codex Switch requires a strictly validated signed subject and Claude Switch remains unavailable without one. Android revoke legs use the bounded #640 foreground-guard/snapshot contract; default artifacts exclude lifecycle hooks. |
| **Status** | **Mitigated / monitored** — host, UI, Android contract, hook-isolation, real Claude/Codex Disconnect/Reconnect, post-Reconnect turns, same-account rejection, and a physical two-account Codex Switch are recorded in [the #645 manifest](../evidence/issue-645-manifest.json). AC-3 is closed and REQ-AGENT-015 is implemented. Residual provider revoke-scope and identity-contract drift remains monitored. Design: [ADR-007](../adr/007-agent-architecture.md). |

---

## R12 — Shared Agent session replay and cross-device authority

| | |
|---|---|
| **Risk** | A transport retry duplicates a provider call or cloud effect, a stale session lease publishes after takeover, a request resumes under a different provider generation, or a one-time pairing transfer is reused. |
| **Impact** | High — duplicate mutation, transcript fork, authority confusion, or session disclosure across devices. |
| **Mitigation** | #628 adds route/session/payload-bound durable request IDs, bounded encrypted provider-step journals, provider-generation and harness binding, renewed server-time session leases, staged immutable objects with fenced manifest publication, host-owned cancellation/terminal ordering, and confirmation-gated one-time Pairing V2. Strict JSON and sealed mutation chunks remove mutable secrets and large bodies from URLs. |
| **Status** | **Mitigated / pre-RC verified; release pending** — REQ-AGENT-016 is implemented and the exact implementation commit, host gates, default APK, safe UI, live product-provider observations, and bounded cross-device recovery evidence are recorded in [the #628 pre-RC manifest](../evidence/issue-628-pre-rc-manifest.json). Protected review, promotion, RC publication, final-RC artifact verification, and explicit issue closure remain outstanding release controls. Design: [ADR-007](../adr/007-agent-architecture.md). |

---

## How this register is maintained

A risk is added the moment it is understood, with an honest status — not after it is
fixed. When a mitigation lands with test evidence, the status moves to **mitigated**
and the relevant test or doc is linked. Nothing here is marked mitigated on the
strength of a code reading alone.
