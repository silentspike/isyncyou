# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** via GitHub Security Advisories:

**https://github.com/silentspike/isyncyou/security/advisories/new**

Do **not** open a public issue for security problems. There is no email or PGP channel — GitHub Security Advisories is the single reporting channel.

## Supported versions

iSyncYou is pre-release (private until its first release candidate). Until the
first tagged release, only the tip of the `dev` branch is supported — please report
issues against `dev`. After the first release, the latest minor release line will
receive security fixes.

## Scope

iSyncYou handles Microsoft 365 access tokens and personal data. Of particular interest:

- Token storage / handling, local API authentication (Unix socket / TLS + capability tokens, CSRF).
- The HTML mail viewer (sanitization, no JS, blocked remote resources).
- Path mapping / sync data-integrity issues that could cause data loss.
- Restore correctness under failure — see the cloud-restore entry in the
  [risk register](docs/security/risk-register.md).

## Known security-relevant posture

These are documented, deliberate states — not undisclosed weaknesses. The full
list with mitigations and status lives in the
[risk register](docs/security/risk-register.md):

- **Cloud-mutating restore is off by default**
  (`restore.cloud_restore_enabled = false`). All five backup services (mail,
  calendar, contacts, ToDo, OneNote) are ledger-backed and boot-recovered (so an
  interrupted restore cannot silently duplicate an item); a service with no
  crash-safe path is refused rather than run unsafely.
- **Data at rest**. `isyncyou login --keyring` stores OAuth token JSON in the
  desktop Secret Service / KDE Wallet compatible keyring and leaves only a
  non-secret marker file in the archive root. File caches are owner-only on Unix
  (`0600`) and AES-256-GCM encrypted — with a configured secret
  (`ISYNCYOU_TOKEN_CACHE_KEY_FILE`, systemd credential `isyncyou-token-cache-key`,
  or `ISYNCYOU_TOKEN_CACHE_KEY`), or, when none is set, with an auto-generated
  owner-only local key kept beside the cache (tokens are never written in the
  clear; the local key does not defend against full config-directory read access).
  The SQLite store is SQLCipher-encrypted when `ISYNCYOU_STORE_KEY_FILE`, systemd
  credential `isyncyou-store-key`, or `ISYNCYOU_STORE_KEY` is configured;
  an existing plaintext store migrates in place with
  `isyncyou migrate --account <id> --encrypt-store` (atomic, refuses without a
  key). Without a store key, stores remain plaintext and the doctor reports that
  as a warning.
