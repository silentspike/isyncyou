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
   local restore root. Mobile destructive Agent execution remains fail-closed until the
   Android security/job stories land (REQ-AGENT-012).

7. **Encrypted, conflict-safe cross-device session.** Per-turn ULID files under
   `/Apps/iSyncYou/agent/<session>/<ulid>.json`, AES-256-GCM with a key derived
   (Argon2id/HKDF) from a **pairing secret** (not the device-local token-cache key, which
   would block cross-device decryption), an active-turn lease to prevent forks.

8. **Streaming.** A new `AgentStreamHub` (per-`turn_id` bounded channel; typed events
   `token`/`tool_call`/`tool_result`/`confirmation_required`/`error`/`done`; cancel;
   backpressure/timeout). The existing SSE path only emits a `change` ping and is not a
   token stream.

## Consequences

- **Cost:** a new crate, a second HTTP client (small, blocking, rustls), and a genuinely
  new streaming hub. The blocking-loop-on-a-thread choice trades a little plumbing for not
  rewriting `graph` as async.
- **Mobile becomes a full cloud-write node** (superseding REQ-AND-013), but only together
  with a full Android security model (Keystore, biometric-gated destructive confirmations,
  foreground-service/WorkManager backup, resumable ledger jobs). Tracked in
  `docs/requirements/android.yml` (REQ-AND-016) and the agent threat model.
- **Governance:** the invariants above are tracked as `REQ-AGENT-*` in
  `docs/requirements/agent.yml`, the mobile-write surface in
  `docs/security/agent-threat-model.md`, and the Claude/Codex OAuth plus #627 local
  fallback residual risk as an accepted entry in the risk register.
- **Supersedes:** none. **Superseded by:** none (yet).
