# iSyncYou — Android client

A minimal, hardened **WebView** app onto the local iSyncYou daemon's web UI, so
the same UI runs as a native Android app with proper touch + swipe (the web UI's
mobile mode handles content-first layout and left/right swipe navigation).

This is intentionally Gradle-free — it builds with just the Android SDK
build-tools via [`build.sh`](build.sh) (`aapt2 → javac → d8 → zipalign →
apksigner`), so there is no Gradle/AGP toolchain to vendor.

## Build

Needs a JDK (17+) and an Android SDK with `build-tools;34.0.0` +
`platforms;android-34`:

```sh
ANDROID_SDK=/path/to/android-sdk ./build.sh
adb install -r build/iSyncYou.apk
```

## Run / test on a device

The app loads `SERVER_URL` (default `http://localhost:8869/`, see
`src/.../MainActivity.java`). For a USB-connected device, map the daemon's port
to the phone first:

```sh
adb reverse tcp:8869 tcp:8869
adb shell am start -n com.silentspike.isyncyou/.MainActivity
```

A real deployment points `SERVER_URL` at the daemon's reachable address (LAN IP
or a NetBird/VPN address) — a configurable server field is the natural next step.

## Notes

- `package=com.silentspike.isyncyou`, `minSdk 24`, `targetSdk 34`.
- `usesCleartextTraffic="true"` so the WebView can reach the daemon over plain
  HTTP on the local loopback / LAN; a TLS-fronted daemon would drop this.
- Hardware/gesture back navigates the WebView history before leaving the app.
- Build artifacts (`build/`, `*.apk`, `*.keystore`) are git-ignored.
