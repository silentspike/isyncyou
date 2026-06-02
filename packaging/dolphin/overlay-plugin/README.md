# iSyncYou Dolphin overlay-icon plugin

A KDE Frameworks 6 `KOverlayIconPlugin` (KIO) that paints a sync-status emblem on
files and folders in Dolphin, by asking the running iSyncYou daemon over DBus
(service `org.silentspike.iSyncYou`, the Rust `isyncyou-dbus-status` crate).

| daemon status | emblem |
|---|---|
| `synced`   | `emblem-checked` |
| `syncing`  | `view-refresh` |
| `error`    | `emblem-error` |
| `ignored`  | `emblem-unavailable` |
| `unknown`  | *(no overlay)* |

`getOverlays()` runs on the GUI thread and must not block, so the plugin answers
from a short-TTL cache and issues an **asynchronous** DBus call, emitting
`overlaysChanged()` when the reply arrives. If the daemon is not running, no
overlay is shown — overlays are advisory; the ServiceMenu (one directory up) is
the always-available fallback.

## Build dependencies

- **openSUSE:** `extra-cmake-modules kf6-kio-devel kf6-kcoreaddons-devel qt6-base-devel gcc-c++ cmake`
- **Debian/Ubuntu:** `extra-cmake-modules libkf6kio-dev libkf6coreaddons-dev qt6-base-dev g++ cmake`

These are **build-only**; end users get the compiled plugin from the distro
package / Flatpak / AppImage and never install them.

## Build & install

```bash
cmake -S . -B build -DCMAKE_INSTALL_PREFIX=/usr -DKDE_INSTALL_USE_QT_SYS_PATHS=ON
cmake --build build
sudo cmake --install build        # -> /usr/lib64/qt6/plugins/kf6/overlayicon/isyncyouoverlay.so
kbuildsycoca6                      # refresh KDE's service cache
# restart Dolphin (or log out/in) to load the plugin
```

`-DKDE_INSTALL_USE_QT_SYS_PATHS=ON` (needs `qtpaths6`) installs into the system
Qt plugin path KIO scans. Without it the plugin lands in
`<prefix>/lib64/plugins/kf6/overlayicon/`; distro packaging normally sets the
flag, and Flatpak/AppImage use their own prefix.

To test in a user prefix without root, install into `~/.local` and point Qt at it:

```bash
cmake -S . -B build -DCMAKE_INSTALL_PREFIX="$HOME/.local"
cmake --build build && cmake --install build
export QT_PLUGIN_PATH="$HOME/.local/lib64/qt6/plugins:$QT_PLUGIN_PATH"
kbuildsycoca6 && dolphin &
```

## Runtime requirement

`isyncyoud` must be running and publishing the FileStatus service on the session
bus (it does so automatically on Linux desktops). Verify with:

```bash
gdbus call --session --dest org.silentspike.iSyncYou \
  --object-path /org/silentspike/iSyncYou/FileStatus \
  --method org.silentspike.iSyncYou.FileStatus.Status /path/to/a/synced/file
```
