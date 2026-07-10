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
| **Mitigation** | Separate read and write/restore app registrations with least-privilege scopes; the write/restore scope is only requested when a restore is actually performed; tokens are never logged; public-client OAuth (PKCE / device-code) with no client secret on the desktop/CLI. |
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
| **Risk** | The in-app agent (Epic #614) ships a Claude/Codex app-OAuth provider path whose runtime is compatible with the installed Claude Code / Codex clients. Provider wire shape can drift, and the default-off #627 local CLI fallback/capture path must not be confused with product auth. A per-app consent dialog and encrypted token storage reduce local risk but do not make any provider policy question disappear. |
| **Impact** | Medium — brittle against provider header/wire drift and sensitive in audits if product auth, local CLI fallback, and BYO-key stories are blurred. |
| **Mitigation** | #623 product builds use app-authorized Claude/Codex OAuth credentials loaded from the encrypted agent CredentialStore, not BYO Anthropic/OpenAI API keys and not `~/.claude` / `~/.codex` local CLI auth. Non-live tests isolate HOME/CODEX_HOME/CLAUDE_CONFIG_DIR, clear provider env vars, and fail unexpected provider network POSTs. `FakeProvider` remains CI/unconfigured fallback only. The local CLI fallback and private wire-drift captures stay default-off under #627 `agent-subscription-experimental`; committed evidence must be redacted and contain no tokens, refresh tokens, auth files, account IDs, or reusable credential material. |
| **Status** | **Accepted / in progress** — accepted as a deliberate compatibility trade-off with strict product-vs-#627 boundaries. #623 cannot close on FakeProvider or local CLI auth; it still needs live product OAuth StoreArchive roundtrip evidence or an honest blocked state. Design: [ADR-007](../adr/007-agent-architecture.md). |

---

## How this register is maintained

A risk is added the moment it is understood, with an honest status — not after it is
fixed. When a mitigation lands with test evidence, the status moves to **mitigated**
and the relevant test or doc is linked. Nothing here is marked mitigated on the
strength of a code reading alone.
