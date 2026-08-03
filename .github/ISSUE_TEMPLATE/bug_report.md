---
name: Bug report
about: A reproducible defect with a bounded correction and regression proof.
title: '[Bug] '
labels: type:bug
assignees: ''
---

## Current behavior

## Expected behavior

## Reproduction

1.

## Environment and baseline

- iSyncYou version / commit:
- Target branch:
- OS / desktop / Android version:
- First known bad and last known good, when available:

## Impact and safety boundary

<!-- User impact, data/authority risk, affected surfaces, and immediate containment. -->

## Scope

### In scope

- Smallest correction that restores the expected contract.
- Regression coverage for the real failing path.

### Out of scope

- Unrelated refactors or feature expansion discovered during diagnosis.

## Acceptance criteria

- [ ] The reproduction fails before the fix and passes after it.
- [ ] Direct callers, consumers, recovery/teardown, and rejection paths remain correct.
- [ ] No unrelated behavior or platform is changed.

## Verification cadence

- Diagnose first, then implement related fixes as one coherent block.
- Run cheap checks and focused regression tests during iteration.
- Run the required aggregate gate once after the candidate diff is frozen.
- Do not repeatedly rebuild unchanged platforms.

## Redacted logs and evidence

<!-- Never include credentials, account identity, device serial, private content, or raw auth/provider data. -->

## Landing

<!-- Target branch, close semantics, rollback, and any separate operational approval. -->
