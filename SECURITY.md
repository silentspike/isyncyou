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

- **Cloud-mutating restore is off by default and mail-only when enabled**
  (`restore.cloud_restore_enabled = false`). Only mail is ledger-backed and
  boot-recovered (so an interrupted restore cannot silently duplicate an item);
  non-mail cloud re-create is refused until each service is ledger-migrated.
- **Data at rest is currently unencrypted** (SQLite store + cached tokens, behind
  file permissions). An at-rest encryption layer is designed but not yet shipped.
