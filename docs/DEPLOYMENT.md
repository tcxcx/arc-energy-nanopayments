# How to deploy Autarqia

Use this guide to deploy the Autarqia Rust node, the Python AirLLM worker, the Arc Testnet settlement configuration, and the Next.js operator console.

## Prerequisites

- Rust `1.85+` (`cargo`, `rustfmt`, `clippy`)
- Foundry (`forge` `1.5+`) for `contracts/ando-mppi`
- Python `3.12+` with `.venv` initialized from `workers/airllm/requirements.lock.txt`
- Node.js `20+` and `npm` for the Next.js operator console
- Access to an Arc Testnet RPC endpoint (`https://rpc.testnet.arc.network`, chain ID `5042002`)

## Verify the workspace DAG before building

Run the DAG checker to verify that no crate violates the 5-layer dependency hierarchy (`primitives -> runtime -> node -> rpc -> cli/bin`):

```bash
./scripts/verify-dag.sh
```

## Build the release binaries

Build `autarqia-node` and `autarqia-cli` in release mode:

```bash
cargo build --release --workspace
```

The compiled binaries are written to `target/release/autarqia-node` and `target/release/autarqia-cli`.

## Configure Arc Testnet and Circle Gateway x402

Autarqia ships preconfigured for the canonical Arc Testnet contracts listed in `contracts/ando-mppi/deployments/arc-testnet.json`:

- `AndoMPPI` proxy: `0xbdc5891b1372D7572046e41E1e38159c9A7fe638`
- `AndoMPPI` implementation: `0x8c709e04155eDCAA98C04eeB2c0E23D7e7C3caA4`
- Circle `GatewayWalletBatched` proxy: `0x0077777d7EBA4688BDeF3E311b846F25870A19B9`
- Arc Testnet USDC: `0x3600000000000000000000000000000000000000`

To sync the contract workspace with Arc Studio CLI once authenticated on the host machine:

```bash
arc-studio login --paste
./scripts/arc-studio-sync.sh
```

## Run the services under a process supervisor

1. Start `workers/airllm/server.py` first so it creates the `0600` Unix domain socket:

	```bash
	.venv/bin/python workers/airllm/server.py \
		--uds-path /var/run/autarqia/airllm.sock \
		--manifest-path data/models/Qwen--Qwen2.5-0.5B-Instruct/manifest.json \
		--worker-id airllm-prod-0
	```

2. Start `target/release/autarqia-node` with a dedicated RocksDB directory, BUFI JSONL sink path, and Arc Testnet RPC URL:

	```bash
	./target/release/autarqia-node \
		--listen 127.0.0.1:8787 \
		--db-path /var/lib/autarqia/rocksdb \
		--worker-uds /var/run/autarqia/airllm.sock \
		--bufi-log /var/log/autarqia/bufi-events.jsonl \
		--arc-rpc-url https://rpc.testnet.arc.network \
		--provider-key-hex "$AUTARQIA_PROVIDER_KEY_HEX" \
		--esp32-url "$AUTARQIA_ESP32_TELEMETRY_URL"
	```

## Build and start the Next.js operator console

The Next.js console proxies node health, discovery, and receipt queries through `app/api/autarqia/node/route.ts` using `AUTARQIA_NODE_URL` (default `http://127.0.0.1:8787`).

```bash
export AUTARQIA_NODE_URL="http://127.0.0.1:8787"
npm run build
npm run start
```
