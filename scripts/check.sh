#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

cargo fmt --all -- --check
cargo fmt --manifest-path crates/stk-gui/Cargo.toml -- --check

cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy \
    --manifest-path crates/stk-gui/Cargo.toml \
    --features desktop \
    --all-targets \
    --locked \
    -- \
    -D warnings

cargo test --workspace --all-targets --locked
cargo test \
    --manifest-path crates/stk-gui/Cargo.toml \
    --features desktop \
    --locked
