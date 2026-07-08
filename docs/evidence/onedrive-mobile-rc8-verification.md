# OneDrive Mobile RC8 Verification (#725)

Issue: [#725](https://github.com/silentspike/isyncyou/issues/725)  
Epic: [#646](https://github.com/silentspike/isyncyou/issues/646)  
Repo: `silentspike/isyncyou`  
Report status: `IN_PROGRESS`

This report records the #725 release-governance close-out evidence. It is intentionally
strict: a row is `PASS` only when backed by a committed artifact or a linked GitHub run.
Rows that still need physical-device or release-pipeline proof remain `PENDING`.

## Gate

| Check | Result | Evidence |
|---|---:|---|
| #718 | PASS | `gh issue view` returned CLOSED |
| #719 | PASS | `gh issue view` returned CLOSED |
| #720 | PASS | `gh issue view` returned CLOSED |
| #721 | PASS | `gh issue view` returned CLOSED |
| #722 | PASS | `gh issue view` returned CLOSED |
| #723 | PASS | `gh issue view` returned CLOSED |
| #724 | PASS | `gh issue view` returned CLOSED |
| #725 | OPEN | This close-out issue remains open during evidence collection |
| #646 | OPEN | Epic remains open until #725 ACs and release proof are complete |
| Open PRs | PASS | `gh pr list --state open` returned `[]` |
| Open `promote/*` PRs | PASS | Promote-filter query returned no rows |
| CodeQL on current `main` | PASS | Run #221 on `fdc0eb4` completed successfully, including `Analyze (java-kotlin)` |

Branch/commit snapshot at host-gate collection:

| Ref | SHA |
|---|---|
| `origin/dev` | `14e819d8e182a6f440e4c255692e0545cd8c1b0d` |
| `origin/staging` | `12d700904bfc86a39463aa00c50488a8b2ed9505` |
| `origin/main` | `fdc0eb4d7a6839b97c9fd639ac5ba66514e401a7` |
| Evidence branch head before this report | `b289ba7d3a4e5f3476bba93592f8ddf5a05b5a97` |

## Release Pipeline

Current status: `PENDING`.

The latest completed `release.yml` run before #725 evidence was run #81, which was
`cancelled` on commit `d44320126fdd1a62e307ac3f40dc123f9cdffd91`. The latest published
prerelease was `v1.0.0-rc.80` from 2026-06-27, so it is not evidence for the current
RC8 tree.

Required before this section can pass:

- merge #725 through `dev -> staging -> main` after the local evidence commit;
- manually dispatch `release.yml` from `main`;
- verify the new `v1.0.0-rc.<run>` prerelease, tag target, assets, checksums,
  cosign bundles, and attestations.

## Host And Android Gates

| Gate | Result | Artifact |
|---|---:|---|
| `cargo +1.95.0 fmt --all --check` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `node --check gui/webui/src/app.js` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `cargo +1.95.0 clippy --workspace --all-targets -- -D warnings` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `cargo +1.95.0 test --workspace --no-fail-fast` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `python3 tools/check_traceability.py` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `python3 tools/check_evidence.py` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `python3 tools/check_workflow_pins.py` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `git diff --check origin/dev...HEAD` | PASS | [`host-gates.txt`](artifacts/issue-725/host-gates.txt) |
| `./gradlew :app:testDebugUnitTest :app:compileDebugKotlin :app:assembleDebug` | PASS | [`android-debug-build.txt`](artifacts/issue-725/android-debug-build.txt) |

Debug APK SHA256 from the local Android gate:

```text
8f2c3f23e3f7b2cbde7342f24b8211ec6d2b02a65fba50eaee047599744915f7  app/build/outputs/apk/debug/app-debug.apk
```

Evidence-validator boundary: `tools/check_evidence.py` validates
`docs/evidence/sample-manifest.json` by default. The #725 manifest is separate and must
be checked explicitly with:

```sh
python3 tools/check_evidence.py --manifest docs/evidence/issue-725-manifest.json
```

## On-Device E2E Matrix

Device evidence status: `PENDING`.

| Row | Scope | Result | Artifact |
|---|---|---:|---|
| A | Mode 1 online browse/open; no local store write | PENDING | pending |
| B | Mode configuration and persistence | PENDING | pending |
| C | Mode 2 Sync lazy cache body path | PENDING | pending |
| D | Mode 3 Offline materialization, airplane open, writeback, free-up guard | PENDING | pending |
| E | Cloud create/rename/move/delete with biometric gate and Graph revert | PENDING | pending |
| F | Upload/replace plus root upload | PENDING | pending |
| G | Share/invite with permission verification and revert | PENDING | pending |
| H | SAF read path | PENDING | pending |
| I | Android at-rest sentinel scan | PENDING | pending |
| J | No general mobile TCP data listener | PENDING | pending |
| K | #723 biometric risk catalogue re-check | PENDING | pending |

## Release Artifact Verification

Status: `PENDING`.

The final RC artifact verification must record:

- `release.yml` run number, URL, event, head branch, and head SHA;
- `v1.0.0-rc.<run>` release URL and prerelease flag;
- tag target equals `RC_COMMIT`;
- asset list includes Linux tarball, AppImage, Windows zip, Android APK, SBOM,
  `SHA256SUMS`, and the expected cosign bundles;
- `.sha256` sidecars are not expected to have their own `.cosign.bundle` files;
- APK checksum matches the release asset and release smoke result.

## Issue And Epic Close-Out

Status: `PENDING`.

#725 and #646 must stay open until all #725 ACs pass or are explicitly waived with a
linked owner decision. No close-out is claimed by this in-progress report.

