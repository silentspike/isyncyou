# Contributing to iSyncYou

Thanks for your interest. iSyncYou is private until RC; external contributions open up later.

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
| `dev` | `dev-checks` (`pr-dev.yml`) | fast: fmt + clippy + unit tests — quick to merge |
| `staging` | `staging-pass` (`pr-staging.yml`) | full: build/test + cargo-deny + docs (later: integration + UI snapshots) |
| `main` | `main-pass` (`pr-main.yml`) | release-grade: staging checks + release build |

Always-on: secret scan (Gitleaks), Conventional-Commit PR-title check, auto-labeling.

**Automation:**
- Merging into `dev`/`staging` auto-opens the next-stage promotion PR (`promote.yml`).
- Merging into `main` builds binaries and publishes an **RC prerelease** (`release.yml`); a `vX.Y.Z` tag publishes a full release.
- Dependabot PRs auto-merge once the branch gate passes.

CI runs on the project's **self-hosted runners** while the repo is private (no hosted-minutes usage). At public launch this switches back to GitHub-hosted runners.
