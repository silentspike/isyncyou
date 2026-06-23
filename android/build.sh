#!/usr/bin/env bash
# Build the iSyncYou Android APK without Gradle, using only the Android SDK
# build-tools (aapt2 -> javac -> d8 -> zipalign -> apksigner). A WebView client
# onto the local iSyncYou daemon's web UI.
#
# Prereqs: a JDK (17+) and an Android SDK with build-tools;34.0.0 and
# platforms;android-34. Point ANDROID_SDK at the SDK root.
#
#   ANDROID_SDK=/path/to/android-sdk ./build.sh
#   adb install -r build/iSyncYou.apk
#
# On-device test: the daemon is reached over `adb reverse tcp:8869 tcp:8869`
# (SERVER_URL in MainActivity.java defaults to http://localhost:8869/). A real
# deployment points SERVER_URL at the daemon's reachable address (LAN / VPN).
set -euo pipefail

SDK="${ANDROID_SDK:?set ANDROID_SDK to the Android SDK root}"
BT="$SDK/build-tools/34.0.0"
ANDJAR="$SDK/platforms/android-34/android.jar"
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"
mkdir -p build/classes build/gen

echo "[1/7] aapt2 compile resources"
"$BT/aapt2" compile --dir res -o build/res.zip

echo "[2/7] aapt2 link (base apk + R.java)"
"$BT/aapt2" link -o build/base.apk -I "$ANDJAR" \
  --manifest AndroidManifest.xml -R build/res.zip --java build/gen \
  --auto-add-overlay --min-sdk-version 24 --target-sdk-version 34

echo "[3/7] javac"
# shellcheck disable=SC2046
javac -source 17 -target 17 -d build/classes -classpath "$ANDJAR" \
  $(find src build/gen -name '*.java')

echo "[4/7] d8 -> classes.dex"
# shellcheck disable=SC2046
"$BT/d8" --lib "$ANDJAR" --output build $(find build/classes -name '*.class')

echo "[5/7] add classes.dex to the apk"
( cd build && zip -j base.apk classes.dex >/dev/null )

echo "[6/7] zipalign"
"$BT/zipalign" -f 4 build/base.apk build/isyncyou-aligned.apk

echo "[7/7] sign"
SIGNING_PROPS="$HERE/signing.properties"
if [ -f "$SIGNING_PROPS" ]; then
  # Release signing from the user-supplied keystore (REQ-AND-003). signing.properties
  # (gitignored) holds storeFile / keyAlias / storePassword / keyPassword. Passwords
  # are passed to apksigner via env: (never on the command line → not in `ps`), and
  # never echoed.
  # shellcheck disable=SC1090
  . "$SIGNING_PROPS"
  : "${storeFile:?signing.properties: storeFile missing}"
  : "${keyAlias:?signing.properties: keyAlias missing}"
  : "${storePassword:?signing.properties: storePassword missing}"
  : "${keyPassword:?signing.properties: keyPassword missing}"
  [ -f "$storeFile" ] || { echo "release keystore not found: $storeFile" >&2; exit 1; }
  echo "      release key (alias $keyAlias)"
  export _ISY_STOREPW="$storePassword" _ISY_KEYPW="$keyPassword"
  "$BT/apksigner" sign --ks "$storeFile" --ks-pass "env:_ISY_STOREPW" \
    --key-pass "env:_ISY_KEYPW" --ks-key-alias "$keyAlias" \
    --out build/iSyncYou.apk build/isyncyou-aligned.apk
  unset _ISY_STOREPW _ISY_KEYPW
else
  echo "      debug key (no android/signing.properties — add one for a release build)"
  if [ ! -f build/debug.keystore ]; then
    keytool -genkeypair -keystore build/debug.keystore -storepass android \
      -keypass android -alias isy -keyalg RSA -keysize 2048 -validity 10000 \
      -dname "CN=iSyncYou Debug"
  fi
  "$BT/apksigner" sign --ks build/debug.keystore --ks-pass pass:android \
    --key-pass pass:android --ks-key-alias isy \
    --out build/iSyncYou.apk build/isyncyou-aligned.apk
fi
"$BT/apksigner" verify build/iSyncYou.apk && echo "OK -> build/iSyncYou.apk"
