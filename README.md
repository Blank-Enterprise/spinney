# Spinney

A lightweight Git desktop client for macOS (Apple Silicon), written in Rust with egui.

## Build

Requires Rust 1.88 or newer.

    cargo build --release
    cargo run

## Install

Requires the GitHub CLI (`gh auth login`) while the repo is private:

    gh api repos/SameetKeshav/spinney/contents/install.sh -H "Accept: application/vnd.github.raw" | bash
