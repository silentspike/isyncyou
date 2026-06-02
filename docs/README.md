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

## Planned artifacts

These will be filled as the corresponding work lands:

- `sqlite-snapshot-consistency.md` — quiesce / WAL checkpoint / PBS (quiesce not yet implemented).
- `packaging-daemon-model.md` — daemon vs. GUI packaging.
- `html-viewer-security.md` — sanitized mail viewer (viewer not yet implemented).
- `test-chaos-matrix.md` — chaos / data-loss matrix (logic-level lives in `crates/acceptance`).
