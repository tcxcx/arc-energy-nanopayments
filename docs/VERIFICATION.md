# Verification matrix and commands

This document maps every requirement in the Autarqia engineering specification to its executable verification command and measured result.

## Summary of verification gates

| Gate | Command | Result |
| --- | --- | --- |
| 5-layer workspace DAG and zero-I/O purity | `./scripts/verify-dag.sh` | `valid: true`, `checked_workspace_crates: 12`, `violations: []` |
| Negative DAG fixture rejection tests | `cargo test -p autarqia-cli` | `3 passed; 0 failed` |
| Solidity `AndoMPPI` unit, fuzz, gas, and live Arc Testnet fork suite | `cd contracts/ando-mppi && RUN_ARC_FORK=true ARC_TESTNET_RPC_URL="https://rpc.testnet.arc.network" forge test` | `69 passed; 0 failed; 0 skipped` |
| Real AirLLM Python worker UDS test (`Qwen/Qwen2.5-0.5B-Instruct`) | `.venv/bin/python workers/airllm/tests/test_worker_real.py` | `PASS` (`0600` UDS, `PrefillCompleted` before `TokenDelta`, mid-run `UpdateAllowance` 3->6, stale `AdmissionEpoch` rejected) |
| 18-scenario Rust acceptance suite | `cargo test -p autarqia-node --test acceptance_suite` | `2 passed; 0 failed` (all 18 scenarios verified) |
| Release benchmark suite | `cargo test --release -p autarqia-node --test benchmark_suite -- --nocapture` | `1 passed; 0 failed` (`278 us` control plane `p50`, `5,793 ms` 4-token real AirLLM run `p50`) |
| Next.js ESLint and production build | `npm run lint && npm run build` | `0 errors`, production build succeeded |

## 18 acceptance scenarios (`bins/autarqia-node/tests/acceptance_suite.rs`)

### `acceptance_scenarios_01_02_11_14_17_real_airllm_and_proxy`

- **Scenario 1**: Happy-path streaming run over HTTP and SSE against the real Python AirLLM worker (`Qwen/Qwen2.5-0.5B-Instruct` split into 26 `.safetensors` shards), mid-run EIP-712 `UsageAck` and `AndoMPPI` `Voucher` updates, and verified `SignedUsageReceipt`.
- **Scenario 2a**: Admission-time `402 Payment Required` when initial voucher (`0` atomic) leaves prefill cost (`340` atomic) above `uncovered_exposure_cap_atomic` (`250` atomic).
- **Scenario 2b**: Mid-run `PausedPaymentRequired` halt when buyer stops acknowledging after prefill (`tokens_received <= 2`, uncovered exposure `<= 250` atomic).
- **Scenario 11**: Injecting solar drop below hard floor increments `AdmissionEpoch`.
- **Scenario 14**: Channel high-water voucher and signed `UsageReceipt` persist in RocksDB.
- **Scenario 17**: Local OpenAI-compatible MPP-I proxy (`POST /v1/chat/completions`) drives the full quote, session, SSE stream, voucher signing, and receipt verification loop transparently.

### `acceptance_scenarios_03_to_10_12_13_15_16_18_deterministic_and_arc_rpc`

- **Scenario 3**: Voucher exceeding `onchain_deposit_atomic` is rejected.
- **Scenario 4**: Onchain `topUp` reconciliation increases deposit and admits the higher voucher.
- **Scenario 5**: Out-of-order lower voucher (`1,800 < 2,500`) is rejected (`NonMonotonicVoucher`).
- **Scenario 6**: `close` transitions the channel immediately to `Closed`; `requestRefund` transitions to `Refunding` and only after `refundAvailableAt` to `Refunded`.
- **Scenario 7**: Live Arc Testnet JSON-RPC check verifies non-empty bytecode at `AndoMPPI` proxy (`0xbdc5891b1372D7572046e41E1e38159c9A7fe638`, 199 bytes), implementation (`0x8c709e04155eDCAA98C04eeB2c0E23D7e7C3caA4`, 7,867 bytes), and `GatewayWalletBatched` (`0x0077777d7EBA4688BDeF3E311b846F25870A19B9`, 223 bytes), and proves `filter_canonical_usdc_transfer_logs` strips EIP-7708 native transfer logs from `0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE`.
- **Scenario 8**: Circle Gateway x402 (`CircleGatewayX402`) session registration and run completion on an isolated rail.
- **Scenario 9**: Duplicate `(session_id, order_id)` with identical parameters is idempotent; reusing the same `order_id` with a different prompt hash returns `DuplicateOrderConflict` (`409 Conflict`).
- **Scenario 10**: `solar_surplus_only` rejects new runs when `net_surplus_mw < min_solar_surplus_mw`.
- **Scenario 12**: `solar_plus_reserved_battery` blocks new runs between `hard_floor_soc_bps` and `soft_floor_soc_bps` while allowing admitted runs to continue; dropping below `hard_floor_soc_bps` aborts active runs (`AbortedEnergyPolicy`).
- **Scenario 13**: Telemetry older than `max_staleness_ms` fails closed.
- **Scenario 15**: RocksDB reopen after dropping the store handle recovers `highest_verified_voucher_atomic`, `orders`, `receipts`, and undelivered BUFI outbox entries.
- **Scenario 16**: Reopening the node increments `WorkerEpoch` and `OwnerFence`, fencing stale worker generation tasks.
- **Scenario 18**: `autarqia-cli verify-dag` succeeds on the workspace and fails on any reverse-layer or forbidden I/O edge.
