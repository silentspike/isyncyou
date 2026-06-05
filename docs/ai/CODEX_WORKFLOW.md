# Independent Second-Model Review

A companion to the [AI-Assisted Engineering Protocol](AI_ASSISTED_ENGINEERING_PROTOCOL.md).
It describes how an **independent model** — one that did not write the change — is used
as a second engineer and adversarial reviewer, and why its output is never trusted on
sight.

The implementing agent here is one model; the reviewer is a *different* one (the file is
named for the Codex CLI that fills this role in practice, but the workflow is
model-agnostic and more than one reviewer is used when a decision is important).

---

## Why a second model

A single agent shares its own blind spots end to end: the same wrong assumption that
produced a bug will happily wave it through on self-review. An independent model has
*different* blind spots, so it catches a different class of error — especially the
"confidently reasonable but actually wrong" design choice that no compiler will flag.

This is most valuable exactly where it is most dangerous:

- **Restore semantics** — idempotency, the operation-ledger state machine, what happens
  on a crash between a Graph write and the local record.
- **The sync state machine** — conflict resolution, delete propagation, `410`
  reconciliation.
- **Anything that mutates the cloud** or could lose data.

## How it is used

1. **Frame the question without leaking the answer.** The reviewer is given the design
   or the diff and asked to find what is wrong — not asked to confirm a conclusion. For
   a fresh design review it is given *no* prior context, so its assessment is genuinely
   independent ("first-principles second opinion").
2. **Capture the output to a file.** Reviewer runs are logged to a working file (kept
   out of the repository — see the repo's `.gitignore`), so the full reasoning can be
   re-read, not just a summary.
3. **Treat the output as input, not truth.** This is the rule that matters. The reviewer
   hallucinates too. Every claim it makes is checked against the actual code and against
   the [hard gate](AI_ASSISTED_ENGINEERING_PROTOCOL.md#the-hard-gate-every-change-no-exceptions)
   before it changes anything. A reviewer assertion is a *candidate finding*, not a fact.
4. **Triangulate when it matters.** For an important decision, more than one independent
   model is asked separately. Where they disagree, the disagreement itself is the signal:
   it points at an unstated assumption that needs to be made explicit and tested.
5. **Findings become tracked work.** A confirmed finding turns into a scoped task or a
   risk-register entry, picked up under the same one-slice-per-PR, evidence-gated process
   as any other change. Nothing the reviewer says is auto-applied.

## Adversarial framing

The reviewer is pointed at failure, not success: *"try to refute this", "where does this
duplicate data on a crash?", "what does this delete that it shouldn't?"* Defaulting a
verdict to "broken until proven safe" surfaces more real problems than asking "does this
look fine?", which biases toward agreement.

## Honest limits

- A second model is a **second opinion, not an oracle.** It reduces the rate of
  plausible-but-wrong changes; it does not certify correctness. Only the gate and live
  verification do that.
- Independent review **does not replace tests.** A finding is closed by a test or a
  live-system check, not by a model agreeing it was fixed.
- Reviewer transcripts are development artifacts and stay out of the repository; what
  lands is the verified change, its tests, and — where relevant — a risk-register entry.
