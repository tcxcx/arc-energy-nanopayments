# How to operate an Autarqia node

Use this guide to prepare the pinned AirLLM model, start the Python inference worker, run `autarqia-node`, execute paid inference runs, and recover from energy or process faults.

## Prepare the local AirLLM model shards

Before starting the worker, split the pinned Hugging Face model into layer-wise safetensors shards and write the integrity manifest.

1. Create the Python virtual environment and install the locked dependencies:

	```bash
	python3 -m venv .venv
	.venv/bin/pip install -r workers/airllm/requirements.lock.txt
	```

2. Run `workers/airllm/prepare_model.py` against `Qwen/Qwen2.5-0.5B-Instruct` at revision `7ae557604adf67be50417f59c2c2f167def9a775`:

	```bash
	.venv/bin/python workers/airllm/prepare_model.py \
		--model-id Qwen/Qwen2.5-0.5B-Instruct \
		--revision 7ae557604adf67be50417f59c2c2f167def9a775 \
		--output-root data/models/Qwen--Qwen2.5-0.5B-Instruct
	```

3. Confirm that `data/models/Qwen--Qwen2.5-0.5B-Instruct/manifest.json` exists and lists 26 `.safetensors` layer files under `splitted_model/`.

## Start the Python AirLLM gRPC worker

The worker listens only on a local Unix domain socket with file mode `0600`.

1. Start `workers/airllm/server.py`:

	```bash
	.venv/bin/python workers/airllm/server.py \
		--uds-path /tmp/autarqia-airllm.sock \
		--manifest-path data/models/Qwen--Qwen2.5-0.5B-Instruct/manifest.json \
		--worker-id airllm-local-0
	```

2. Check the socket permissions if you need to confirm local isolation:

	```bash
	stat -f "%Lp %N" /tmp/autarqia-airllm.sock
	```

	The mode must be `600`.

## Start the Rust provider node

`autarqia-node` opens the RocksDB ledger store, connects to the AirLLM worker over the Unix domain socket, reconciles Arc Testnet settlement state, and serves the HTTP and SSE gateway.

1. Run `autarqia-node`:

	```bash
	cargo run --release -p autarqia-node -- \
		--listen 127.0.0.1:8787 \
		--db-path data/rocksdb \
		--worker-uds /tmp/autarqia-airllm.sock \
		--bufi-log data/bufi-events.jsonl \
		--arc-rpc-url https://rpc.testnet.arc.network
	```

2. Query `/healthz` and `/readyz` to verify that the node and the worker handshake succeeded:

	```bash
	curl -s http://127.0.0.1:8787/healthz | jq .
	curl -s http://127.0.0.1:8787/readyz | jq .
	```

## Run an autonomous paid inference session

Use `autarqia-cli` to request a quote, open an `ando_mppi_arc_v1` or `circle_gateway_x402` session, stream tokens over SSE, sign sequential `UsageAck` and `Voucher` messages, and verify the final `UsageReceipt`.

1. Execute a direct buyer run:

	```bash
	cargo run --release -p autarqia-cli -- buyer-run \
		--node-url http://127.0.0.1:8787 \
		--rail ando-mppi-arc-v1 \
		--prompt "Explain solar microgrids in one sentence." \
		--max-output-tokens 16
	```

2. To route unmodified OpenAI SDK clients through the MPP-I payment loop, start the local proxy:

	```bash
	cargo run --release -p autarqia-cli -- openai-proxy \
		--listen 127.0.0.1:8788 \
		--node-url http://127.0.0.1:8787
	```

3. Send a standard OpenAI chat completion request to `http://127.0.0.1:8788/v1/chat/completions`.

## Handle an energy drop or telemetry outage

When solar power or battery state of charge drops below the active policy threshold, `autarqia-runtime` increments `AdmissionEpoch` and cancels active runs according to the configured `EnergyMode`.

1. Push an updated telemetry snapshot to `/v1/energy/telemetry` or restore the ESP32 telemetry feed:

	```bash
	curl -s -X POST http://127.0.0.1:8787/v1/energy/telemetry \
		-H "content-type: application/json" \
		-d '{
			"solar_power_mw": 55000,
			"battery_power_mw": 12000,
			"load_power_mw": 14000,
			"grid_power_mw": 0,
			"battery_soc_bps": 8500,
			"panel_voltage_mv": 18600,
			"panel_current_ma": 2956,
			"temp_mc": 28000,
			"measured_at_monotonic_ms": 0,
			"source": "operator_override"
		}' | jq .
	```

2. Wait for `hold_down_ms` to expire before submitting a new run after an energy state change.

## Recover from an unclean node restart

`autarqia-store` persists channel high-water vouchers, order idempotency records, run receipts, and undelivered BUFI outbox entries in RocksDB with `WriteOptions::set_sync(true)`.

1. Restart `autarqia-node` with the same `--db-path` and `--bufi-log` arguments.
2. On startup, `NucleusHandle::boot` loads the latest durable checkpoint, increments `WorkerEpoch` and `OwnerFence` so stale worker streams cannot resume, and flushes any undelivered BUFI outbox entries to the JSONL sink.
3. Query `/v1/mppi/runs/{id}/receipt` to confirm that receipts committed before the crash remain available.
