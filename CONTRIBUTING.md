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
