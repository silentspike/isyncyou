# Documentation

Public-facing docs. The detailed design (SDD) is maintained internally.

## Index

- [ARCHITECTURE.md](ARCHITECTURE.md) — high-level design.
- [graph-capability-matrix.md](graph-capability-matrix.md) — proven scopes / capabilities per service.
- [restore-fidelity-matrix.md](restore-fidelity-matrix.md) — what is preserved vs. lossy on restore.
- [sync-state-machine.md](sync-state-machine.md) — per-item sync state automaton.
- [path-mapping.md](path-mapping.md) — cloud↔local namespace mapping rules.
- [delete-trash-conflict-model.md](delete-trash-conflict-model.md) — deletion, trash and conflict handling.
- [local-api-security.md](local-api-security.md) — local web UI/API security.
- [auth-token-lifecycle.md](auth-token-lifecycle.md) — OAuth, token storage, invalidation.
- [sqlite-snapshot-consistency.md](sqlite-snapshot-consistency.md) — SQLite `VACUUM INTO`, PBS staging and temporary restore-store rules.
- [packaging-daemon-model.md](packaging-daemon-model.md) — daemon vs. GUI packaging & distribution.
- [html-viewer-security.md](html-viewer-security.md) — safe rendering of archived items (escape + CSP).
- [test-chaos-matrix.md](test-chaos-matrix.md) — chaos / data-loss matrix and how each case is verified.
- [acceptance-v0.1.md](acceptance-v0.1.md) — v0.1 acceptance gate: A1–A10 evidence + live test-account verification.

## Planned artifacts

No mandatory SDD document from the current plan is missing. New docs should be
added here when a new design surface lands.
