# Dependency advisory review — 2026-09-06

Refs #850. This review covers the three expired advisory exceptions in the `dev`
dependency graph. It is maintenance evidence, not release or device qualification.

## Resolved vulnerabilities

RUSTSEC-2026-0194 and RUSTSEC-2026-0195 affect quick-xml versions below 0.41.0.
The former concerns quadratic duplicate-attribute checking; the latter concerns
unbounded namespace-declaration allocation.

The previous lockfile resolved quick-xml 0.39.4 through wayland-scanner 0.31.10.
The compatible stable wayland-scanner 0.31.11 release now accepts quick-xml 0.41.
The updated lockfile selects wayland-scanner 0.31.11 and quick-xml 0.41.0, while
retaining winit 0.30.13. Both advisory exceptions are removed.

Sources:

- [RUSTSEC-2026-0194](https://rustsec.org/advisories/RUSTSEC-2026-0194.html)
- [RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195.html)
- [wayland-scanner 0.31.11 dependency metadata](https://crates.io/api/v1/crates/wayland-scanner/0.31.11/dependencies)

## Bounded informational exception

RUSTSEC-2026-0192 classifies ttf-parser as unmaintained, with no patched version.
It does not identify a vulnerability. Version 0.25.1 remains in these runtime font
paths:

- cosmic-text 0.19.0 → fontdb 0.23.0 → ttf-parser;
- winit 0.30.13 → sctk-adwaita 0.10.1 → ab_glyph → owned_ttf_parser → ttf-parser.

The current stable cosmic-text and winit versions do not admit fontdb 0.24 or
sctk-adwaita 0.11 under their existing semver constraints. Winit 0.31 remains a
prerelease. Migrating those parent APIs or replacing the GUI stack is outside
this maintenance change.

Retain only this informational exception until **2026-10-06**, with another
review of compatible parent releases by that date. This explicitly accepts the
maintenance risk for the existing desktop font paths; it does not assert that
unmaintained font parsing is generally safe or exempt future vulnerability reports.

Sources:

- [RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192.html)
- [cosmic-text 0.19.0 dependencies](https://crates.io/api/v1/crates/cosmic-text/0.19.0/dependencies)
- [winit 0.30.13 dependencies](https://crates.io/api/v1/crates/winit/0.30.13/dependencies)
- [fontdb 0.24.0 dependencies](https://crates.io/api/v1/crates/fontdb/0.24.0/dependencies)
- [winit release metadata](https://crates.io/api/v1/crates/winit)

## Queued updates and verification boundary

The maintenance candidate also incorporates #841–#845, #851, and #853. Third-party
actions remain pinned to immutable commits. Argon2 0.6.0 retains the explicit
Argon2id version 0x13 parameters and HKDF labels used by existing paired sessions.
An independent libargon2/HMAC-SHA256 known-answer regression checks the resulting
session key, in addition to the existing encrypted-session tests.

The weekly reminder selects the oldest open issue with the exact advisory-review
title from security issues, avoiding punctuation-sensitive full-text search.
The PR records actual focused, aggregate, security, and CI results against its
implementation commit. No real provider, physical-device, installation, promotion,
or release result is claimed by this document.
