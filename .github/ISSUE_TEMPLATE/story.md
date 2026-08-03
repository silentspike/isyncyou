---
name: Story
about: A self-contained user-facing capability or workstream.
title: '[Story] S-X: '
labels: type:story
assignees: ''
---

## Parent and dependencies

**Parent epic:** Refs #
**Depends on:** #
**Blocks:** #

<!-- State which dependencies are already landed and which are hard start gates. -->

## User outcome

As a [role],
I want [capability],
so that [observable benefit].

## Current baseline

<!--
Describe the relevant behavior in the current target branch. Assume the worker
knows only this issue and the repository-local instructions. Do not rely on old
plans, private chat, or "as discussed" context.
-->

## Scope

### In scope

-

### Out of scope

-

## Acceptance criteria

<!-- Use observable outcomes. Separate deterministic, live, device, and release claims. -->

- [ ] ...

## Expected change boundary

<!-- Name likely files/modules and explicitly call out neighboring systems not to redesign. -->

-

## Implementation and verification cadence

- Read the complete affected path and its callers, consumers, tests, recovery,
  and evidence contracts before editing.
- Implement related behavior in a small number of coherent blocks. Do not create
  a build, evidence, or commit cycle for every checklist line.
- After each block, run cheap syntax/static checks and only the focused tests for
  the changed surface. Batch related findings before rerunning checks.
- Run the repository-required aggregate build, test, lint, security,
  traceability, and evidence gates once against the frozen candidate.
- Run live, device, provider, or release work only when an acceptance criterion
  requires that surface. State explicitly which surfaces are not required.

## Privacy and evidence

<!-- Define forbidden public data, evidence identity, cleanup, and honest FAIL/BLOCKED handling. -->

-

## Landing

<!-- State target branch, issue-closing behavior, and every separate approval boundary. -->

- One protected PR to `dev` unless this issue explicitly defines another target.
- No promotion, tag, RC, release, or deployment is implied by implementation.
