#!/usr/bin/env bash
set -euo pipefail
export PATH="$HOME/.cargo/bin:$HOME/.foundry/bin:$HOME/.local/bin:/opt/homebrew/bin:$PATH"

WHOAMI_OUT="$(arc-studio whoami --json 2>&1 || true)"
if echo "$WHOAMI_OUT" | grep -qi "Not authenticated"; then
  echo '{"status":"auth_required","gate":"arc-studio login --paste","ando_mppi_arc_testnet_proxy":"0xbdc5891b1372D7572046e41E1e38159c9A7fe638","chain_id":5042002}'
  exit 0
fi

arc-studio contracts list --json
