# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** via GitHub Security Advisories:

**https://github.com/silentspike/isyncyou/security/advisories/new**

Do **not** open a public issue for security problems. There is no email or PGP channel — GitHub Security Advisories is the single reporting channel.

## Scope

iSyncYou handles Microsoft 365 access tokens and personal data. Of particular interest:

- Token storage / handling, local API authentication (Unix socket / TLS + capability tokens, CSRF).
- The HTML mail viewer (sanitization, no JS, blocked remote resources).
- Path mapping / sync data-integrity issues that could cause data loss.
