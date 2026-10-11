# Install

kraal is a single binary for Linux (x86_64, arm64) and macOS (Apple Silicon, Intel). Linux is the
primary target; macOS passes the test suite but is not used day to day yet. Windows is not
supported.

## The install script

```sh
curl -fsSL https://raw.githubusercontent.com/kraal-sh/kraal/main/install.sh | sh
```

The script takes the archive for your machine from the
[releases](https://github.com/kraal-sh/kraal/releases) and checks it against its checksum before
installing anything, into `~/.local/bin`. Settings:

| Variable | |
|---|---|
| `KRAAL_VERSION` | a release tag such as `v0.1.0` (the latest by default) |
| `KRAAL_INSTALL_DIR` | where the binary goes (`~/.local/bin` by default) |
| `KRAAL_DOWNLOAD_URL` | where the archives are, instead of GitHub (a mirror) |

## Updates

`kraal update` replaces kraal with the latest release the same way: `--check` only says whether
there is one, `--to v0.2.0` picks one. A server already running keeps the old version until
`kraal kill`. A kraal installed by a package manager (Homebrew, Nix, your distribution) is
updated there instead.

## With Cargo

```sh
cargo install kraal                                  # the latest release, from crates.io
cargo install --git https://github.com/kraal-sh/kraal  # the main branch
```

## By hand

Each [release](https://github.com/kraal-sh/kraal/releases) has an archive per platform, its
`.sha256`, and `SHA256SUMS`. The Linux builds are static (musl), so they run on any
distribution.

## On a server

`kraal remote-install <server>` copies the running binary to `~/.local/bin` on a server of the
same OS and CPU, for [remote sessions](servers.md#remote-sessions).
