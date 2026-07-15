# Agent threat model

Scope: the in-app M365 agent (Epic #614) — the data assistant and operations agent that
run inside the desktop daemon and the Android engine. This document enumerates the agent's
attack surface and the mitigations that become release-blocker acceptance criteria in the
implementing stories. Design: [ADR-007](../adr/007-agent-architecture.md). Requirements:
[`agent.yml`](../requirements/agent.yml). Residual risks: [risk register](risk-register.md).

## Assets
- The user's M365 content (already archived locally and in OneDrive).
- The user's provider credentials (official API keys / OAuth tokens).
- The cross-device session history (may summarize M365 content).
- The ability to perform destructive cloud operations (restore-cloud, live-write, share).

## Trust boundaries
- **WebView ↔ loopback API.** The UI talks only to `127.0.0.1`; the LLM call is server-side
  (CSP `connect-src 'self'`, ADR-004). On Android, **any app on the device can reach
  `127.0.0.1`**, so every `/api/v1/agent/*` route is session-token gated (REQ-AND-012).
- **Agent loop ↔ provider.** Outbound HTTPS to the provider carries selected M365 content;
  this requires explicit one-time user consent at setup.
- **Agent ↔ tools.** The agent's only capability is the `isyncyou` tool over the M365
  domain — no shell/FS/OS/device/free-form-HTTP (REQ-AGENT-001).

## Threats and mitigations
| # | Threat | Mitigation |
|---|---|---|
| T1 | **Prompt injection** — retrieved mail/document content instructs the model to take a destructive action ("delete my inbox"). | Tool calls are taken only from the model's `tool_use` structure, never parsed from content; tool results are tagged `untrusted_content`; the system prompt forbids content from overriding policy; destructive actions require a human-confirmed one-time token (REQ-AGENT-005, REQ-AGENT-002/003). |
| T2 | **Model self-authorization** — the model obtains a capability token and authorizes its own destructive action. | The model never receives a capability/confirmation token; the server mints a one-time, action-bound, single-use token only after a human confirms via a session-authenticated request (REQ-AGENT-003/004). |
| T3 | **Scope escape** — the agent reaches the device/OS/filesystem beyond M365. | App-scope invariant: a single `isyncyou` tool, enforced by a tool-registry snapshot test; no other tool exists (REQ-AGENT-001). |
| T4 | **Loopback abuse on mobile** — a malicious local app calls the agent API. | Session-token gate on every `/api/v1/agent/*` route (REQ-AND-012); destructive mobile actions additionally require the per-action biometric-token gate before the Agent one-time token is consumed (#625), with physical native-prompt cancel/approval evidence from #626 (REQ-AND-016, S-AG.11). |
| T5 | **Session history disclosure** — plaintext M365 excerpts in OneDrive. | Per-turn files are AES-256-GCM encrypted with a pairing-derived key (Argon2id/HKDF), not the device-local key; only ciphertext is uploaded (REQ-AGENT-006). |
| T6 | **Credential theft** - provider API keys/tokens leak. | Credentials are stored through the typed encrypted CredentialStore, with owner-only Unix files and a separate Android Keystore-wrapped agent credential key installed before `nativeStart`; raw credentials and raw credential keys are never sent to the WebView, bridge, or API logs. The LLM call is server-side (S-AG.5, REQ-AGENT-010). |
| T7 | **Interrupted destructive job on mobile** — a backup/restore-cloud job is killed mid-flight and re-runs, duplicating cloud items. | Mobile backup/restore-cloud run as durable `mobile_jobs` with owner leases, WorkManager-only execution, restart recovery, dedupe keys, and late-cancel reconciliation. Restore-cloud also reuses the crash-safe restore ledger (`run_restore_op` / `recover_pending_restores`) so retries do not duplicate cloud items (REQ-AND-016, R1). |
| T8 | **Experimental provider-origin exposure** — local client credentials, account data, raw drift captures, or a reusable operational recipe leak; or the default-off desktop fallback ships in a product artifact. | Provider-compatible product implementation necessarily exists in source. The enforceable boundary is that product builds use encrypted app OAuth, local CLI reads exist only in the Linux experimental module, mobile cannot enable that feature, and default desktop/APK artifacts are scanned for fallback markers. The allowlist reducer publishes only bounded presence/structure summaries; raw captures, credentials, personal identifiers, full header sets, and standalone request recipes never enter docs/evidence. Local fallback cannot satisfy product readiness (R8, S-AG.12/#627). |
| T9 | **Confirmed-operation bypass** — the Agent executes backup/restore/share/live-write through a new raw mutation path, skips revalidation, or leaks body/share/credential material in API or audit output. | Confirmed operations revalidate the stored PendingAction before execution, reuse existing daemon/engine/writer/ledger paths, and centrally redact API/audit summaries. On mobile, #625 queues backup/restore-cloud jobs, keeps share on the ledger-backed handler, and limits live-write to the explicit metadata allowlist; unsupported mobile verbs fail closed. Tests cover controlled restore-local output, backup runner dispatch, restore-cloud ledger dispatch, live-write writer dispatch, share ledger dispatch, read-class confirm rejection, mobile gate ordering, and redacted confirm summaries (REQ-AGENT-012, REQ-AND-016). |
| T10 | **Network-critical flow loss or diagnostics leakage** — Android backgrounds OAuth/streaming work, a product credential refresh silently downgrades, or connectivity diagnostics disclose provider/network details. | #640 uses a bounded dataSync foreground guard for OAuth, explicit refresh, and active turns; probe/status results are closed redacted codes, and product credential errors remain reconnect-required rather than falling back to local credentials or FakeProvider. The default APK excludes diagnostic hooks. Host, artifact, and physical-device closeout evidence is recorded in the #640 manifest (REQ-AGENT-013). |
| T11 | **Product-readiness spoofing / harness-handoff bypass** — a turn runs on a credential that never completed the official OAuth → custom-harness handoff (bare credential presence, a forged/stale onboarding journal, an experimental local-CLI credential, or a policy/contract-mismatched or half-written credential), a default-client harness component reaches the wire, or the pasted manual code leaks. | #639 makes readiness the durable, authenticated `ProductActivationV1` (generation + official policy fingerprint + harness contract) plus a valid Active bundle plus a passing static harness attestation; the TTL'd journal is evidence/recovery only, never authority. Every provider round re-attests the actually-sent request against a positive allowlist and the transport accepts only an attested request. One product-runtime gate spans selection + readiness + build before any turn state and returns a closed 409 with no cross-provider fallback; experimental credentials and FakeProvider can never be a product turn. Crash windows recover without re-exchange; interrupted attempts are `error_redacted`, never resumed; refresh is a V2-lifecycle event only. Status is `no-store` and secret-free; `/oauth/complete` is strict-JSON, attempt-state-bound, Claude-only; the pasted code is transient and never persisted/logged/placed in a URL or HTML attribute (REQ-AGENT-014, S-AG.14/#639). |
| T12 | **Account lifecycle grant loss or orphaning** — local deletion leaves a provider grant live, Reconnect overwrites an older refresh authority, a crash loses a newly issued candidate, or an unverified identity is accepted as an account switch. | #645 uses provider-scoped shared/exclusive leases, a cross-process lifecycle fence, stable installation-bound idempotency, encrypted authority/journal/candidate records, and explicit revoke legs. Product readiness is false from the durable prepared state. Confirmed revoke is persisted before matching local cleanup; ambiguous outcomes and rejected candidates remain encrypted and non-ready until explicit recovery. Reconnect cleans the prior generation before OAuth, and Codex Switch requires a strictly validated signed subject while Claude Switch remains unavailable. Strict JSON/status/UI surfaces expose only closed lifecycle state (REQ-AGENT-015). |

## Mobile full-write surface (supersedes REQ-AND-013)
Making the phone a full cloud-write node enlarges the surface (T4, T7). It is gated by the
combination of: session-token gate (T4), one-time PendingAction token (T2), per-action
biometric-token confirmation before Agent-token consumption, Android Keystore for
credentials (T6), native BiometricPrompt for every destructive confirmation, foreground
job presentation, strict device-state gates, and resumable jobs (T7). #625 implements the
server/router/job contract; #626 implements the WorkManager/notification/native ordering
and closes the physical native-prompt, Keystore, foreground-notification, process-death,
and deterministic application-network recovery gates. The complete package is a **release blocker**
for the mobile full node
(REQ-AND-016, stories S-AG.10/#625 + S-AG.11/#626) — not optional hardening.
