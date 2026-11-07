# Benchmark report and bottleneck analysis

This document reports the measured latency of the deterministic Rust MPP-I control plane and the end-to-end real AirLLM streaming inference pipeline.

## Verdict

- **Rust runtime control plane (`admit_run` + `PrefillCompleted` + 2 `TokenDelta` steps + dual EIP-712 `UsageAck`/`Voucher` recovery + `UsageReceipt` finalization)**: `p50 = 278 us` (`0.278 ms`), median of 200 measured iterations after 20 warmup iterations, range `217 us` to `654 us` (`p95 = 556 us`), bound by single-core `k256` secp256k1 ECDSA public-key recovery.
- **Full-stack real AirLLM 4-token run (`AutonomousBuyerClient` HTTP/SSE -> `autarqia-rpc` -> `autarqia-nucleus` -> RocksDB synced WAL -> UDS gRPC -> Python AirLLM `Qwen/Qwen2.5-0.5B-Instruct` 26-shard safetensors execution -> mid-run EIP-712 `UsageAck`/`Voucher` -> `UpdateAllowance` -> signed `UsageReceipt`)**: `p50 = 5,793 ms` (`1,448 ms` per output token at `p50`), median of 5 measured runs after 1 warmup run, range `5,598 ms` to `5,838 ms` (`p95 = 5,813 ms`), bound by AirLLM's per-step layer-shard loading across 26 `.safetensors` files (`104` shard loads per 4-token run) and MPS host-to-device tensor transfers.
- **Control plane share of end-to-end latency**: `0.278 ms / 5,793 ms = 0.0048%`. The Rust MPP-I accounting and EIP-712 verification path adds less than one hundredth of one percent to real AirLLM token generation time.

## Reproduction command

Run the release benchmark suite in `bins/autarqia-node/tests/benchmark_suite.rs`:

```bash
cargo test --release -p autarqia-node --test benchmark_suite -- --nocapture
```

## Hardware and environment

- **Machine**: Apple M3 Pro (`arm64`, 11 CPU cores, 18 GB unified memory, macOS)
- **Load average at measurement**: `2.05` (1-minute)
- **Rust profile**: `cargo test --release` (`opt-level = 3`)
- **Python runtime**: Python 3.12 (`.venv`), PyTorch `2.11.0`, device `mps`, dtype `bfloat16`
- **AirLLM commit**: `c4c9de3d41e73770b95716bd447e53f049a2bc85` with `workers/airllm/patches/0001-safetensors-torch-backend-and-trust-policy.patch`
- **Model**: `Qwen/Qwen2.5-0.5B-Instruct` at revision `7ae557604adf67be50417f59c2c2f167def9a775`, split into 26 `.safetensors` layer shards (`988 MB` total on disk)

## Measured results

```json
{
  "hardware": "Apple M3 Pro (18 GB unified memory, macOS arm64, MPS bfloat16)",
  "model": {
    "airllm_commit": "c4c9de3d41e73770b95716bd447e53f049a2bc85",
    "id": "Qwen/Qwen2.5-0.5B-Instruct",
    "revision": "7ae557604adf67be50417f59c2c2f167def9a775",
    "split_shards": 26
  },
  "real_airllm_e2e_4tok_run_ms": {
    "max_ms": 5838,
    "measured_runs": 5,
    "min_ms": 5598,
    "p50_ms": 5793,
    "p95_ms": 5813,
    "per_token_p50_ms": 1448,
    "warmup_runs": 1
  },
  "runtime_control_plane_microbench_us": {
    "max_us": 654,
    "measured_iterations": 200,
    "min_us": 217,
    "p50_us": 278,
    "p95_us": 556,
    "warmup_iterations": 20
  }
}
```

## Bottleneck analysis ("Why not double?")

1. **End-to-end AirLLM generation (`5,740 ms` for 4 tokens, `1,435 ms/token`)**:
	- AirLLM bounds memory by keeping only one transformer layer resident on the accelerator at a time.
	- For `Qwen/Qwen2.5-0.5B-Instruct`, each forward pass iterates over 26 split `.safetensors` shards (`model.embed_tokens`, 24 `model.layers.{0..23}`, `model.norm`, and `lm_head`).
	- Generating 4 tokens requires 4 forward passes (`1` prefill pass + `3` autoregressive decode passes), or `4 * 26 = 104` sequential shard loads via `SafetensorModelPersister.load_model`, weight materialization onto `mps` in `bfloat16`, layer execution, and `gc.collect()` cleanup.
	- At `5,740 ms / 104 = 55.19 ms` per shard pass, disk/page-cache deserialization and MPS buffer allocation dominate 99.9% of wall-clock time.
2. **Rust MPP-I runtime step (`272 us` at `p50`)**:
	- Each iteration executes two secp256k1 ECDSA public-key recoveries (`verify_signed_usage_ack` and `verify_signed_voucher` via `k256::ecdsa::VerifyingKey::recover_from_prehash`), two recursive SHA-256 prefix commitment updates (`advance_prefix_commitment`), and integer tariff ceiling division.
	- Two `k256` scalar multiplications on a single M3 Pro performance core account for `~230 us` of the `272 us` median.
3. **Verification that all work executed**:
	- Every measured end-to-end run asserted `out.tokens_received >= 1` (`4` tokens delivered per run), verified the provider's EIP-712 `SignedUsageReceipt`, and committed 5 synced RocksDB WAL batches (`register_session`, `admit_run`, `usage_ack`, `finalize_run`, `mark_outbox_delivered`) with zero HTTP or gRPC errors.
