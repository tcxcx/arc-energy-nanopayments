pub use autarqia_primitives::{
    AdmissionEpoch, AllowanceSeq, BatchId, Bytes32, ChannelId, ChatMessage, EnergyPolicyConfig,
    EnergyPolicyMode, EnergyTelemetrySnapshot, EvmAddress, InternalWorkAllowance, LedgerVersion,
    ModelIntegrityManifest, MonotonicTimestampMs, OrderId, OutboxEntryId, OwnerFence,
    PreparedInputId, QuoteId, ReceiptId, RunId, RunReceipt, RunTerminalStatus, SessionId,
    SettlementRail, SignedUsageAck, SignedUsageAttestation, SignedVoucher, TokenAtomicAmount,
    UsageTariff, WallClockUnixMs, WorkerEpoch, X402AuthorizationPayload,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemId {
    Store,
    Energy,
    Payments,
    Inference,
    Bufi,
    Nucleus,
    Rpc,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalPriority {
    ControlPlaneCritical,
    NormalWork,
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubsystemError {
    #[error("store error: {0}")]
    Store(String),
    #[error("energy admission rejected: {0}")]
    EnergyRejected(String),
    #[error("payment error: {0}")]
    Payment(String),
    #[error("inference worker error: {0}")]
    Inference(String),
    #[error("runtime rejection: {0}")]
    RuntimeRejected(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict or duplicate order: {0}")]
    Conflict(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDiscoveryDocument {
    pub profile_id: String,
    pub profile_version: String,
    pub chain_id: u64,
    pub ando_mppi_proxy: EvmAddress,
    pub ando_mppi_implementation: EvmAddress,
    pub gateway_wallet_proxy: EvmAddress,
    pub usdc_token: EvmAddress,
    pub provider_payee: EvmAddress,
    pub provider_operator: EvmAddress,
    pub provider_signer: EvmAddress,
    pub supported_rails: Vec<SettlementRail>,
    pub model_manifest: ModelIntegrityManifest,
    pub tariff: UsageTariff,
    pub energy_policy: EnergyPolicyConfig,
    pub latest_energy: Option<EnergyTelemetrySnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateQuoteRequest {
    pub model_id: String,
    pub rail: SettlementRail,
    pub estimated_input_tokens: u32,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateQuoteResponse {
    pub quote_id: QuoteId,
    pub profile_id: String,
    pub rail: SettlementRail,
    pub model_id: String,
    pub model_manifest_hash: Bytes32,
    pub tariff: UsageTariff,
    pub estimated_total_atomic: TokenAtomicAmount,
    pub required_initial_deposit_atomic: TokenAtomicAmount,
    pub energy_mode: EnergyPolicyMode,
    pub expires_at_unix_ms: WallClockUnixMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub quote_id: Option<QuoteId>,
    pub rail: SettlementRail,
    pub payer: EvmAddress,
    pub buyer_signer: EvmAddress,
    pub channel_id: ChannelId,
    pub onchain_deposit_atomic: TokenAtomicAmount,
    pub initial_voucher: Option<SignedVoucher>,
    pub x402_authorization: Option<X402AuthorizationPayload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
    pub rail: SettlementRail,
    pub channel_id: ChannelId,
    pub covered_voucher_cumulative_atomic: TokenAtomicAmount,
    pub onchain_deposit_atomic: TokenAtomicAmount,
    pub bearer_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRunRequest {
    pub session_id: SessionId,
    pub order_id: OrderId,
    pub model_id: String,
    pub messages: Vec<ChatMessage>,
    pub max_output_tokens: u32,
    pub temperature_milli: u32,
    pub top_p_milli: u32,
    pub seed: u64,
    pub initial_voucher: Option<SignedVoucher>,
    pub x402_authorization: Option<X402AuthorizationPayload>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRunResponse {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub order_id: OrderId,
    pub rail: SettlementRail,
    pub input_token_count: u32,
    pub initial_allowance: InternalWorkAllowance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunControlCommand {
    AckUsage {
        signed_ack: SignedUsageAck,
    },
    TopUpChannel {
        new_onchain_deposit_atomic: TokenAtomicAmount,
        voucher: Option<SignedVoucher>,
    },
    Cancel {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunControlResponse {
    pub run_id: RunId,
    pub accepted: bool,
    pub detail: String,
    pub updated_allowance: Option<InternalWorkAllowance>,
    pub covered_voucher_cumulative_atomic: TokenAtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RunStreamFrame {
    RunStarted {
        run_id: RunId,
        session_id: SessionId,
        rail: SettlementRail,
        model_id: String,
        model_manifest_hash: Bytes32,
        admission_epoch: AdmissionEpoch,
    },
    PrefillCompleted {
        run_id: RunId,
        input_tokens_processed: u32,
        cumulative_due_atomic: TokenAtomicAmount,
        elapsed_monotonic_ms: u64,
    },
    TokenDelta {
        run_id: RunId,
        output_seq: u32,
        token_id: u32,
        text_delta: String,
        prefix_commitment: Bytes32,
        cumulative_due_atomic: TokenAtomicAmount,
        allowance_seq_used: AllowanceSeq,
    },
    UsageAttestation {
        signed_attestation: SignedUsageAttestation,
        require_ack_by_output_seq: u32,
    },
    PaymentRequired {
        run_id: RunId,
        current_output_seq: u32,
        cumulative_due_atomic: TokenAtomicAmount,
        covered_voucher_cumulative_atomic: TokenAtomicAmount,
        uncovered_exposure_cap_atomic: TokenAtomicAmount,
        reason: String,
    },
    AllowanceExtended {
        run_id: RunId,
        allowance: InternalWorkAllowance,
    },
    ReceiptReady {
        receipt: RunReceipt,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelLedgerView {
    pub channel_id: ChannelId,
    pub rail: SettlementRail,
    pub payer: EvmAddress,
    pub payee: EvmAddress,
    pub authorized_signer: EvmAddress,
    pub deposit_atomic: TokenAtomicAmount,
    pub highest_verified_voucher_atomic: TokenAtomicAmount,
    pub highest_voucher_signature: Option<autarqia_primitives::SignatureBytes>,
    pub settled_onchain_atomic: TokenAtomicAmount,
    pub accrued_cumulatively_atomic: TokenAtomicAmount,
    pub acknowledged_cumulatively_atomic: TokenAtomicAmount,
    pub close_requested_at_unix_s: Option<u64>,
    pub closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLedgerView {
    pub run_id: RunId,
    pub session_id: SessionId,
    pub order_id: OrderId,
    pub rail: SettlementRail,
    pub channel_id: ChannelId,
    pub prepared_input_id: PreparedInputId,
    pub prompt_sha256: Bytes32,
    pub input_tokens_processed: u32,
    pub output_tokens_generated: u32,
    pub accrued_atomic: TokenAtomicAmount,
    pub acknowledged_output_seq: u32,
    pub acknowledged_atomic: TokenAtomicAmount,
    pub latest_prefix_commitment: Bytes32,
    pub active_allowance: InternalWorkAllowance,
    pub terminal_status: Option<RunTerminalStatus>,
    pub receipt: Option<RunReceipt>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufiEntryKind {
    RunReceiptCommitted,
    VoucherHighWaterUpdated,
    OnchainSettlementSubmitted,
    OnchainSettlementConfirmed,
    EnergyPolicyTransition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufiOutboxEntry {
    pub entry_id: OutboxEntryId,
    pub ledger_version: LedgerVersion,
    pub kind: BufiEntryKind,
    pub run_id: Option<RunId>,
    pub channel_id: Option<ChannelId>,
    pub payload_json: String,
    pub created_at_unix_ms: WallClockUnixMs,
    pub delivered: bool,
    pub delivery_attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementBatchItem {
    pub channel_id: ChannelId,
    pub cumulative_amount: TokenAtomicAmount,
    pub signature: autarqia_primitives::SignatureBytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementSubmissionReceipt {
    pub batch_id: BatchId,
    pub rail: SettlementRail,
    pub tx_hash: Bytes32,
    pub settled_channels: Vec<(ChannelId, TokenAtomicAmount)>,
    pub block_number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeHealthSnapshot {
    pub healthy: bool,
    pub ready: bool,
    pub worker_ready: bool,
    pub worker_epoch: WorkerEpoch,
    pub owner_fence: OwnerFence,
    pub admission_epoch: AdmissionEpoch,
    pub ledger_version: LedgerVersion,
    pub active_runs: usize,
    pub energy_mode: EnergyPolicyMode,
    pub latest_energy: Option<EnergyTelemetrySnapshot>,
    pub pending_bufi_outbox: usize,
}
