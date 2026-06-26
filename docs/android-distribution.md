# Android OTA distribution (GitHub Releases + Obtainium)

**REQ-AND-006 / Epic #90 story D6.** The iSyncYou Android app ships as a signed APK
attached to every GitHub Release — no Play Store, no paid distribution channel.
Updates are delivered over-the-air by [Obtainium](https://github.com/ImranR98/Obtainium),
an open-source installer that watches a GitHub Releases feed and installs new APKs.

## The artifact

Every release carries `isyncyou-android-arm64.apk` (arm64-v8a, the only ABI shipped to
devices) plus its `.sha256`. The APK is:

- **signed** with the release keystore (CI secrets, `apksigner`-verified in the
  `android-apk` job — REQ-AND-003),
- **versioned** from the build (`versionName` = the workspace version, `versionCode` =
  the CI run number — REQ-AND-002),
- **built once** and attached to the prerelease; a stable tag reuses the byte-identical
  APK (REQ-AND-005),
- **cosign-keyless signed** and **provenance-attested** alongside the desktop artifacts,
  with its checksum in `SHA256SUMS`.

`release.yml` triggers on push to `main` and on `v*` tags, so the APK is published from
the same workflow that ships the Linux/Windows artifacts.

## Tracks

The dev→staging→main cascade (`promote.yml`) is a byte-identical tree overlay: every
change that lands integrates through dev→staging→main and reaches `main`, which publishes
an RC prerelease. There is therefore **one rolling pre-release stream**, not three
per-branch APK streams — publishing separately from dev and staging would be redundant
(identical trees). The two Obtainium-consumable tracks are:

| Track | GitHub source | Obtainium setting | Who |
|---|---|---|---|
| **stable** | version tags `vX.Y.Z` (not prereleases) | `includePrereleases = false` | most users |
| **edge** (beta/nightly) | `vX.Y.Z-rc.<run>` prereleases, auto-published on every push to `main` | `includePrereleases = true`, `fallbackToOlderReleases = true` | testers who want each integrated build |

The single edge stream fills the nightly **and** beta role: because the cascade lands
every integrated change on `main` continuously, each `main` push is both "the latest
nightly" and "the next beta". Promoting an RC to a `vX.Y.Z` tag publishes the stable track
from the byte-identical APK.

## Adding iSyncYou in Obtainium

Obtainium → **Add App** → URL `https://github.com/silentspike/isyncyou`, then set the APK
filter and prerelease toggle per the track. Or import one of the configs below
(Obtainium → **Import/Export** → **Import from JSON**).

### Stable track

```json
{
  "id": "com.silentspike.isyncyou",
  "url": "https://github.com/silentspike/isyncyou",
  "author": "silentspike",
  "name": "iSyncYou",
  "additionalSettings": "{\"includePrereleases\":false,\"fallbackToOlderReleases\":true,\"apkFilterRegEx\":\"isyncyou-android-arm64\\\\.apk$\",\"invertAPKFilter\":false,\"versionDetection\":true}",
  "overrideSource": "GitHub"
}
```

### Edge track (RC prereleases)

```json
{
  "id": "com.silentspike.isyncyou",
  "url": "https://github.com/silentspike/isyncyou",
  "author": "silentspike",
  "name": "iSyncYou (edge)",
  "additionalSettings": "{\"includePrereleases\":true,\"fallbackToOlderReleases\":true,\"apkFilterRegEx\":\"isyncyou-android-arm64\\\\.apk$\",\"invertAPKFilter\":false,\"versionDetection\":true}",
  "overrideSource": "GitHub"
}
```

The `apkFilterRegEx` `isyncyou-android-arm64\.apk$` selects the APK and excludes its
`.sha256` sidecar; `$` anchors the end so `…​.apk.sha256` never matches.

## Verifying an install

Each release publishes `SHA256SUMS` and a `isyncyou-android-arm64.apk.sha256`; the APK is
also cosign-signed (`isyncyou-android-arm64.apk.cosign.bundle`). Obtainium pins the
package signature on first install and refuses an APK signed by a different key on update,
so a swapped release cannot silently replace the app.
