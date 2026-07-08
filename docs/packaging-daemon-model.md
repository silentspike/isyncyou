# Packaging & daemon model

How iSyncYou is split into shippable pieces (plan §15, §22, §24), and the current
state of each.

## Two binaries, neither needs a browser engine

| Binary | Role | Dependencies |
|---|---|---|
| `isyncyoud` | engine **daemon** — serves the local web UI/API, runs background work | pure Rust (graph/store/connectors/webui); **no webkit, no GTK** |
| `isyncyou` | full CLI (init/check/status/sync/backup/search/restore/export/migrate/serve/login) | same pure-Rust stack |
| `isyncyou-doctor` | standalone health/recovery checker | minimal (core + fs2) |

The **web UI is served by the daemon** over a localhost socket and opened in the
user's **own browser** — there is no embedded browser engine (plan §25). The
native **status bar** GUI (the windowed app, #56) uses iSyncYou's own renderer
(`tiny-skia` + `cosmic-text`, plan §24) — also **no webkit/GTK**. So the entire
shipped surface is webkit-free: the server/CLI deploy is slim and dependency-light,
and the GUI has no browser-engine runtime dependency.

## Daemon deployment (implemented)

`isyncyoud --config <toml>` validates the config, serves the web UI/API on the
default owner-only Unix socket, and logs a periodic liveness heartbeat. TCP is
available only as an explicit loopback opt-in (`--tcp --bind 127.0.0.1:8765`). It
is run as a **systemd `--user` service** (`packaging/isyncyoud.service`, hardened
per §11) so it starts on login and restarts on failure. The daemon never holds a
store's single-instance lock open (the web UI opens stores per request), so it
composes with the CLI.

Scheduled background backup/sync layers on top once the OAuth token store is wired
so the daemon can mint per-account tokens unattended (the refresh path is
implemented; only the initial device-code login needs a human — see
[`auth-token-lifecycle.md`](auth-token-lifecycle.md)).

## Distribution (implemented)

`.github/workflows/release.yml` builds release binaries on a GitHub-hosted
`ubuntu-latest` runner (self-hosted runners are forbidden in the public repo) and
bundles **`isyncyou-linux-x86_64.tar.gz`** = the three binaries + `SHA256SUMS` +
the documented `isyncyou.toml.sample` + `isyncyoud.service` + a README. The same
workflow builds the AppImage and Windows zip, builds the **signed Android APK**
once in the `android-apk` job and attaches `isyncyou-android-arm64.apk` (see
[`android-distribution.md`](android-distribution.md)), generates
`dist/isyncyou.sbom.cdx.json` from `cargo metadata --locked`, publishes a top-level
`dist/SHA256SUMS`, and requests GitHub artifact attestations for the release
archives, AppImage, Windows zip, APK, SBOM, and checksum file. Consumers can verify
the published attestation with:

```sh
gh attestation verify isyncyou-linux-x86_64.tar.gz -R silentspike/isyncyou
```

When deliberately dispatched from `main`, the workflow publishes an RC prerelease.
A `vX.Y.Z` tag publishes a full release and reuses the APK from the matching RC
commit.

## Distribution scripts (partial)

- **Flatpak / AppImage** packaging files exist under `packaging/flatpak` and
  `packaging/appimage`. Final GUI-bundle validation is still gated on the
  windowed status-bar binary, a display-capable environment, and the host tooling
  (`flatpak-builder` / `appimagetool`). The plan's Flatpak `--filesystem`
  sync-root grant is a manifest concern for that work.
- A `musl` fully-static build is possible (the target is installed) once the musl
  C toolchain (`musl-tools`) is present — the bundled SQLite needs a C compiler for
  the target.

## Build vs runtime dependencies (plan §22)

End users install nothing manually beyond the binaries: the daemon/CLI are
self-contained apart from the system C library; with the §24 own-renderer decision,
**webkit is neither a build nor a runtime dependency** of any shipped component.
