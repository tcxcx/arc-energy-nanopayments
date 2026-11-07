#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.cargo/bin:$HOME/.foundry/bin:$HOME/.local/bin:/opt/homebrew/bin:$PATH"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
cargo run --quiet -p autarqia-cli -- verify-dag --manifest-path "$REPO_ROOT/Cargo.toml"
