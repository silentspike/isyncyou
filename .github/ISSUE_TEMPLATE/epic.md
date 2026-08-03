---
name: Epic
about: A self-contained initiative spanning multiple bounded stories.
title: '[Epic] E-X: '
labels: type:epic
assignees: ''
---

## Product outcome

<!-- What observable user or release outcome does this epic deliver? -->

## Current baseline

<!-- Separate landed foundations, remaining gaps, and volatile start gates. -->

## Scope boundary

### In scope

-

### Out of scope

-

## Story dependency graph

```text
S-? -> S-?
```

## Stories and disposition

- [ ] S-X.1: ... (release-blocking / optional / post-release)
- [ ] S-X.2: ...

## Architecture decisions

| Decision | Value | Rationale |
|----------|-------|-----------|
|          |       |           |

## Epic acceptance criteria

<!-- Map each final claim to child implementation plus required evidence. -->

- [ ] ...

## Verification and release boundary

- Child stories use focused iteration and one aggregate candidate gate; the epic
  does not require every child to rerun unchanged surfaces after each edit.
- Live/device/release evidence belongs only to the story that owns that claim.
- State the exact candidate identity, branch flow, and approvals required for any
  promotion or release. Epic completion alone is not release authorization.

## Done when

- [ ] All release-blocking child stories are merged and their acceptance criteria
      and evidence are read back from the required branch.
- [ ] Optional or deferred stories are named explicitly and do not silently block
      or expand the epic.
