# iSyncYou — Linux release

Portable binaries for x86_64 Linux.

## Contents

| File | What |
|---|---|
| `isyncyou` | CLI: check / status / sync / backup / search / restore / migrate / serve |
| `isyncyoud` | engine daemon |
| `isyncyou-doctor` | standalone health/recovery checker (minimal dependencies) |
| `isyncyou-statusbar` | native status tray and browser launcher |
| `org.silentspike.iSyncYou.desktop` | desktop application launcher |
| `isyncyou.toml.sample` | documented sample configuration |
| `isyncyoud.service` | systemd `--user` unit to run the daemon as a service |
| `SHA256SUMS` | checksums of the binaries |

## Install

```sh
sudo install -m755 isyncyou isyncyoud isyncyou-doctor isyncyou-statusbar /usr/local/bin/
install -Dm644 org.silentspike.iSyncYou.desktop ~/.local/share/applications/org.silentspike.iSyncYou.desktop
```

(Or run them in place — they are self-contained apart from the system C library.)

## Configure

```sh
cp isyncyou.toml.sample isyncyou.toml
$EDITOR isyncyou.toml          # set your account(s), sync_root and archive_root
isyncyou check --config isyncyou.toml
```

## Use

```sh
isyncyou backup --account primary    # index + archive mail/calendar/contacts/todo/onenote
isyncyou search --account primary --query invoice
isyncyou serve                        # local API on the default owner-only Unix socket
isyncyou serve --tcp                  # optional browser/TCP transport, loopback-only
```

Use the account menu in the browser UI to connect an account.

## Run as a service (systemd --user)

```sh
install -m755 isyncyoud ~/.local/bin/
mkdir -p ~/.config/isyncyou && cp isyncyou.toml.sample ~/.config/isyncyou/isyncyou.toml
install -Dm644 isyncyoud.service ~/.config/systemd/user/isyncyoud.service
# edit ReadWritePaths in the unit to match your sync_root + archive_root
systemctl --user daemon-reload && systemctl --user enable --now isyncyoud
```

The desktop service exposes the local UI at `http://127.0.0.1:8765/`; launch
the tray using the application menu. For a non-desktop Unix-socket deployment,
remove the `--tcp --bind` options and use an appropriate local socket client.

## Verify checksums

```sh
sha256sum -c SHA256SUMS
```
