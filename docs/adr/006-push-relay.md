# ADR-006 — Push notification delivery (relay + transport)

- **Status:** Proposed. Finalized when the push epic (R2) starts; recorded now so the
  reliability requirements are not discovered late.
- **Date:** 2026-06-22
- **Related:** ADR-005 (Android build), [no paid dependencies](../../README.md) (project
  constraint), Graph change-notification subscriptions.

---

## Context

The mobile app should raise **native** notifications (the normal Android drawer) for new
live items and backup/sync errors. The display is the standard `NotificationManager`;
the open question is the **wake transport** when the app is backgrounded.

Two facts constrain it:

1. **Graph does not push to devices.** Change-notification subscriptions deliver to a
   **public HTTPS webhook** (or to Azure **Event Hubs / Event Grid** — which are *paid*
   services, excluded by the project's no-paid-dependencies rule). Subscriptions also
   **expire** and must be renewed, and missed notifications need a delta-query catch-up.
2. **"Like every other app" means FCM.** The chosen device wake transport is **Google
   Firebase Cloud Messaging** — free, but a Google dependency in an otherwise
   self-hosted / Google-free product.

## Decision (to finalize at R2 start)

- **Relay:** a small, self-hostable **HTTPS webhook** (no Event Hubs/Grid) that
  - acknowledges Graph fast (within the few-second budget, else notifications are
    dropped/delayed) and **enqueues to durable storage**,
  - is **idempotent per notification id**,
  - **renews subscriptions** before expiry and runs a **delta-query catch-up** for gaps,
  - authenticates and rate-limits its own endpoint.
- **Transport:** FCM (`FirebaseMessagingService`, token rotation, Google-Play-Services
  check, Android-13 runtime notification permission, notification channels).
  `google-services.json` is provided by the operator and **never committed**.
- **Pluggable.** Delivery sits behind a `PushProvider` interface so **ntfy / UnifiedPush**
  can replace FCM; R2 must ship either that self-hosted fallback or a documented
  limitation, so the Google dependency stays a visible product decision.

## Consequences

- **+** Reliable notifications without a paid Azure tier; standard native UX.
- **−** A relay service to run (state, renewal, retries) and a free **Google/Firebase**
  dependency at the transport layer — accepted, but kept swappable on purpose.

## Implementation status (2026-06-24)

The **transport + direct-trigger** half of this ADR is **implemented and
device-verified** (#576); the **relay** half (Graph change-subscription → webhook →
catch-up, for background wake while the app is closed) remains **proposed** (R2).

Shipped:

- **FCM HTTP v1 sender** — `crates/graph/src/push.rs`: service-account → RS256-signed
  JWT (`ring`) → OAuth2 access token → `messages:send`. The private key and access
  token are never logged. Behind the `http` feature. (REQ-AND-008)
- **`PushProvider` boundary** — the daemon registers device tokens and sends through the
  webui `PushHandler` trait (`DaemonPush`), so ntfy / UnifiedPush can replace FCM
  without touching callers. FCM is the shipped provider; the self-hosted fallback stays
  the documented alternative, keeping the Google dependency a visible product decision.
- **Device-token registration** — the native shell exposes the FCM token via a JS bridge
  (`AndroidPush.fcmToken()`); the web UI registers it at the cap-gated
  `POST /api/v1/push/register` (dedup, persisted next to the account archive). (REQ-AND-009)
- **Direct trigger** — a completed local backup pass that archived new content notifies
  the phone ("N emails backed up"); no relay needed while the daemon runs. (REQ-AND-010)

Still proposed (R2): the public HTTPS relay (durable queue, per-id idempotency,
subscription renewal, delta catch-up, endpoint auth/rate-limit) for wake-on-cloud-change
when the daemon is not the one observing the change.
