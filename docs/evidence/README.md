# Evidence Manifests

An **evidence manifest** is a machine-readable record that links each requirement to
the executed proof that verifies it. It is the last link in the chain this repo is
built around:

```
requirement (docs/requirements/*.yml)  ──►  test / example  ──►  evidence manifest
        check_traceability.py                                    check_evidence.py
```

Each tool checks its own link, so "this is covered" is never a claim — it is a
traceable, validated fact.

## Files

| File | Purpose |
|---|---|
| [`manifest.schema.json`](manifest.schema.json) | JSON Schema (draft 2020-12) defining a manifest's shape |
| [`sample-manifest.json`](sample-manifest.json) | A real example: six entries citing the actual tests/example and their captured output |

## Schema, in short

A manifest records the `commit` and `generated_at` time, then a list of `entries`,
each with:

- `id` — `EV-NNN`
- `requirement` — the `REQ-…` it verifies (must exist in `docs/requirements`)
- `claim` — the specific thing demonstrated
- `method` — `test` | `command` | `example` | `probe` (`probe` = a live check)
- `command` — the exact command that produced the evidence
- `result` — `pass` | `fail` (a manifest records failures honestly)
- `evidence` — a captured excerpt of the real output
- optional `artifact`, `notes`

## Validating

```sh
python3 tools/check_evidence.py        # validates docs/evidence/sample-manifest.json
```

The validator checks the manifest against the schema **and** cross-checks that every
cited requirement exists in `docs/requirements`. It exits non-zero on any violation,
so it can run as a required CI check (wired in the public-CI-hardening stage).

## Why a sample, not a generated manifest

The sample is hand-authored from real run output so the format is reviewable on its
own. The natural next step is a small generator that runs the cited commands and
emits a manifest automatically; the schema and validator are designed for exactly
that, so a generated manifest validates with the same tool.
