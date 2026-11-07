# Autarqia MPP-I protocol reference

This document specifies the wire formats, cryptographic domains, pricing equations, and state transitions for the experimental bounded-risk MPP-I profile `urn:autarqia:mppi:acknowledged-usage:0` (`0.1.0`).

## Profile constants

| Constant | Value |
| --- | --- |
| Profile identifier | `urn:autarqia:mppi:acknowledged-usage:0` |
| Profile version | `0.1.0` |
| Arc Testnet chain ID | `5042002` |
| `AndoMPPI` ERC-1967 proxy | `0xbdc5891b1372D7572046e41E1e38159c9A7fe638` |
| `AndoMPPI` implementation | `0x8c709e04155eDCAA98C04eeB2c0E23D7e7C3caA4` |
| Circle `GatewayWalletBatched` proxy | `0x0077777d7EBA4688BDeF3E311b846F25870A19B9` |
| Arc Testnet USDC ERC-20 interface | `0x3600000000000000000000000000000000000000` |
| Arc EIP-7708 native transfer system emitter | `0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE` |

## EIP-712 domains and type hashes

The protocol uses two distinct EIP-712 domains. Never sign offchain usage messages under the onchain `AndoMPPI` voucher domain.

### Onchain `AndoMPPI` voucher domain

- `name`: `"AndoMPPI"`
- `version`: `"1"`
- `chainId`: `5042002`
- `verifyingContract`: `0xbdc5891b1372D7572046e41E1e38159c9A7fe638`
- Primary type: `Voucher(bytes32 channelId,uint128 cumulativeAmount)`

Channel identifier derivation matches `AndoMPPI.sol`:

- `computeChannelId`: `keccak256(abi.encode(payer, payee, token, salt, authorizedSigner, verifyingContract, chainId))` (7 ABI words, 224 bytes).
- `computeChannelIdWithOperator`: `keccak256(abi.encode(payer, payee, token, salt, authorizedSigner, operator, verifyingContract, chainId))` (8 ABI words, 256 bytes).

### Offchain `AutarqiaMPPIRun` usage and receipt domain

- `name`: `"AutarqiaMPPIRun"`
- `version`: `"0"`
- `chainId`: `5042002`
- `verifyingContract`: `0xbdc5891b1372D7572046e41E1e38159c9A7fe638`
- Buyer acknowledgment type: `UsageAck(string sessionId,string runId,bytes32 channelId,uint32 acknowledgedOutputSeq,uint128 acknowledgedCumulativeDueAtomic,uint128 voucherCumulativeAmountAtomic,bytes32 prefixCommitment,uint64 issuedAtUnixMs)`
- Provider receipt type: `UsageReceipt(string receiptId,string sessionId,string runId,string orderId,string rail,bytes32 channelId,string modelId,bytes32 modelManifestHash,bytes32 promptSha256,uint32 inputTokens,uint32 outputTokensDelivered,uint32 outputTokensAcknowledged,uint128 cumulativeDueAtomic,uint128 coveredVoucherCumulativeAtomic,bytes32 finalPrefixCommitment,string terminalState,uint64 issuedAtUnixMs)`

## Integer tariff and bounded exposure equations

All monetary amounts use `TokenAtomicAmount` (`u128`, 6-decimal USDC atomic units). All calculations use checked integer arithmetic with ceiling division for per-1,000-token rates:

```text
prefill_due_atomic = input_tokens == 0
    ? 0
    : ceil(input_tokens * prefill_rate_per_1k_input_atomic / 1000)

decode_due_atomic = ceil(output_tokens_delivered * decode_rate_per_1k_output_atomic / 1000)

cumulative_due_atomic = session_open_fee_atomic + prefill_due_atomic + decode_due_atomic

uncovered_exposure_atomic = max(0, cumulative_due_atomic - covered_voucher_cumulative_atomic)
```

Runtime invariant:

```text
uncovered_exposure_atomic <= uncovered_exposure_cap_atomic
```

Before emitting output token `k`, `autarqia-runtime` computes `cumulative_due_atomic(input_tokens, k)`. Lookahead scheduling grants `max_output_seq_exclusive` only across output sequence numbers that satisfy the invariant under the highest verified voucher `covered_voucher_cumulative_atomic`.

## Cumulative token prefix commitment

Every output token `output_seq >= 1` updates a recursive SHA-256 chain starting from `H_0 = 0x00..00` (32 zero bytes):

```text
H_k = SHA256(
    "autarqia:prefix:v0"
    || H_{k-1}
    || u32_be(output_seq)
    || u32_be(token_id)
    || u32_be(utf8_byte_len(text_delta))
    || utf8_bytes(text_delta)
)
```

Both `UsageAck.prefix_commitment` and `UsageReceipt.final_prefix_commitment` commit to `H_k`.

## HTTP and SSE gateway endpoints

| Method and path | Request type | Response type |
| --- | --- | --- |
| `GET /.well-known/autarqia/provider.json` | None | `ProviderDiscoveryDocument` |
| `GET /v1/models` | None | OpenAI model list with `manifest_commitment` |
| `POST /v1/mppi/quotes` | `CreateQuoteRequest` | `CreateQuoteResponse` |
| `POST /v1/mppi/sessions` | `CreateSessionRequest` | `CreateSessionResponse` (`201 Created`) |
| `POST /v1/mppi/runs` | `StartRunRequest` | `StartRunResponse` (`201 Created`, `402 Payment Required`, `409 Conflict`, or `503 Service Unavailable`) |
| `GET /v1/mppi/runs/{id}/events` | None | `text/event-stream` (`RunStreamFrame` events) |
| `POST /v1/mppi/runs/{id}/control` | `RunControlCommand` | `RunControlResponse` |
| `GET /v1/mppi/runs/{id}/receipt` | None | `SignedUsageReceipt` |
| `POST /v1/chat/completions` | `OpenAiChatRequest` | OpenAI chat completion or `402 Payment Required` |
| `POST /v1/energy/telemetry` | `EnergyTelemetrySnapshot` | `NodeHealthSnapshot` |
| `GET /healthz`, `GET /readyz`, `GET /metrics` | None | `NodeHealthSnapshot` or Prometheus text |

## Worker UDS gRPC service (`autarqia.worker.v1.InferenceWorker`)

Defined in `proto/autarqia/worker/v1/worker.proto`:

- `Handshake(HandshakeRequest) -> HandshakeResponse`: verifies `model_id`, `model_revision`, and SHA-256 digests for the tokenizer, chat template, and split shard manifests.
- `Prepare(PrepareRequest) -> PrepareResponse`: applies the tokenizer chat template on the real worker and returns `prepared_input_id`, `input_token_count`, and `prompt_sha256`.
- `Generate(GenerateRequest) -> stream GenerateEvent`: runs AirLLM layer-wise generation, emitting `PrefillCompleted` before any `TokenDelta`, enforcing `InternalWorkAllowance` at each decode step via `AllowanceStoppingCriteria`, and terminating with `Completed`, `Paused`, `Cancelled`, or `Failed`.
- `UpdateAllowance(UpdateAllowanceRequest) -> UpdateAllowanceResponse`: advances `max_output_seq_exclusive` when `(worker_epoch, owner_fence, admission_epoch)` match and `allowance_seq` strictly increases.
- `Cancel(CancelRequest) -> CancelResponse`: aborts generation immediately.
- `Health(HealthRequest) -> HealthResponse`: reports worker readiness, active run count, and runtime device.
