# Requirements as Code

Requirements live here as version-controlled YAML, each with an explicit statement,
acceptance criteria, and **traceability** to the test(s) that prove it. A mechanical
checker fails if a requirement is malformed or if an `implemented` requirement points
at a test that does not exist — so "this is covered" always has a receipt.

This is the requirements counterpart to the [ADR](../adr/) (decisions) and the
[risk register](../security/risk-register.md) (risks).

## Files

| File | Area |
|---|---|
| [`sync.yml`](sync.yml) | Core sync invariants — the v0.1 acceptance criteria A1–A10 |
| [`restore.yml`](restore.yml) | Restore safety — default-off gate, mail ledger/recovery, non-mail refusal, and restore OAuth scope invariants |
| [`security.yml`](security.yml) | Local API security invariants — TCP Host/Origin boundary, destructive POST guard, and owner-only Unix socket |
| [`operations.yml`](operations.yml) | Operations invariants — standalone doctor behavior and local health reporting |

## Schema

```yaml
requirements:
  - id: REQ-<AREA>-NNN        # unique, e.g. REQ-RST-001
    title: short title
    statement: what must be true
    rationale: why (optional)
    status: implemented | planned
    design: docs/adr/001-restore-semantics.md   # optional; must exist if given
    acceptance:                # non-empty list of observable criteria
      - ...
    verified_by:               # list of {test: <fn name>} or {file: <path>}
      - test: some_test_fn
```

- **`implemented`** requirements must have at least one `verified_by` entry, and every
  `test:` reference must resolve to a `fn <name>(` in the source tree (`crates/`,
  `bin/`, `gui/`). Every `file:` reference must exist.
- **`planned`** requirements are tracked but not yet required to have tests — their
  tests land with the implementation. Any `design:` document they cite must already
  exist, so a planned requirement is never an untraceable promise.

## Running the checker

```sh
python3 tools/check_traceability.py          # from the repo root
```

Exit code 0 = all requirements well-formed and every implemented requirement traceable;
non-zero on any violation. The only dependency is PyYAML.

This checker is wired into CI as a required check in the public-CI-hardening stage; until
then it is run locally and on demand.
