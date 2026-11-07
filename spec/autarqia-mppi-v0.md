# Autarqia MPP-I Profile Specification v0 (`urn:autarqia:mppi:acknowledged-usage:0`)

- **Profile Identifier:** `urn:autarqia:mppi:acknowledged-usage:0`
- **Schema Version:** `0.1.0`
- **Status:** Experimental bounded-risk profile for real-time metered inference over Arc.
- **Attribution:** Adapts concepts from the MPP-I Paper v1.0 (Jamie / Alethieum, DOI `10.5281/zenodo.21017440`, CC-BY-4.0) and interoperates at the contract ABI boundary with `AndoMPPI` v1 (`Ando-OSS/mpp-i`, Apache-2.0). The HTTP routes, JSON/Protobuf schemas, offchain EIP-712 domain (`AutarqiaMPPIRun`), and bounded-exposure state machine defined in this document are Autarqia specifications, not upstream normative standards.

## 1. Conformance Separation

Implementations must distinguish three independent claims:

1. **AndoMPPI v1 ABI Interoperability:** Exact byte-level compatibility with `AndoMPPI.sol` (`computeChannelId`, `computeChannelIdWithOperator`, EIP-712 `Voucher(bytes32 channelId,uint128 cumulativeAmount)` under domain `AndoMPPI` version `1`, `open`, `openWithOperator`, `topUp`, `settle`, `settleBatch`, `close`, `requestClose`, `withdraw`).
2. **Autarqia Profile Compatibility:** Interoperability between `autarqia-node` and the Autarqia autonomous buyer SDK/CLI under `urn:autarqia:mppi:acknowledged-usage:0`.
3. **Paper Aspirations vs. Bounded-Risk Reality:** `AndoMPPI` v1 settles only an exact signed `cumulativeAmount` voucher and does not verify model execution or allow partial redemption of a higher-cap voucher. Therefore, `CreditGrant` is an offchain future spending permission, not a collectible onchain voucher. Value becomes collectible onchain only when the buyer signs an exact `Voucher(channelId, cumulativeAmount)` matching a `DeliveryAck` for an acknowledged output prefix.

## 2. Cryptographic Commitments and Domains

### 2.1 Request Commitment (`request_commitment`)

Built before quote creation to prevent circular dependencies:

```text
request_commitment = keccak256(
  "autarqia.request.v0" ||
  canonical_json({
    "profile": "urn:autarqia:mppi:acknowledged-usage:0",
    "model_id": model_id,
    "model_revision": model_revision,
    "tokenizer_hash": tokenizer_hash,
    "chat_template_hash": chat_template_hash,
    "max_output_tokens": max_output_tokens,
    "temperature_milli": temperature_milli,
    "top_p_milli": top_p_milli,
    "seed": seed,
    "messages_digest": keccak256("autarqia.messages.v0" || salt || canonical_messages_bytes),
    "salt": salt
  })
)
```

- `request_commitment` never includes `quote_id`, `policy_id`, or signatures.
- Private prompt content and `salt` never leave the offchain session and are never submitted in onchain calldata or public financial logs.

### 2.2 Offchain EIP-712 Domain (`AutarqiaMPPIRun`)

Used for `InferencePolicy`, `CreditGrant`, `MeterFrame`, `DeliveryAck`, and `FinalReceipt`:

- `name`: `"AutarqiaMPPIRun"`
- `version`: `"0"`
- `chainId`: selected EVM chain ID (e.g., `5042002` for Arc Testnet)
- `verifyingContract`: selected `AndoMPPI` proxy address (e.g., `0xbdc5891b1372D7572046e41E1e38159c9A7fe638`)

### 2.3 Onchain Voucher EIP-712 Domain (`AndoMPPI`)

Used exclusively for collectible cumulative channel vouchers:

- `name`: `"AndoMPPI"`
- `version`: `"1"`
- `chainId`: selected EVM chain ID
- `verifyingContract`: `AndoMPPI` proxy address
- Primary type: `Voucher(bytes32 channelId,uint128 cumulativeAmount)`

### 2.4 Channel ID Derivation (`AndoMPPI` v1 Parity)

- Without operator (`operator == address(0)`):
  `keccak256(abi.encode(payer, payee, token, salt, authorizedSigner, verifyingProxy, chainId))`
- With operator (`operator != address(0)`):
  `keccak256(abi.encode(payer, payee, token, salt, authorizedSigner, operator, verifyingProxy, chainId))`
  Note that `operator` is the 3rd parameter in `computeChannelIdWithOperator(payer, payee, operator, token, salt, authorizedSigner)` but the 6th element in the `abi.encode` tuple.

## 3. Core Protocol Objects

All atomic money fields are canonical base-10 integer strings on the wire (`"0"` to `"340282366920938463463374607431768211455"`). Floating-point numbers are rejected.

### 3.1 `InferenceQuote`

Issued by the provider for a pre-assigned `run_id` and `request_commitment`:

- `profile`: `"urn:autarqia:mppi:acknowledged-usage:0"`
- `schema_version`: `"0.1.0"`
- `quote_id`: `QuoteId`
- `run_id`: `RunId`
- `provider_id`: `ProviderId`
- `node_id`: `NodeId`
- `request_commitment`: `0x`-prefixed 32-byte hex
- `model_id`, `model_revision`, `tokenizer_hash`, `chat_template_hash`
- `payment_rail`: `"ando_mppi_arc_v1"` or `"circle_gateway_x402"` (fixed for the lifetime of the run)
- `chain_id`, `verifying_contract`, `token_address`, `payee_address`, `operator_address`
- `input_tokens_quoted`: `u32`
- `max_output_tokens`: `u32`
- `input_rate`: `{ "numerator_atomic": "<u128>", "denominator_units": "<u64>" }`
- `output_rate`: `{ "numerator_atomic": "<u128>", "denominator_units": "<u64>" }`
- `quantization_note`: `"cumulative_class_charge = ceil(cumulative_billable_units * numerator_atomic / denominator_units)"`
- `quoted_prefill_atomic`: `<u128>`
- `max_total_atomic`: `<u128>`
- `issued_at_wall_ms`, `expires_at_wall_ms`, `max_clock_skew_ms`
- `provider_signature`: EIP-712 signature under `AutarqiaMPPIRun`

### 3.2 `InferencePolicy`

Signed by the buyer (`payer` or `authorizedSigner`) to bind budget and channel to `quote_id` and `run_id`:

- `policy_id`: `PolicyId`
- `quote_id`: `QuoteId`
- `run_id`: `RunId`
- `request_commitment`: 32-byte hex
- `channel_id`: 32-byte hex
- `payer_address`, `signer_address`, `payee_address`, `token_address`, `chain_id`, `verifying_contract`
- `max_total_atomic`: `<u128>`
- `max_input_tokens`: `u32`
- `max_output_tokens`: `u32`
- `input_rate`, `output_rate`
- `baseline_settled_or_high_water_atomic`: `<u128>`
- `expires_at_wall_ms`: `u64`
- `buyer_signature`: EIP-712 signature under `AutarqiaMPPIRun`

### 3.3 `CreditGrant`

Cumulative offchain authorization for the run (never directly redeemable onchain):

- `run_id`: `RunId`
- `policy_id`: `PolicyId`
- `quote_id`: `QuoteId`
- `grant_seq`: `GrantSeq` (strictly increasing per run)
- `cumulative_authorized_atomic`: `<u128>` (monotonic non-decreasing, `<= policy.max_total_atomic`)
- `max_authorized_output_tokens`: `u32`
- `causal_ack_seq`: `AckSeq`
- `causal_meter_seq`: `MeterSeq`
- `expires_at_wall_ms`: `u64`
- `buyer_signature`: EIP-712 signature under `AutarqiaMPPIRun`

### 3.4 `MeterFrame`

Immutable cumulative usage statement signed by the provider:

- `run_id`: `RunId`
- `meter_seq`: `MeterSeq` (strictly increasing)
- `output_seq`: `OutputSeq` (number of committed output tokens in prefix)
- `prefill_completed`: `bool`
- `input_tokens_processed`: `u32`
- `output_tokens_generated`: `u32`
- `prefix_commitment`: 32-byte hash over `(run_id, output_seq, token_ids[0..output_seq], utf8_text)`
- `provisional_delivered_amount_atomic`: `<u128>`
- `cumulative_amount_due_atomic`: `<u128>` (increases only upon verified `PrefillCompleted` and buyer-acknowledged `DeliveryAck` prefix)
- `last_acknowledged_seq`: `AckSeq`
- `last_acknowledged_output_seq`: `OutputSeq`
- `provider_signature`: EIP-712 signature under `AutarqiaMPPIRun`

### 3.5 `DeliveryAck`

Signed by the buyer to acknowledge a contiguous output prefix without gaps, paired atomically with an exact `AndoMPPI` voucher:

- `run_id`: `RunId`
- `ack_seq`: `AckSeq` (strictly increasing)
- `acknowledged_output_seq`: `OutputSeq`
- `acknowledged_input_tokens`: `u32`
- `prefix_commitment`: 32-byte hex matching the provider's committed prefix at `acknowledged_output_seq`
- `acknowledged_cumulative_due_atomic`: `<u128>` (run delta)
- `channel_cumulative_voucher_atomic`: `<u128>` (`channel_baseline_high_water_atomic + acknowledged_cumulative_due_atomic`)
- `ack_signature`: EIP-712 signature under `AutarqiaMPPIRun`
- `voucher_signature`: EIP-712 signature over `Voucher(channelId, channel_cumulative_voucher_atomic)` under `AndoMPPI` v1 (may reuse the prior valid voucher signature when quantization yields zero monetary delta)

### 3.6 `FinalReceipt`

Terminal, versioned reconciliation record signed by the provider:

- `run_id`, `quote_id`, `policy_id`, `channel_id`, `payment_rail`
- `receipt_version`: `u64` (increments if a valid late `DeliveryAck` + voucher arrives within the reconciliation window)
- `terminal_status`: `"completed" | "cancelled_by_buyer" | "halted_exposure_limit" | "halted_credit_exhausted" | "halted_energy_limit" | "halted_worker_error"`
- `input_tokens_processed`, `output_tokens_generated`, `output_tokens_acknowledged`
- `final_prefix_commitment`, `acknowledged_prefix_commitment`
- `authorized_atomic`, `funded_available_atomic`, `acknowledged_due_atomic`, `voucher_collectible_atomic`, `settled_atomic`, `uncollectible_exposure_atomic`
- `settlement_reference`: `{ "rail": "ando_mppi_arc_v1", "tx_hash": "...", "block_number": 123, "log_index": 0 }` or `{ "rail": "circle_gateway_x402", "transfer_uuid": "...", "state": "completed" }`
- `reconciliation_state`: `"pending_voucher" | "voucher_collectible" | "settlement_submitted" | "settled" | "uncollectible_recorded"`
- `provider_signature`: EIP-712 signature under `AutarqiaMPPIRun`

## 4. HTTP Payment Envelope (`HTTP 402`)

When a request to `/v1/mppi/runs` or `/v1/chat/completions` lacks an active session capability or sufficient grant:

- Status: `402 Payment Required`
- Header: `WWW-Authenticate: Payment realm="autarqia", profile="urn:autarqia:mppi:acknowledged-usage:0", version="0.1.0", challenge="<base64url(PaymentChallengeJson)>"`
- Client response header: `Authorization: Payment profile="urn:autarqia:mppi:acknowledged-usage:0", token="<session_capability_token>"`
- Receipt header: `Payment-Receipt: <base64url(FinalReceiptSummaryJson)>`

This envelope is distinct from Circle x402 (`PAYMENT-REQUIRED` / `PAYMENT-SIGNATURE` / `PAYMENT-RESPONSE`), which is restricted exclusively to runs that select `PaymentRail::CircleGatewayX402`.
