# Experimental local CLI credential fallback

The `agent-subscription-experimental` feature is unsupported, default-off, and
limited to personal Linux desktop development builds. It is not part of the
desktop product defaults, Android, CI release artifacts, or product onboarding.

Product authentication remains app-managed Claude/Codex OAuth. iSyncYou stores
those credentials in its encrypted CredentialStore, and a present product
credential always wins. Invalid or refresh-failed product credentials require a
product reconnect; they never fall through to a local client.

The experimental fallback may be enabled only through the daemon's explicit
feature:

```bash
cargo remote -c -- build -p isyncyou-daemon --features agent-subscription-experimental
```

When enabled on Linux and product credentials are absent, the daemon may read the
credential file maintained by the installed Claude or Codex client. It uses the
credential in memory and never copies or migrates it into the iSyncYou
CredentialStore. It imports no client prompt, tools, skills, plugins, MCP
configuration, rules, memories, history, or execution context. The normal
iSyncYou M365 prompt, single `isyncyou` tool, retrieval, confirmation, streaming,
and usage handling remain authoritative.

Local client credentials cannot satisfy product OAuth connection, encrypted
credential, subscription identity, onboarding completion, or product-ready state.
They are development fallback input only and are never product-auth evidence.

## Drift capture privacy

The tracked capture wrapper invokes the already authenticated local clients in
private temporary directories with bounded timeouts. It does not open their auth
files. Raw output remains owner-only under `/tmp`, is deleted on every exit path,
and must never be committed or attached to an issue.

Only allowlisted reduced summaries may enter `docs/evidence`. They contain client
versions, bounded event/usage structure, presence booleans, and an honest drift
decision. They contain no credentials, account or request identifiers, prompt or
response text, local paths, raw hashes, full header sets, or standalone provider
request recipe. Missing visibility is recorded as `not_safely_observable`, never
as a pass.

## Rollback

Rebuild the daemon without `agent-subscription-experimental`. No data migration or
credential cleanup is required because the fallback never persists local client
credentials.
