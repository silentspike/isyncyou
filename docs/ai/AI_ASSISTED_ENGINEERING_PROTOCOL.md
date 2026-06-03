# AI-Assisted Engineering Protocol

This project is built by a human engineer directing an AI coding agent. That fact is
stated openly because the interesting part is not *that* AI wrote code — it is the
**protocol that makes AI-assisted output trustworthy**. "The model wrote a lot of
code" is worthless without a gate that catches the plausible-but-wrong. This document
is that gate, written down so it is reproducible and auditable.

It is the long form of the README's *Engineering approach* note, and it governs every
change in the repository.

---

## Principle

> **AI accelerates production. The gate guarantees correctness. The human owns
> direction and acceptance.**

Speed is never traded against verification. An AI agent can produce a confident,
well-structured, completely wrong change in seconds; the entire point of the protocol
is that such a change cannot *land*.

## Roles

| Role | Responsibility |
|---|---|
| **Human (lead / architect)** | Sets direction and scope, defines acceptance criteria, reviews evidence, makes irreversible/outward-facing decisions, owns the result. |
| **AI agent (implementer)** | Implements one well-scoped slice at a time under the constraints below, produces the evidence, never declares its own work "done". |

The human is accountable for everything that ships. Delegating the typing does not
delegate the responsibility — agent output is treated as a *draft to be verified*, not
as a finished result.

## The hard gate (every change, no exceptions)

Before any change can land it must pass, with output captured:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check        # when dependencies change
```

This gate is mechanical and non-negotiable. "It's a small change" is not an exemption;
"the simplest approach" *includes* the full verification cycle. After editing code, the
next action is to build/test it — not to report success.

## Evidence standard

A claim of working behaviour is backed by **an executed command and its real output**.
The following are explicitly **not** evidence:

- "I read the code, it looks correct."
- Citing line numbers or that a pattern/structure "is present."
- A source review without execution.

Default status of any task or acceptance criterion is **UNTESTED**. No command run ⇒
UNTESTED ⇒ not passed. Where a result requires the running system (a restore against a
live test mailbox, a rendered UI, a measured latency), it is verified *in the running
system*, not inferred from the source.

## One vertical slice per change

Work ships as a finished, verified unit behind a single pull request — not as a pile of
half-features. There is no "partially done":

- a unit is **complete, with evidence**, or
- it is **open, with a stated blocker**.

A blocker is named honestly (an external prerequisite, a missing credential, a platform
capability) and the unit stays open. "Most of the acceptance criteria pass" is not an
acceptable closing state — every criterion is verified individually or recorded as a
blocker.

## Honest confidence

Confidence is reported with a reason and rounded *down*, never up:

- *"Unit tests pass; integration against a live account untested — 75%."*
- *"Component verified in isolation; full assembled-system run pending a display — 60%."*

Marketing language ("production-ready", "works perfectly", "fully verified") is banned
unless it is backed by executed evidence at that confidence.

## Independent second-model review

Risky or wide-blast-radius work (restore semantics, the sync state machine, anything
that mutates the cloud) gets a **fresh-eyes review from an independent model** that did
not write the code. Its output is treated as *input, not truth*: every claim it makes is
verified against the code and the gate before it changes anything. See
[`CODEX_WORKFLOW.md`](CODEX_WORKFLOW.md).

## What this protects against

- **Plausible-but-wrong code** — the most dangerous failure mode of AI assistance.
  Caught by the gate (it has to compile, lint clean, and pass tests) and by the
  evidence standard (it has to actually run).
- **Silent overclaiming** — caught by the honest-confidence rule and by requiring
  command output, so "done" always has a receipt.
- **Scope creep into the irreversible** — outward-facing or hard-to-reverse actions
  (publishing, deleting, flipping the repo public) are the human's explicit decision,
  never the agent's.

## Traceability

Every change is one Conventional-Commits PR with a description that states what was
verified and how. The risk register ([`../security/risk-register.md`](../security/risk-register.md))
records known sharp edges with honest status. Nothing is marked mitigated on the
strength of a code reading alone.
