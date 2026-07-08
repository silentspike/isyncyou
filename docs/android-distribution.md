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
- **built once** and attached to the manually dispatched RC prerelease; a stable tag
  reuses the byte-identical APK (REQ-AND-005),
- **cosign-keyless signed** and **provenance-attested** alongside the desktop artifacts,
  with its checksum in `SHA256SUMS`.

`release.yml` runs for manual dispatches and `v*` tags. RC APKs are published only
when the workflow is deliberately dispatched from `main`; stable releases are
published by an explicit `vX.Y.Z` tag. The APK comes from the same workflow that ships
the Linux/Windows artifacts.

## Tracks

The dev→staging→main cascade (`promote.yml`) is a byte-identical tree overlay: every
change that lands integrates through dev→staging→main and reaches `main`. RC
prereleases are then cut deliberately from selected `main` commits, so there is
**one pre-release stream**, not three per-branch APK streams — publishing separately
from dev and staging would be redundant (identical trees). The two
Obtainium-consumable tracks are:

| Track | GitHub source | Obtainium setting | Who |
|---|---|---|---|
| **stable** | version tags `vX.Y.Z` (not prereleases) | `includePrereleases = false` | most users |
| **edge** (beta/nightly) | deliberately published `vX.Y.Z-rc.<run>` prereleases from `main` | `includePrereleases = true`, `fallbackToOlderReleases = true` | testers who want release-candidate builds before stable |

The single edge stream fills the release-candidate role: because the cascade lands
integrated changes on `main`, the owner can cut an RC from the selected current
commit without rebuilding a separate dev/staging APK stream. Promoting an RC commit
to a `vX.Y.Z` tag publishes the stable track from the byte-identical APK.

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
