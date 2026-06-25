# Supply-chain & SecDevOps across the pipeline

This repository runs its DevOps and SecDevOps disciplines **distributed across every
branch with a different intensity** — cheap and fast on `dev`, heaviest on `staging`
(the gate before production), release-grade on `main`, and signed + attested at the
`RC` boundary. This document is the map of that distribution and of the artifact
lifecycle that backs it.

> **No MLOps here, honestly.** iSyncYou ships no machine-learning model, so there is no
> model training/serving/monitoring lifecycle to govern. What plays the equivalent
> role is **artifact-lifecycle / supply-chain governance** — SBOM → vulnerability scan
> → keyless signing → SLSA provenance → attestation. It is labelled as SecDevOps /
> supply-chain, not as MLOps theatre.

## Branch model

```
dev  ──(pr-dev.yml)──►  staging  ──(pr-staging.yml + codeql.yml)──►  main  ──►  RC (release.yml)
        fast gate            heaviest pre-prod gate                  release-grade   signed + attested
```

A change is opened as a single PR into `dev`. On merge, `promote.yml` cascades it
automatically: it cuts a promotion branch from the **target** and overlays the
**source tree** (so the promoted tree is byte-identical to the source), opens an
auto-merging PR, and lets the target branch's gate run. `dev → staging → main`, then a
push to `main` triggers `release.yml`, which publishes the RC pre-release.

## Distribution of disciplines

| Discipline | dev | staging | main | RC |
|---|---|---|---|---|
| Build / test | fmt · clippy · unit · msrv · js-check | + deploy E2E · Android APK · release build | release build | multi-platform release artifacts |
| SAST | clippy · cargo-deny · **Semgrep** (JS/Kotlin/secrets) | **CodeQL** (rust + javascript-typescript + java-kotlin) | CodeQL | — |
| DAST | — | **OWASP ZAP baseline** vs the served UI | — | — |
| Supply-chain | cargo-deny · dependency-review | wrapper-validation · cargo-deny · **CycloneDX SBOM** | cargo-deny · **trivy** | **SBOM · trivy · cosign · SLSA provenance · attestation** |
| Secret scanning | gitleaks | gitleaks | gitleaks | gitleaks |
| Coverage | 75% line gate (`coverage.yml`) | — | — | — |
| Quality / governance | traceability · language · PR-title · actionlint | traceability · language | traceability · language | release notes |

Rust SAST is owned by clippy (`-D warnings`), cargo-deny advisories, and CodeQL on
staging/main; Semgrep's Rust support is only experimental, so on `dev` it runs the OSS
rulesets for the hand-written web UI (JavaScript) and the Android app (Kotlin) plus a
secrets sweep. trivy runs **vulnerability-only** — gitleaks already owns secret
scanning repo-wide, and a local trivy run showed the release binary and the APK carry
no language-specific files, so they are not scanned; the value is the Cargo
lockfile / SBOM vulnerability check, a different engine than cargo-deny.

## Artifact lifecycle

1. **SBOM (staging + RC).** `tools/generate_sbom.py` produces a **CycloneDX SBOM of the
   Rust/Cargo dependency graph** (`cargo metadata --locked`, `pkg:cargo/*` PURLs). It is
   the Cargo dependency SBOM, **not** a whole-product SBOM — the Gradle/Android
   dependencies and the CI-only npm tree are not included (a Gradle SBOM is future
   work). Surfaced on staging; regenerated and attested at release.
2. **Vulnerability scan (main + RC).** trivy scans the Cargo lockfile / SBOM and fails
   the gate on HIGH/CRITICAL findings.
3. **Signing & provenance (RC only).** `release.yml` signs the release artifacts with
   **cosign keyless / Sigstore** (Fulcio cert + Rekor transparency entry, no long-lived
   key), generates **SLSA build provenance**, and attests both the artifacts and the
   SBOM. The runner is locked down with `harden-runner` (egress audit) and runs only on
   GitHub-hosted infrastructure.

## What is signed, and what is not

This distinction is deliberate and must not be overstated:

- **Release artifacts are signed and attested.** Each published artifact carries a
  cosign keyless signature and SLSA provenance / SBOM attestation, verifiable with
  `cosign verify-blob`.
- **Promotion is automated, not cryptographically signed.** `promote.yml` performs a
  plain bot commit (`git commit` — **no** `-S`/GPG signing) and an auto-merge driven by
  a fine-grained PAT. The integrity of a promotion rests on the **tree-overlay
  equality** (the promoted tree is byte-identical to the already-gated source) and on
  the target branch's required status checks — not on a commit signature.

## Why GitHub-hosted, not self-hosted CI

CI runs exclusively on GitHub-hosted `ubuntu-latest` runners. For a **public** repo
this is a security decision: self-hosted runners (and a self-hosted CI controller such
as Jenkins) would let a fork PR execute arbitrary code on private infrastructure.
GitHub-hosted runners are ephemeral and isolated per job. Every action is pinned by
commit SHA, every container by digest, and every external tool install is
checksum-verified; workflow `permissions` are least-privilege by default.

## Required status checks (branch protection)

A workflow job only blocks a merge once it is registered as a **required status check**
in branch protection — adding a job does not make it required. The checks that must be
required per branch:

- **dev:** `semgrep` (plus the existing dev checks).
- **staging:** the `staging-pass` aggregator (covers the staging-e2e / DAST / release /
  Android jobs) and the three CodeQL checks `Analyze (rust)`,
  `Analyze (javascript-typescript)`, `Analyze (java-kotlin)`.
- **main:** the `main-pass` aggregator (covers `vuln-scan`) and the three CodeQL checks.

CodeQL runs in its own workflow and therefore cannot feed an aggregator job; its three
language checks are wired as required checks directly.
