# Security model and trust boundaries

Autarqia separates untrusted network buyers, the deterministic Rust accounting runtime, the local Python AirLLM worker, and the Arc Testnet settlement contracts across explicit boundaries.

## Why bounded uncovered exposure replaces unpaid post-hoc billing

Streaming inference over HTTP and Server-Sent Events cannot atomically lock an onchain voucher for every single token without blocking generation on block confirmation times. Instead, `autarqia-runtime` bounds provider credit risk at every token boundary:

```text
uncovered_exposure_atomic = max(0, cumulative_due_atomic - covered_voucher_cumulative_atomic)
uncovered_exposure_atomic <= uncovered_exposure_cap_atomic
```

The provider never exposes more than `uncovered_exposure_cap_atomic` (`250` atomic USDC units by default, or `0.000250 USDC`) of uncompensated inference work. If a buyer stops sending signed `UsageAck` and `Voucher` messages, the worker stops at the current `max_output_seq_exclusive` boundary inside `AllowanceStoppingCriteria` and transitions the run to `PausedPaymentRequired`.

## Trust boundary 1. Network buyer to `autarqia-rpc`

- **Principal**: untrusted remote HTTP client or OpenAI proxy caller.
- **Boundary**: `crates/autarqia-rpc/src/lib.rs` and `crates/autarqia-runtime/src/lib.rs`.
- **Controls**:
	- Session bearer tokens are hashed with SHA-256 before storage (`bearer_token_sha256`).
	- Every `SignedVoucher` is verified against the buyer's authorized signer under the onchain `AndoMPPI` EIP-712 domain (`chainId = 5042002`, `verifyingContract = 0xbdc5891b1372D7572046e41E1e38159c9A7fe638`).
	- Every `SignedUsageAck` is verified under the offchain `AutarqiaMPPIRun` EIP-712 domain (`version = "0"`), binding `sessionId`, `runId`, `channelId`, `acknowledgedOutputSeq`, `acknowledgedCumulativeDueAtomic`, `voucherCumulativeAmountAtomic`, and `prefixCommitment`.
	- Vouchers that decrease `cumulativeAmount` or exceed `onchain_deposit_atomic` are rejected before updating `highest_verified_voucher_atomic`.
	- Duplicate `(session_id, order_id)` submissions with different prompt hashes or token limits return `409 Conflict`.

## Trust boundary 2. `autarqia-nucleus` to the Python AirLLM worker

- **Principal**: local Python child process running `workers/airllm/server.py`.
- **Boundary**: Unix domain socket with file mode `0600` (`proto/autarqia/worker/v1/worker.proto`).
- **Controls**:
	- The worker never binds a TCP port and never holds private keys or RocksDB handles.
	- At startup, `NucleusHandle::boot` calls `Handshake` and verifies `model_id`, `model_revision`, `tokenizer_sha256`, `chat_template_sha256`, `original_manifest_sha256`, and `split_manifest_sha256` against the pinned local `ModelManifest`.
	- Every `InternalWorkAllowance` carries `(run_id, worker_epoch, owner_fence, admission_epoch, allowance_seq, max_output_seq_exclusive)`.
	- `workers/airllm/patches/0001-safetensors-torch-backend-and-trust-policy.patch` forces `trust_remote_code=False` across all Hugging Face `AutoConfig`, `AutoTokenizer`, and `GenerationConfig` loads so model repositories cannot execute arbitrary Python during load.

## Trust boundary 3. Arc Testnet dual-interface USDC and EIP-7708 logs

On Arc Testnet (`5042002`), USDC combines a 6-decimal ERC-20 interface at `0x3600000000000000000000000000000000000000` with an 18-decimal native coin balance. Native value transfers emit EIP-7708 `Transfer` logs from the system address `0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE` scaled to 18 decimals (`10^12` times larger than 6-decimal USDC atomic units).

`filter_canonical_usdc_transfer_logs` in `crates/autarqia-payments/src/lib.rs` accepts only logs whose emitter address equals `0x3600000000000000000000000000000000000000` and rejects logs emitted by `0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE`. This prevents an 18-decimal native transfer log from inflating 6-decimal channel deposit accounting by `10^12`.

## Trust boundary 4. One settlement rail per run

Each session and run binds to exactly one `SettlementRail` variant (`AndoMppiArcV1` or `CircleGatewayX402`). `autarqia-runtime` rejects any control message or settlement payload whose rail does not match the session's bound rail, preventing cross-rail double counting between `AndoMPPI` channel vouchers and Circle Gateway `TransferWithAuthorization` permits.

## Trust boundary 5. Single-writer RocksDB WAL persistence

`SingleWriterStoreHandle` in `crates/autarqia-store/src/lib.rs` serializes all mutations through a single background writer thread onto RocksDB `WriteBatch` commits with `WriteOptions::set_sync(true)`. On restart after a crash, `NucleusHandle::boot` restores the highest verified voucher per channel, all order idempotency bindings, all signed receipts, and all undelivered BUFI outbox entries before accepting traffic.
