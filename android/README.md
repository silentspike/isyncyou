# iSyncYou — Android client

A **standalone** Android app (#89): it **embeds the real iSyncYou engine in the app
process** (`crates/mobile` → `libisyncyou_mobile.so`) and serves the web UI over
loopback; the WebView loads `http://127.0.0.1:<port>/`. No desktop daemon and no
`adb reverse` — the phone is a self-contained iSyncYou **live companion** over mobile
data (the laptop remains the backup-of-record). A thin shell — all features live in the
web UI (`gui/webui/`), so the Kotlin stays small.

## Build (Gradle + Kotlin + cargo-ndk)

Standard Gradle project (migrated from the old manual `build.sh`, #573). Needs a
JDK (17+), an Android SDK with `build-tools;34.0.0` + `platforms;android-34` + an NDK,
the `aarch64-linux-android` Rust target and `cargo-ndk`. Point the SDK via
`local.properties` (`sdk.dir=…`, gitignored) or `ANDROID_HOME`.

```sh
./gradlew :app:assembleDebug      # -> app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

The `cargoNdkBuild` Gradle task cross-compiles the embedded engine into
`app/src/main/jniLibs/arm64-v8a/libisyncyou_mobile.so` before assembling (arm64 only for
now; multi-arch is deferred). On launch, `MainActivity` calls `NativeEngine.nativeStart`,
gets the loopback port + session token, and loads the local UI:

```sh
adb shell am start -n com.silentspike.isyncyou/.MainActivity
```

Sign in once via the in-app account menu (device-code); the engine fills a local cache
store and serves the live view. The loopback API is fully session-token gated (#89 P1).

### Release build

`./gradlew :app:assembleRelease` signs with the keystore from `android/signing.properties`
(user-supplied, gitignored: `storeFile`/`keyAlias`/`storePassword`/`keyPassword`) — see
`docs/requirements/android.yml` REQ-AND-003. `versionName`/`versionCode` come from the
`ISY_VERSION_NAME` / `ISY_VERSION_CODE` env vars (RC injection), falling back to `0.1`/`1`.

### Push (Firebase FCM)

FCM (story 2, #575) requires `app/google-services.json` (user-supplied, gitignored) and
applies the `com.google.gms.google-services` plugin. The daemon-side sender lives behind a
`PushProvider` abstraction (ADR-006), with a self-hosted ntfy/UnifiedPush alternative.

## Notes

- `applicationId = com.silentspike.isyncyou`, `minSdk 24`, `targetSdk 34`.
- `usesCleartextTraffic="true"` so the WebView can reach the daemon over plain HTTP on
  loopback / LAN; a TLS-fronted daemon would drop this.
- Hardware/gesture back navigates the WebView history before leaving the app.
- `build/`, `.gradle/`, `local.properties`, `*.apk`, keystores and credentials are gitignored.

> **Legacy:** the old Gradle-free `build.sh` (`aapt2 → javac → d8 → apksigner`) and the
> root-level `src/`/`res/`/`AndroidManifest.xml` are superseded by this Gradle project and
> kept only as a temporary reference — build via `./gradlew`.
