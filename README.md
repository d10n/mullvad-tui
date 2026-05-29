# mullvad-tui

<img src="docs/mullvad-tui.gif" alt="demo" align="right"/>

`mullvad-tui` is a terminal user interface for Mullvad VPN that implements most of the GUI features for headless servers or l33t h4x0rs who just don't want to use a GUI. It includes mouse support.

The mullvad daemon protocol may drift across versions, and the mullvad Rust API is very strict. mullvad-tui has a compatibility shim, but it is still recommended to update mullvad-tui and the mullvad app/daemon around the same time.

<br clear="right">

## Installation
Download the latest release from the [releases page](https://github.com/d10n/mullvad-tui/releases).

Currently published packages:
* `rpm` file for Fedora/RHEL
* `deb` file for Debian/Ubuntu
* `tar.gz` file with a statically-linked binary for any other Linux distro

For Arch Linux, there are AUR packages:
* `mullvad-vpn`
* `mullvad-vpn-bin`

## Compiling from Source

### Clone

```bash
git clone --recurse-submodules https://github.com/d10n/mullvad-tui.git
cd mullvad-tui
```

The `mullvadvpn-app/` submodule is used for the daemon RPC client and types. If you cloned without `--recurse-submodules`, run `git submodule update --init --recursive`.

### Build

```bash
cargo build
```

The binary output is at `target/debug/mullvad-tui`.

### Static build

```bash
make static
```

The static binary output is at `target/crt-static/x86_64-unknown-linux-gnu/release/mullvad-tui`.

## Development

Quality gate:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

To run a focused test:

```bash
cargo test -p mullvad-tui maps_navigation_keys -- --nocapture
```

## License

* Copyright d10n <david at bitinvert dot com> and any potential future contributors (see [AUTHORS](./AUTHORS)).
* Licensed under `GPL-3.0-or-later`. See [`./LICENSES/GPL-3.0-or-later.txt`](./LICENSES/GPL-3.0-or-later.txt).
