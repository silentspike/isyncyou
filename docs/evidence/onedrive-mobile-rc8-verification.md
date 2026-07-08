# OneDrive Mobile RC8 Verification (#725)

Issue: [#725](https://github.com/silentspike/isyncyou/issues/725)
Epic: [#646](https://github.com/silentspike/isyncyou/issues/646)
Repo: `silentspike/isyncyou`
Report status: `IN_PROGRESS_PARTIAL_DEVICE`

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
| Runtime fix / manifest commit | `40e86cc42d60cfa31cf24b8ea249606f481153ed` |

The #725 evidence manifest is intentionally pinned to the runtime-fix commit above:
the later Row D artifacts and this Markdown report are docs/evidence-only follow-up
commits. The manifest is therefore validated without `--require-head`; that option is
reserved for generated CI manifests that are created after checkout and are expected to
match the current working tree HEAD.

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
78dc2b78cb0224e5b76fcfdda6341c2035240d803c953c3dea3d87e7cad77796  android/app/build/outputs/apk/debug/app-debug.apk
```

The Android gate artifact also keeps the original pre-fix APK checksum from the first
debug build log. The checksum above is the final Row D fix APK that was installed for
the USB device re-check.

Evidence-validator boundary: `tools/check_evidence.py` validates
`docs/evidence/sample-manifest.json` by default. The #725 manifest is separate and must
be checked explicitly with:

```sh
python3 tools/check_evidence.py --manifest docs/evidence/issue-725-manifest.json
```

Do not use `--require-head` for this tracked #725 manifest: it records the exact
runtime-fix commit whose behavior was exercised, while later committed files are
sanitized evidence artifacts and narrative report updates.

## On-Device E2E Matrix

Device evidence status: `PARTIAL`.

Fresh Pixel 8 Pro debug-APK evidence was collected under `device-lock om-725`.
Rows are deliberately conservative: a row is not promoted to `PASS` when the
artifact proves only a subset of the requested behavior.

| Row | Scope | Result | Artifact |
|---|---|---:|---|
| A | Mode 1 online browse/open; no local store write | PASS | [`device-modes-probe-raw.json`](artifacts/issue-725/device-modes-probe-raw.json) |
| B | Mode configuration and persistence | PASS | [`device-modes-probe-raw.json`](artifacts/issue-725/device-modes-probe-raw.json) |
| C | Mode 2 Sync lazy cache body path | PASS | [`device-modes-probe-raw.json`](artifacts/issue-725/device-modes-probe-raw.json), [`device-modes-files.txt`](artifacts/issue-725/device-modes-files.txt) |
| D | Mode 3 Offline materialization, airplane open, writeback, free-up guard | PASS | Materialization proved in [`device-modes-probe-raw.json`](artifacts/issue-725/device-modes-probe-raw.json) and [`device-modes-files.txt`](artifacts/issue-725/device-modes-files.txt); USB-Airplane local open proved in [`device-offline-airplane-freeup-usb.json`](artifacts/issue-725/device-offline-airplane-freeup-usb.json) and [`offline-airplane-viewer-usb.png`](artifacts/issue-725/screenshots/offline-airplane-viewer-usb.png); free-up cloud-survival proved in [`device-freeup-cloud-survival-usb.json`](artifacts/issue-725/device-freeup-cloud-survival-usb.json); sealed local edit -> mobile writeback -> Graph new version -> cleanup proved in [`device-writeback-sealed-usb.json`](artifacts/issue-725/device-writeback-sealed-usb.json) |
| E | Cloud create/rename/move/delete with biometric gate and Graph revert | PARTIAL | [`device-cloud-ops-predelete.json`](artifacts/issue-725/device-cloud-ops-predelete.json), [`device-cloud-delete-result.json`](artifacts/issue-725/device-cloud-delete-result.json), [`device-cloud-delete-graph-revert.json`](artifacts/issue-725/device-cloud-delete-graph-revert.json); the delete BiometricPrompt window was not captured |
| F | Upload/replace plus root upload | PASS | [`device-upload-replace-root-rerun.json`](artifacts/issue-725/device-upload-replace-root-rerun.json) |
| G | Share/invite with permission verification and revert | PARTIAL | Link share PASS and reverted in [`device-share-link-permission.json`](artifacts/issue-725/device-share-link-permission.json); invite live-send skipped without a controlled recipient |
| H | SAF read path | PARTIAL | [`saf-picker-uiautomator.xml`](artifacts/issue-725/saf-picker-uiautomator.xml), [`saf-preview-uiautomator.xml`](artifacts/issue-725/saf-preview-uiautomator.xml), screenshots under [`screenshots/`](artifacts/issue-725/screenshots/) |
| I | Android at-rest sentinel scan | PASS | [`android-at-rest-sentinel-scan-20260708T143922Z.log`](artifacts/issue-725/android-at-rest-sentinel-scan-20260708T143922Z.log) |
| J | No general mobile TCP data listener | PASS | [`bridge-isolation-probe.json`](artifacts/issue-725/bridge-isolation-probe.json) |
| K | #723 biometric risk catalogue re-check | PARTIAL | [`device-biometric-risk-recheck.json`](artifacts/issue-725/device-biometric-risk-recheck.json) proves offline-mode biometric gating and materialization; [`device-biometric-risk-mode-clear.txt`](artifacts/issue-725/device-biometric-risk-mode-clear.txt) proves stale mode cleanup; move-out and bulk cleanup were not completed |

Notes on Row D completion and the remaining partial rows:

- Row D was re-run with USB ADB (`3B301FDJG0020Z`). The phone entered airplane
  mode (`settings=1`, `cmd connectivity airplane-mode=enabled`), and both
  `/api/v1/body` and `/api/v1/onedrive/open` returned the offline file's local
  text while offline. The viewer screenshot was captured under airplane mode.
  Free-up evicted only the local body (`body_state=missing`, body route 404) while
  Graph metadata/content survived with the same SHA-256. A sealed local sync-root
  edit (`ISYE` magic) then produced a mobile scoped-pass report with `1 modified`;
  Graph showed the edited body at eTag `...,2`. During this probe a stale configured
  OneDrive scope returned `delta: Fatal(404)` and initially blocked the whole
  offline pass; #725 fixes that by skipping stale 404 scopes and continuing the
  remaining scopes, with a focused connector regression test. Cleanup restored the
  cloud and local body to the original content, store `sync_state=clean`, and
  `conflict_state=null`.
- Row E delete completed and Graph returned 404 for the deleted item, followed by a
  204 fixture-root cleanup. The saved `dumpsys window` poll did not catch the secure
  BiometricPrompt, so the biometric-window sub-proof remains partial.
- Row H direct `adb shell content query` was denied by Android because the provider
  requires ACTION_OPEN_DOCUMENT, which is expected. DocumentsUI displayed the
  `OneDrive/me` provider and a live file, and tapping preview reached the system
  resolver. A byte-hash read through a granted SAF URI was not captured.
- Row K captured the `mode-switch-offline-large` biometric challenge and confirmation,
  plus materialization of the fixture file. The later move-out-of-explicit-offline-scope
  probe hung at CDP receive; the fixture root was deleted, so no cloud residue remains.

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
