# iSyncYou — Linux release

Portable binaries for x86_64 Linux.

## Contents

| File | What |
|---|---|
| `isyncyou` | CLI: check / status / sync / backup / search / restore / migrate / serve |
| `isyncyoud` | engine daemon |
| `isyncyou-doctor` | standalone health/recovery checker (minimal dependencies) |
| `isyncyou.toml.sample` | documented sample configuration |
| `isyncyoud.service` | systemd `--user` unit to run the daemon as a service |
| `SHA256SUMS` | checksums of the binaries |

## Install

```sh
sudo install -m755 isyncyou isyncyoud isyncyou-doctor /usr/local/bin/
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
isyncyou serve                        # open the printed URL in your browser
```

Until interactive OAuth login lands, a Graph access token is supplied via
`--token` or the `ISYNCYOU_TOKEN` environment variable.

## Run as a service (systemd --user)

```sh
install -m755 isyncyoud ~/.local/bin/
mkdir -p ~/.config/isyncyou && cp isyncyou.toml.sample ~/.config/isyncyou/isyncyou.toml
install -Dm644 isyncyoud.service ~/.config/systemd/user/isyncyoud.service
# edit ReadWritePaths in the unit to match your sync_root + archive_root
systemctl --user daemon-reload && systemctl --user enable --now isyncyoud
```

The web UI is then served on <http://127.0.0.1:8765/>.

## Verify checksums

```sh
sha256sum -c SHA256SUMS
```
