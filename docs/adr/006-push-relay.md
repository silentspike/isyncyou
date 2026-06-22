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
