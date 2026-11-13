# Temporary ksni zbus 6 port

This directory contains the published `ksni` 0.3.7 source package, obtained via
the configured Cargo Remote environment and verified against its Cargo.lock
checksum:

`1bcc1cb43797140f74b711f1c4a49f48a7a7429f211c5fb78264e7bcd1e99caf`

The original `Cargo.toml.orig` was used as `Cargo.toml`, and the complete
[upstream PR 48](https://github.com/iovxw/ksni/pull/48) diff, head
`93f005b9f94f3da633b3d067665349765089cc31`, was applied without conflicts.
The retained `UPSTREAM-PR-48.patch` SHA-256 is
`09babb91ac381600fe055675f05310db1d104dddea2c0fe24b3e94c900a6ad03`.
Examples and tests are retained. The original license is in `UNLICENSE`.
The Cargo registry checksum identifies the unmodified source archive. Review
this temporary copy against upstream and replace it with a released zbus
6-compatible ksni as soon as qualified.
