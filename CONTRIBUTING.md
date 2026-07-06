# Contributing to iSyncYou

Thanks for your interest. iSyncYou is public and at the release-candidate stage.
**Bug reports and issues are welcome from anyone.** All coding, however, is done by the
silentspike org — **external pull requests are not accepted**; please open an issue
instead. The workflow below documents how the org develops the project.

## Ground rules

- **Language:** all repository content is **English** (issues, PRs, commits, docs, code comments).
- **Conventional Commits** for commit messages and PR titles (`feat:`, `fix:`, `docs:`, `refactor:`, `ci:`, `chore:`, `deps:`).
- **No secrets** in the repo. A Gitleaks scan runs in CI and locally via the pre-commit hook.
- **No GPL code** copied in. Techniques may be re-implemented; the `cargo-deny` license gate enforces permissive-only dependencies.

## Local setup

```sh
git config core.hooksPath .githooks   # enable format/secret/lint hooks
just check                            # fmt-check + clippy + tests (the pre-push gate)
```

## Issue model

Work is tracked as **Epic → Story → Task** (`E-`/`S-`/`T-` IDs). Tasks carry acceptance criteria, dependencies (Depends On / Blocking) and a testing strategy. See the issue templates.

## Verification

No task is "done" without evidence: command + output, or a headless UI snapshot, or a passing test against fixtures / the dedicated test account.

## Branch model & CI

Promotion flows `feature → dev → staging → main`, each with its own gate:

| Branch | PR gate (workflow) | Scope |
|--------|--------------------|-------|
| `dev` | `dev-checks` (`pr-dev.yml`) | fast shift-left: fmt + clippy + unit tests + MSRV + JS parse-check + Semgrep SAST + cargo-deny + 75% coverage gate |
| `staging` | `staging-pass` (`pr-staging.yml`) + CodeQL | heaviest, pre-prod: token-free deploy + end-to-end UI smoke, Android build, CodeQL (Rust/JS/Kotlin), OWASP ZAP DAST, CycloneDX SBOM, release build |
| `main` | `main-pass` (`pr-main.yml`) + CodeQL | release-grade: full build + Trivy vulnerability scan + CodeQL |

Always-on (required on every PR into `dev`/`staging`/`main`): secret scan (Gitleaks),
Conventional-Commit PR-title check, the **English-only language check** (`language-check`),
and auto-labeling. The language check fails with the offending `file:line` and how to
resolve it, so a non-English change is caught, fixed, and re-submitted; legitimate
non-English (locale files, encoding/MIME test fixtures) is allowlisted in
`tools/lang_allowlist.txt` or with an inline `lang-allow` marker.

**Automation:**
- Merging into `dev`/`staging` auto-opens **and auto-merges** the next-stage promotion PR (`promote.yml` — a PAT-driven tree-overlay so the promoted tree is byte-identical to its source) once the target's required checks pass, so `dev → staging → main` cascades hands-off.
- Merging into `main` runs the release-grade branch gate and keeps `main` current; release artifacts are published only by an explicit release workflow/tag, not by the ordinary `dev → staging → main` promotion cascade.
- Dependabot PRs auto-merge once the branch gate passes.

CI runs entirely on **GitHub-hosted runners** — no self-hosted runner touches this repo (a self-hosted runner would live on private infra and be an attack surface on a public repo; see the note in `release.yml`).

## Working in parallel (multi-agent)

Multiple agents may work concurrently — each on its own feature branch (and, ideally,
its own git worktree) — and open PRs into `dev` independently. Branch protection on
`dev`/`staging`/`main` is `strict` (a PR must be up to date with its base before
merging), so land one PR at a time per base and rebase the next; this keeps every
merge tested against the exact tree it lands on.

One agent acts as the **orchestrator**. It owns review (see `.github/CODEOWNERS`) and
gates promotion to `main`: feature work flows `feature → dev → staging → main`, and the
orchestrator decides when a `staging → main` promotion is ready and performs that merge.
The automated gates (build/lint/test, `cargo-deny`, requirements + evidence, secret
scan, and the English-only `language-check`) are the objective bar every change must
clear; the orchestrator is the human-in-the-loop judgement on top of them.

### Merge strategy

- **`feature → dev`: squash-merge** — one tidy commit per change on `dev`.
- **`dev → staging` and `staging → main`: squash-merge** — the repository allows
  squash merges only (merge commits and rebase merges are disabled in repo settings), so
  each promotion lands as a single squash commit on the target branch. The three
  branches therefore have independent histories with the same *content*; reconcile by
  promoting forward (a promotion PR's diff is the content delta), not by expecting shared
  SHAs.
- **Promotion is automated** end-to-end: `promote.yml` opens and auto-merges the
  `dev → staging` and `staging → main` tree-overlay PRs via a fine-grained PAT
  (`PROMOTE_TOKEN`), once the target's required checks pass. The orchestrator stays the
  human-in-the-loop for `feature → dev`, and can intervene at any hop. When a promotion's
  gate fails for a runner-infra reason (not content), the orchestrator may admin-merge a
  content-verified promotion; `main` keeps `enforce_admins`, so that requires a
  deliberate, restored protection toggle.

### Review policy (solo-merge is intentional)

Branch protection on `dev`/`staging`/`main` sets `required_approving_review_count = 0`:
a single maintainer may merge their own PR. This is **deliberate** for a
single-maintainer repository — a human-approval requirement that only the same person
can satisfy adds ceremony, not safety.

The compensating control is the **automated required-checks gate**, which every PR must
pass before it can merge and which a self-approval cannot bypass: build/lint/test
(`dev-checks`/`staging-pass`/`main-pass`), `cargo-deny`, the requirements + evidence
traceability check, the secret scan (Gitleaks), the Conventional-Commit title check, and
the English-only `language-check`. `main` additionally has `enforce_admins` so even the
maintainer cannot push past the gate. When the project gains additional maintainers, set
`required_approving_review_count = 1` to require cross-review.
