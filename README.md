# Spinney

A lightweight Git desktop client for macOS (Apple Silicon), written in Rust with egui.

## Build

Requires Rust 1.95 or newer.

    cargo build --release
    cargo run

## Install

    curl -fsSL https://raw.githubusercontent.com/SameetKeshav/spinney/main/install.sh | bash

This installs the latest release into `/Applications` after checking it against its
published SHA-256 checksum. To install a specific release, set `SPINNEY_VERSION`:

    curl -fsSL https://raw.githubusercontent.com/SameetKeshav/spinney/main/install.sh | SPINNEY_VERSION=v1.1.0 bash

While the repo is private, the curl command can't reach it. Use the GitHub CLI
(`gh auth login`) instead:

    gh api repos/SameetKeshav/spinney/contents/install.sh -H "Accept: application/vnd.github.raw" | bash

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in Spinney, as defined in the Apache-2.0 license, is dual licensed as above,
without any additional terms or conditions.
