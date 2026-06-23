# iSyncYou — Android client

A minimal, hardened **WebView** app onto the local iSyncYou daemon's web UI, so the
same UI runs as a native Android app with proper touch + swipe. A thin shell — all
features live in the web UI (`gui/webui/`), so this stays small.

## Build (Gradle + Kotlin)

Standard Gradle project (migrated from the old manual `build.sh`, #573). Needs a
JDK (17+) and an Android SDK with `build-tools;34.0.0` + `platforms;android-34`.
Point the SDK via `local.properties` (`sdk.dir=…`, gitignored) or `ANDROID_HOME`.

```sh
./gradlew :app:assembleDebug      # -> app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

On-device the daemon is reached over `adb reverse tcp:8869 tcp:8869` (`SERVER_URL` in
`MainActivity.kt` defaults to `http://localhost:8869/`):

```sh
adb reverse tcp:8869 tcp:8869
adb shell am start -n com.silentspike.isyncyou/.MainActivity
```

A real deployment points `SERVER_URL` at the daemon's reachable address (LAN IP /
NetBird VPN).

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
