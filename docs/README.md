# Documentation

Public-facing docs. The detailed design (SDD) is maintained internally.

## Index

- [ARCHITECTURE.md](ARCHITECTURE.md) — high-level design.

## Planned artifacts

These will be filled as the corresponding work lands:

- `graph-capability-matrix.md` — proven scopes / capabilities per service.
- `restore-fidelity-matrix.md` — what is preserved vs. lossy on restore.
- `sync-state-machine.md` — per-item sync state automaton.
- `path-mapping.md` — cloud↔local namespace mapping rules.
- `delete-trash-conflict-model.md` — deletion, trash and conflict handling.
- `local-api-security.md` — Unix socket / TLS, tokens, CSRF.
- `auth-token-lifecycle.md` — OAuth, token storage, invalidation.
- `sqlite-snapshot-consistency.md` — quiesce / WAL checkpoint / PBS.
- `packaging-daemon-model.md` — daemon vs. GUI packaging.
- `html-viewer-security.md` — mail viewer sanitization.
- `test-chaos-matrix.md` — chaos / data-loss test matrix.
