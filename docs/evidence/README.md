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
| [`sample-manifest.json`](sample-manifest.json) | A real example: requirement evidence entries citing the actual tests/examples/probes and their captured output |

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
python3 tools/check_evidence.py                 # validates docs/evidence/sample-manifest.json
python3 tools/check_evidence.py --require-head  # for freshly generated CI manifests
```

The validator checks the manifest against the schema, verifies the recorded
`commit` exists in the current Git repository, cross-checks that every cited
requirement exists in `docs/requirements`, and ensures each `method: test` entry
names a real Rust test function. With `--require-head`, it also requires
`commit == HEAD`; use that mode for generated manifests, not for the tracked sample
file.

## Why a sample, not a generated manifest

The sample is hand-authored from real run output so the format is reviewable on its
own. The natural next step is a small generator that runs the cited commands and
emits a manifest automatically; the schema and validator are designed for exactly
that, so a generated manifest validates with the same tool.
