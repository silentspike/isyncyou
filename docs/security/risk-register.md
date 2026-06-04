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
| **Mitigation** | (a) Cloud-mutating restore is **off by default** (`restore.cloud_restore_enabled = false`), enforced at the engine entry point before any store or network access. (b) The crash-safe design — operation ledger, explicit state machine, content-derived `HMAC-SHA256` idempotency key, reconcile-on-recovery, crash-injection matrix — is **implemented and proven in isolation** (against a non-idempotent fake cloud); **wiring it into the live `restore_cloud` path and invoking recovery on daemon boot is still in progress**, which is why (a) stays in force. (c) Local-file restore is unaffected and always available. |
| **Status** | **In progress** — the off-by-default gate is mitigated and tested today; the ledger + crash matrix that would let the gate be turned on safely are being built. Design: [ADR-001](../adr/001-restore-semantics.md); overview: `SHOWCASE.md` §1. |

## R2 — Data at rest is unencrypted

| | |
|---|---|
| **Risk** | The SQLite store (metadata, mail-body index) and cached OAuth tokens live in plaintext on disk, protected only by file permissions. |
| **Impact** | Medium–High — a local attacker or a backup of the home directory exposes tokens and indexed content. |
| **Mitigation** | Tokens are kept out of logs; the storage layer is designed as a pluggable backend so an `encrypted` backend can replace `plain` without touching callers. Documented in the README and SECURITY.md so no user is misled. |
| **Status** | **Accepted (temporary)** — explicitly disclosed; do not point iSyncYou at sensitive data on a shared machine yet. At-rest encryption is queued, not shipped. |

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
| **Mitigation** | The mail viewer sanitizes HTML, runs **no JavaScript**, blocks external resource loads (tracking pixels), maps `cid:` references locally, and never auto-opens links or attachments. |
| **Status** | **Mitigated** — see `docs/html-viewer-security.md`. |

## R6 — Local API exposure

| | |
|---|---|
| **Risk** | The daemon serves a local API/web UI; if reachable beyond the intended boundary it could allow unauthorized control or destructive actions. |
| **Impact** | Medium–High. |
| **Mitigation** | Unix-socket by default (file-permission scoped); HTTP is opt-in only and, when enabled, uses TLS + capability tokens + CSRF protection with no destructive `GET`s; remote access requires pairing/mTLS. |
| **Status** | **Mitigated** by design — see `docs/local-api-security.md`. |

## R7 — Supply chain (dependencies)

| | |
|---|---|
| **Risk** | A compromised or vulnerable crate could ship in a release. |
| **Impact** | Medium–High. |
| **Mitigation** | `cargo deny` runs in the gate (advisories + licenses + bans); Dependabot tracks updates. Reproducible build-once-promote with SBOM and signed artifacts is being stood up. |
| **Status** | **In progress** — `cargo deny` + Dependabot are active; SBOM/signing are queued. |

---

## How this register is maintained

A risk is added the moment it is understood, with an honest status — not after it is
fixed. When a mitigation lands with test evidence, the status moves to **mitigated**
and the relevant test or doc is linked. Nothing here is marked mitigated on the
strength of a code reading alone.
