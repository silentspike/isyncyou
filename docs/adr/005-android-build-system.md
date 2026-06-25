# ADR-005 — Android build system & on-device auth

- **Status:** Proposed. The build-system choice is finalized when the standalone APK
  epic (R3) starts; recorded now so R2/R3 do not back into it.
- **Date:** 2026-06-22
- **Related:** ADR-003 (direct-Graph token), ADR-006 (push relay),
  [packaging / daemon model](../packaging-daemon-model.md).

---

## Context

`android/` builds **without Gradle** today: a hand-rolled `build.sh` chains
`aapt2 → javac → d8 → zipalign → apksigner`, and the app is just a hardened WebView onto
the daemon at `http://localhost:8869` (reached via `adb reverse` for testing). It signs
with a throwaway debug keystore only.

The standalone APK (R3) and push (R2) need libraries shipped as **AndroidX / AAR**
artifacts that the manual chain cannot easily consume:

- `androidx.webkit.WebViewAssetLoader` — to serve the bundled UI from an `https://`
  asset origin (valid CORS + CSP) instead of `file://` (null origin).
- the **Firebase Cloud Messaging** SDK (ADR-006).

The current app also has `usesCleartextTraffic="true"` globally, needed only for the
local-HTTP daemon.

## Decision (to finalize at R3 start)

- **Build:** introduce a **Gradle build with a pinned wrapper and pinned dependencies**
  for the standalone/release APK (AndroidX + Firebase are first-class there). The
  minimal Gradle-free build may stay for the daemon-WebView debug APK if it still earns
  its keep; otherwise it is retired. (Alternative considered: vendoring specific AARs
  into the manual build — rejected as more fragile to maintain than a pinned Gradle
  setup.)
- **On-device OAuth (RFC 8252):** authenticate via the **system browser / Custom Tabs**,
  not an embedded-WebView login. Microsoft's WebView SSO pattern is native auth +
  Authorization-header injection, not "log in inside the WebView".
- **Asset serving:** `WebViewAssetLoader` from `https://appassets.androidplatform.net`
  with `allowFileAccess=false` and the other hardened `WebSettings`.
- **Cleartext:** removed, or restricted to the daemon-debug build via a
  `network-security-config`; the standalone app talks only HTTPS (Graph).
- **CI:** new GitHub Actions must remain **SHA-pinned** (REQ-OPS-004); Android build/E2E
  runs on `ubuntu-latest` (no self-hosted on a public repo).

## Consequences

- **+** Unblocks `WebViewAssetLoader`, FCM, and a properly signed standalone APK.
- **−** A Gradle toolchain to vendor, pin, and wire into CI — real maintenance weight;
  this is why it is a deliberate decision rather than a default.

## Implementation status (2026-06-25, #89) — Accepted + partly built

- **Build system: Gradle is implemented**, not just proposed (AGP 8.5.2, Kotlin 1.9.24,
  Gradle 8.7, signing + FCM). The standalone APK adds a `cargoNdkBuild` Gradle task that
  cross-compiles the embedded Rust engine (`libisyncyou_mobile.so`) into `jniLibs` via
  cargo-ndk (NDK r27d) — arm64 first; multi-arch is deferred hardening.
- **On-device auth: device-code (not Auth-Code+PKCE/Custom-Tabs) for now.** The existing
  device-code login already works on mobile with no redirect-URI registration; PKCE is a
  later UX polish.
- **`WebViewAssetLoader` is NOT the #89 path.** Because the engine runs in-process and
  serves the UI over loopback (the same `include_str!` assets the daemon serves), the app
  needs no asset-origin loader. `usesCleartextTraffic` is removed globally and scoped to
  `127.0.0.1` via `network_security_config.xml` (Graph is HTTPS).
