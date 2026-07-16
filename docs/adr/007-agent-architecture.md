# ADR-007: In-app M365 agent architecture

**Status:** Proposed

## Context

iSyncYou is a complete M365 backup/sync client (desktop daemon + standalone
Android node) with a live read/write layer and a WebView UI. The missing surface is
a natural-language one over the user's *own* data: a **data assistant** (Q&A over the
backed-up archive, with citations) and an **operations agent** (backup / restore /
live-write driven by natural language, destructive actions confirmation-gated).

The constraints that force the shape of this decision:

- The desktop daemon and the Android engine already share
  `isyncyou_app_host::build_live_router` — one wired-in handler serves both.
- The WebView CSP is `connect-src 'self'` (ADR-004), so an LLM call **cannot** be made
  from the browser; it must run server-side, in the Rust process.
- `crates/graph` is a synchronous, Graph-specific HTTP client (blocking `reqwest` +
  rustls behind its `http` feature). It is the wrong place for LLM duties.
- The agent will read untrusted content (mail bodies, documents). A naive design that
  lets the model self-authorize actions is a prompt-injection hole.
- This is a public repo with an audit-clean posture. Provider authentication must be
  explicit, encrypted at rest, and separated from developer-local CLI credentials.

## Decision

1. **A new `crates/agent`** holds a provider-agnostic harness (HTTP transport, provider
   trait, turn loop, typed tool surface, session store, stream hub). It depends on
   `engine`/`store`/`graph` but keeps LLM concerns out of `graph`.

2. **Own HTTP transport.** `agent::http::HttpTransport` is a crate-internal blocking
   `reqwest` + rustls client that mirrors graph's retry/classification ideas. We do
   **not** reuse `GraphClient` for LLM calls.

3. **Shared handler.** `DaemonAgent: AgentHandler` is wired into `build_live_router`, so
   it exists on the desktop daemon and the Android engine from one wiring. The blocking
   loop runs on a background thread (the proven `refresh_loop` pattern), feeding a
   stream hub — no async rewrite of `graph`.

4. **App-scope invariant (the safety model).** The agent's *only* capability is a single
   `isyncyou` tool over the M365 domain (search/read/list/export/restore-local/backup/
   restore-cloud/live-write). There is **no** shell, arbitrary-filesystem, OS, device, or
   free-form-HTTP tool. "Full power" is therefore bounded by the user's M365 data, not the
   system. A tool-registry snapshot test enforces it.

5. **Provider strategy.** The product path is app-authorized Claude/Codex OAuth:
   the Rust host/mobile process performs the provider call, stores OAuth material in
   the encrypted agent CredentialStore, streams provider events into the AgentStreamHub,
   and keeps Codex request retention disabled (`store:false`). `FakeProvider` is the
   deterministic CI/unconfigured fallback only. BYO Anthropic/OpenAI API keys are not
   the #623 product path. Local `claude`/`codex` CLI credential fallback and private
   wire-drift capture remain a separate, default-off #627 surface
   (`agent-subscription-experimental`) and are not product-auth evidence.

   The product harness sequence is fixed: complete official provider OAuth, store
   the resulting credential in the encrypted iSyncYou CredentialStore, preserve
   the provider-required auth/billing/subscription identity envelope unchanged and
   in place, remove all other default-client harness behavior, then install the
   iSyncYou harness. For Claude, the original billing block remains the first
   system block. For Codex, required subscription/account identity headers remain
   unchanged and requests retain `store:false`. Required protocol fields such as
   content/stream types and provider version fields remain where the protocol
   requires them.

   The replacement harness contains the iSyncYou M365 system prompt, exactly one
   `isyncyou` tool, StoreArchive retrieval/citations, confirmation policy, and
   streaming/usage handling. It does not import the default client's system
   prompt, tools, skills, plugins, MCP configuration, rules, memories, history,
   client context, shell/filesystem capabilities, or other default agent behavior.
   #627's optional local resolver supplies credential material only and cannot
   advance product OAuth, encrypted-credential, subscription-identity, onboarding,
   handoff, or ready state. #639 owns the visible first-run ordering/state machine;
   the provider builders and #627 evidence enforce the retained-versus-removed
   request boundary.

6. **Confirmation without model authority.** Read/search/list/export/restore-local run
   immediately; `backup`/`restore-cloud`/`live-write`/`share` emit a **PendingAction**.
   The human confirms via a normal session-authenticated request, and the server then
   mints/checks a **one-time confirmation token** bound to `action-hash + account +
   service + item-id + TTL`. The model/agent **never** receives a capability token. Tool
   results carrying mail/document bodies are marked `untrusted_content`; the system prompt
   states content can never override policy. On desktop, confirmed operations reuse the
   existing daemon/app-host/engine runtime paths: backup uses the shared refresh runner,
   restore-cloud uses the ledger restore path, live-write uses the service writer traits,
   share uses the ledger-backed share handler, and restore-local writes only to a controlled
   local restore root. Mobile uses the same confirmed-action model but routes backup and
   restore-cloud through durable mobile jobs, gates Agent confirmation with the per-action
   biometric-token layer before consuming the Agent token, and limits mobile live-write to
   the explicit metadata allowlist proven by the native Android closeout (REQ-AND-016).

7. **Encrypted, conflict-safe cross-device session.** Per-turn ULID files under
   `/Apps/iSyncYou/agent/<session>/<ulid>.json`, AES-256-GCM with a key derived
   (Argon2id/HKDF) from a **pairing secret** (not the device-local token-cache key, which
   would block cross-device decryption), an active-turn lease to prevent forks.

8. **Streaming.** A new `AgentStreamHub` (per-`turn_id` bounded channel; typed events
   `token`/`tool_call`/`tool_result`/`confirmation_required`/`error`/`done`; cancel;
   backpressure/timeout). The existing SSE path only emits a `change` ping and is not a
   token stream.

9. **Network-critical Android flows.** Product OAuth, explicit credential refresh, and
   active streamed turns acquire a bounded, acknowledged `dataSync` foreground-service
   lease before network work. The service is an execution-priority mitigation, not a
   claim that every network path is reachable. A provider/purpose-allowlisted preflight
   returns only closed diagnostic codes; mobile observations cross the WebView boundary
   as single-use, session/guard/reason-bound handles rather than raw device state.
   Credential status is network-free, refresh is explicit and serialized, and a present
   invalid product credential fails closed without local-client or FakeProvider fallback.
   Default product artifacts exclude the separate diagnostic hook (REQ-AGENT-013, #640).

13. **First-run official OAuth → visible, host-enforced custom-harness handoff (REQ-AGENT-014,
    S-AG.14/#639).** Product readiness is a durable, authenticated *activation authority*, not
    credential presence. `provider_ready(p)` holds only when a valid **Active** V2 credential bundle
    exists, a durable `ProductActivationV1` matches its credential generation + the official-OAuth
    policy fingerprint + the harness contract version, and a static harness attestation passes; it is
    **decoupled from provider selection** (an activated non-selected provider still reads ready) and
    an experimental local-CLI credential can never satisfy it. Attestation is two-level: a static
    per-provider allowlist over the shipped harness, plus a **per-round `AttestedProviderRequest`**
    that the transport is the only value it will send — every `provider.next(history)` re-attests the
    actually-sent request, so a mutated header/body cannot reach the wire. A single **product-runtime
    gate** (one in-process mutex) spans selection + readiness (activation + attestation) + provider
    construction and runs **before** any turn-id / stream-slot / archive resolution; a not-ready
    product turn returns a typed `AgentStartTurnError::ProductNotReady` mapped to a closed **409** and
    creates no turn state, with **no cross-provider fallback**. The official OAuth flow records an
    **authenticated onboarding journal** of ordered transitions (official sign-in → credential
    encrypted → retained envelope verified → default harness removed → M365 profile activated →
    iSyncYou tool connected → subscription identity set → ready), keyed by the opaque attempt id
    in-flight and by the credential generation once durable, so startup **crash-window recovery** can
    resume: an activation missing after a credential write is re-attested and activated **without a
    re-exchange**, and an interrupted attempt becomes `error_redacted` and is never resumed. A refresh
    is a **V2 lifecycle event only** and never a journal event. The status onboarding projection
    survives journal TTL (a ready provider reports every step from the activation, never the TTL'd
    journal) and carries `Cache-Control: no-store` with no secret material; `/oauth/complete` is a
    strict-JSON, attempt-state-bound, Claude-only manual step (Codex ends via its loopback callback).
    The first-run wizard renders the ordered handoff from the host projection and keys all gating off
    host readiness, never credential-presence flags. **#627 experimental** stays compiled-opt-in only
    and never sets `connected`/activation/readiness; **FakeProvider** is a test/fixture provider only,
    never an unconfigured product turn.

14. **Reversible product-account lifecycle (REQ-AGENT-015, S-AG.20/#645).** Disconnect,
    Reconnect, and verified-identity Switch are durable provider-scoped operations rather than
    local token deletion. A provider lifecycle lease excludes same-provider turns, refresh, OAuth,
    and maintenance while a mutation is active. An encrypted authority record reserves a monotonic
    fence and stable installation-bound idempotency key before the journal is published; even the
    earliest prepared state makes product readiness false. Revocation records its request target
    separately from the provider's scope guarantee. A confirmed response is persisted before
    provider-scoped credential, activation, usage, settings, and onboarding state are removed;
    an ambiguous or failed response retains encrypted grant authority in a non-ready state.

    Reconnect revokes and cleans the prior generation before opening OAuth. Connect, Reconnect, and
    Switch persist every exchange result first as an encrypted non-ready candidate. Candidate
    identity validation and activation happen only afterward; a rejected candidate is itself a
    separate revoke leg and is retained when its revoke result is unknown. Codex account switching
    requires a strictly validated signed subject. Claude exposes no mutating Switch operation until
    an equivalent verified identity contract exists. On Android, every revoke leg uses the bounded
    `credential_revoke` foreground guard and a one-shot session/guard/purpose-bound network
    snapshot. Default product artifacts exclude deterministic lifecycle test hooks.

15. **Shared product-session authority (REQ-AGENT-016, S-AG.16/#628).** Writable
    product sessions use V2 encrypted records and an authoritative manifest. Immutable
    visible records, request journals, provider-step outcomes, and UUID bindings are
    staged first and become authoritative only through one lease- and fence-bound
    manifest CAS. Legacy V1 sessions remain readable but cannot accept new turns.
    Request recovery is bound to provider, model, credential generation, OAuth policy,
    harness contract, and the canonical installation identity. A changed binding or an
    outbound provider step without a durable outcome fails closed rather than retrying.

    A renewal worker owns quiet long turns. Cancellation is shared by the provider,
    tool loop, and host, while the host alone emits terminal events after durable
    publication. App-wide confirmation, pairing, mutation reservations, and request
    tombstones live in one encrypted control store. Pairing V2 reveals a five-minute
    one-time transfer only after user presence and permanently fences the first claim.
    Mutable APIs use strict JSON and bounded sealed chunks; desktop authority is an
    HttpOnly process cookie with exact-origin mutation checks, while Android injects
    its native session authority.

## Consequences

- **Cost:** a new crate, a second HTTP client (small, blocking, rustls), and a genuinely
  new streaming hub. The blocking-loop-on-a-thread choice trades a little plumbing for not
  rewriting `graph` as async.
- **Mobile becomes a full cloud-write node** (superseding REQ-AND-013) in two layers:
  #625 implements the Rust/server contract (session/cap/per-action-token gates, durable
  backup/restore jobs, restart recovery, and mobile live-write allowlist), while #626 owns
  the native Android proof (Keystore, physical BiometricPrompt, foreground-service/
  WorkManager presentation, and device evidence). #626 now supplies the WorkManager
  contract, strict notification/device gates, bounded native hooks, and probe tooling.
  Its Pixel 8 Pro closeout proves physical native user presence, hardware-backed
  Keystore sealing, visible foreground execution, notification denial, process-death
  adoption, and deterministic application-network retry while USB ADB remains intact.
  Tracked in
  `docs/requirements/android.yml` (REQ-AND-016) and the agent threat model.
- **Governance:** the invariants above are tracked as `REQ-AGENT-*` in
  `docs/requirements/agent.yml`, the mobile-write surface in
  `docs/security/agent-threat-model.md`, and the Claude/Codex OAuth plus #627 local
  fallback residual risk as an accepted entry in the risk register.
- **Supersedes:** none. **Superseded by:** none (yet).
