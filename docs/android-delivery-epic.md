# Epic: Android APK delivery pipeline (#90)

**Status: implemented for 1.0.** This epic's dev→staging→main RC cascade for the Android
WebView APK is built: REQ-AND-001..007 are all `implemented` (`docs/requirements/android.yml`),
realized in `release.yml` (the `android-apk` build-once job), `pr-staging.yml`
(`android-emulator-smoke`) and `docs/android-distribution.md` (Obtainium). The release
keystore + signing passwords + Firebase config are user-provided CI secrets — never
generated or committed in-repo. The FCM push end-to-end has been **verified live**
(daemon → device notification); the only remaining item is **automating that proof in
CI** (#578) — the `live_fcm_send` check is `#[ignore]` because it needs the Firebase
service-account as a CI secret (a 1.1 follow-up).

## Why
The Rust workspace already cascades dev→staging→main with `v1.0.0-rc.<run>` RC tags. The
APK (`android/`, a WebView over the daemon's embedded UI) must ride the same cascade so a
tested RC is the exact artifact users install — via **GitHub Releases + Obtainium** (decided),
not the Play Store and not any paid channel.

## Constraints (fixed)
- **ubuntu-latest only** — self-hosted runners are forbidden in the public repo.
- **SHA-pin** every new third-party action (REQ-AND-007 / REQ-OPS-004).
- **Build system:** ADR-005. Assets come from the single `include_str!` source in
  `gui/webui/src/` (the standalone bundle is R3/#80 territory).
- **Keystore** and any `google-services.json` (push, #81) are **user-provided secrets**.

## Stories
| Story | What | REQ | State |
|---|---|---|---|
| **D1** | `node --check gui/webui/src/app.js` gate + dev path-filter widened to `gui/webui/**` (the embedded web UI; `android/**` is NOT in the dev filter — the Android build runs on staging, see D1b) | — | **shipped (#87, c1e39ac)** |
| **D1b** | staging build gate: JDK17 + Android SDK + NDK + `./gradlew :app:assembleDebug` builds the APK on every staging PR (unconditional, the heavy gate dev doesn't have) | REQ-AND-001 | **implemented (staging `android-build`)** |
| **D2** | Version `versionName`/`versionCode` from the workspace version + run number (literal fallback in `build.gradle.kts` today) | REQ-AND-002 | implemented |
| **D3** | Release signing from a user-provided keystore via CI secrets (debug stays `pass:android`) | REQ-AND-003 | implemented |
| **D4** | Emulator smoke E2E (`android-emulator-runner`, AVD snapshot): APK boots + WebView loads | REQ-AND-004 | implemented |
| **D5** | RC promotion build-once-promote-many: `release.yml` builds the signed APK once, attaches it to the `v1.0.0-rc.<run>` prerelease; the tag release reuses it | REQ-AND-005 | implemented |
| **D6** | OTA: APK as a Release asset + Obtainium docs; two tracks — edge (RC prereleases) + stable (version tags). See `docs/android-distribution.md` | REQ-AND-006 | implemented |

## Release gates (per the plan)
`node --check` · `tools/check_traceability.py` · workflow-pin check · gitleaks ·
`cargo-deny` (on dependency changes). Fix the doc drift in `docs/packaging-daemon-model.md:38`
when D is implemented.

## Dependencies / sequencing
1. ADR-005 build-system decision — **done (#86)**.
2. D1b → D2 → D3 (needs the keystore secret) → D4.
3. D5/D6 after a signed APK exists.
4. R3 standalone APK (#80/#89) is a **separate** epic; D4's emulator E2E covers only the
   daemon-WebView shell until then.

## Out of scope here
Implementing the workflows (GO-gated), the standalone APK (#89, needs keystore + OAuth +
build system), and push (#81, needs `google-services.json`). Those remain their own gated
tasks; this epic is the spec they execute against.
