use std::fmt;
use std::str::FromStr;

use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as Sha2Digest, Sha256};
use sha3::Keccak256;
use thiserror::Error;

pub const AUTARQIA_MPPI_PROFILE_ID: &str = "urn:autarqia:mppi:acknowledged-usage:0";
pub const AUTARQIA_MPPI_PROFILE_VERSION: &str = "0.1.0";
pub const ARC_TESTNET_CHAIN_ID: u64 = 5_042_002;
pub const ARC_TESTNET_USDC_ADDRESS: &str = "0x3600000000000000000000000000000000000000";
pub const ARC_TESTNET_ANDO_MPPI_PROXY: &str = "0xbdc5891b1372D7572046e41E1e38159c9A7fe638";
pub const ARC_TESTNET_ANDO_MPPI_IMPL: &str = "0xca8AcB9Cc1bEEEaAcc7061C085D104ebA5161da0";
pub const ARC_TESTNET_GATEWAY_WALLET_PROXY: &str = "0x0077777d7EBA4688BDeF3E311b846F25870A19B9";
pub const ARC_EIP7708_SYSTEM_SENDER: &str = "0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PrimitivesError {
    #[error("invalid hex string: {0}")]
    InvalidHex(String),
    #[error("invalid byte length: expected {expected}, got {actual}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("rational tariff denominator must not be zero")]
    ZeroDenominator,
    #[error("arithmetic overflow in {0}")]
    ArithmeticOverflow(&'static str),
    #[error("invalid ECDSA signature or recovery failed: {0}")]
    SignatureError(String),
    #[error("invalid basis points {0}: must be <= 10000")]
    InvalidBasisPoints(u16),
}

macro_rules! define_string_id {
    ($name:ident) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(v: impl Into<String>) -> Self {
                Self(v.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

define_string_id!(SessionId);
define_string_id!(RunId);
define_string_id!(QuoteId);
define_string_id!(PreparedInputId);
define_string_id!(ReceiptId);
define_string_id!(BatchId);
define_string_id!(OutboxEntryId);
define_string_id!(OrderId);

macro_rules! define_u64_id {
    ($name:ident) => {
        #[derive(
            Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            pub const fn new(v: u64) -> Self {
                Self(v)
            }

            pub const fn get(self) -> u64 {
                self.0
            }

            pub const fn next(self) -> Self {
                Self(self.0.saturating_add(1))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_u64_id!(LedgerVersion);
define_u64_id!(WorkerEpoch);
define_u64_id!(OwnerFence);
define_u64_id!(AdmissionEpoch);
define_u64_id!(AllowanceSeq);
define_u64_id!(MonotonicTimestampMs);
define_u64_id!(WallClockUnixMs);

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SequenceNo(pub u32);

impl SequenceNo {
    pub const fn new(v: u32) -> Self {
        Self(v)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Bytes32(pub [u8; 32]);

impl Bytes32 {
    pub const ZERO: Self = Self([0u8; 32]);

    pub fn from_hex(s: &str) -> Result<Self, PrimitivesError> {
        let trimmed = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        let raw = hex::decode(trimmed).map_err(|e| PrimitivesError::InvalidHex(e.to_string()))?;
        if raw.len() != 32 {
            return Err(PrimitivesError::InvalidLength {
                expected: 32,
                actual: raw.len(),
            });
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&raw);
        Ok(Self(out))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl fmt::Debug for Bytes32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes32({})", self.to_hex())
    }
}

impl fmt::Display for Bytes32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Bytes32 {
    type Err = PrimitivesError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for Bytes32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Bytes32 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

pub type ChannelId = Bytes32;

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EvmAddress(pub [u8; 20]);

impl EvmAddress {
    pub const ZERO: Self = Self([0u8; 20]);

    pub fn from_hex(s: &str) -> Result<Self, PrimitivesError> {
        let trimmed = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        let raw = hex::decode(trimmed).map_err(|e| PrimitivesError::InvalidHex(e.to_string()))?;
        if raw.len() != 20 {
            return Err(PrimitivesError::InvalidLength {
                expected: 20,
                actual: raw.len(),
            });
        }
        let mut out = [0u8; 20];
        out.copy_from_slice(&raw);
        Ok(Self(out))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }

    pub fn to_word32(&self) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[12..32].copy_from_slice(&self.0);
        word
    }
}

impl fmt::Debug for EvmAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EvmAddress({})", self.to_hex())
    }
}

impl fmt::Display for EvmAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for EvmAddress {
    type Err = PrimitivesError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for EvmAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for EvmAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct SignatureBytes(pub Vec<u8>);

impl SignatureBytes {
    pub fn from_hex(s: &str) -> Result<Self, PrimitivesError> {
        let trimmed = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        let raw = hex::decode(trimmed).map_err(|e| PrimitivesError::InvalidHex(e.to_string()))?;
        Ok(Self(raw))
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(&self.0))
    }
}

impl fmt::Debug for SignatureBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SignatureBytes({})", self.to_hex())
    }
}

impl fmt::Display for SignatureBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for SignatureBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for SignatureBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenAtomicAmount(pub u128);

impl TokenAtomicAmount {
    pub const ZERO: Self = Self(0);

    pub const fn new(v: u128) -> Self {
        Self(v)
    }

    pub const fn get(self) -> u128 {
        self.0
    }

    pub fn checked_add(self, rhs: Self) -> Result<Self, PrimitivesError> {
        self.0
            .checked_add(rhs.0)
            .map(Self)
            .ok_or(PrimitivesError::ArithmeticOverflow("TokenAtomicAmount::checked_add"))
    }

    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl fmt::Display for TokenAtomicAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for TokenAtomicAmount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for TokenAtomicAmount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Str(String),
            Num(u64),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Str(s) => s
                .parse::<u128>()
                .map(TokenAtomicAmount)
                .map_err(serde::de::Error::custom),
            Repr::Num(n) => Ok(TokenAtomicAmount(u128::from(n))),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RationalRate {
    pub numerator: TokenAtomicAmount,
    pub denominator: TokenAtomicAmount,
}

impl RationalRate {
    pub fn new(numerator: u128, denominator: u128) -> Result<Self, PrimitivesError> {
        if denominator == 0 {
            return Err(PrimitivesError::ZeroDenominator);
        }
        Ok(Self {
            numerator: TokenAtomicAmount(numerator),
            denominator: TokenAtomicAmount(denominator),
        })
    }

    pub fn ceil_charge(&self, units: u32) -> Result<TokenAtomicAmount, PrimitivesError> {
        if self.denominator.0 == 0 {
            return Err(PrimitivesError::ZeroDenominator);
        }
        if units == 0 || self.numerator.0 == 0 {
            return Ok(TokenAtomicAmount::ZERO);
        }
        let prod = u128::from(units)
            .checked_mul(self.numerator.0)
            .ok_or(PrimitivesError::ArithmeticOverflow("RationalRate::ceil_charge mul"))?;
        let num_plus = prod
            .checked_add(self.denominator.0 - 1)
            .ok_or(PrimitivesError::ArithmeticOverflow("RationalRate::ceil_charge add"))?;
        Ok(TokenAtomicAmount(num_plus / self.denominator.0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTariff {
    pub prefill_rate: RationalRate,
    pub output_rate: RationalRate,
    pub min_run_fee: TokenAtomicAmount,
    pub uncovered_exposure_cap: TokenAtomicAmount,
    pub ack_stride_tokens: u32,
}

impl UsageTariff {
    pub fn compute_cumulative_due(
        &self,
        input_tokens_processed: u32,
        output_tokens_emitted: u32,
    ) -> Result<TokenAtomicAmount, PrimitivesError> {
        if input_tokens_processed == 0 && output_tokens_emitted == 0 {
            return Ok(TokenAtomicAmount::ZERO);
        }
        let prefill = self.prefill_rate.ceil_charge(input_tokens_processed)?;
        let decode = self.output_rate.ceil_charge(output_tokens_emitted)?;
        self.min_run_fee.checked_add(prefill)?.checked_add(decode)
    }

    pub fn max_output_seq_exclusive_for_budget(
        &self,
        input_tokens_processed: u32,
        covered_budget: TokenAtomicAmount,
        requested_max_output_tokens: u32,
    ) -> Result<u32, PrimitivesError> {
        let total_cap = self.uncovered_exposure_cap.checked_add(covered_budget)?;
        let prefill_due = self.compute_cumulative_due(input_tokens_processed, 0)?;
        if prefill_due > total_cap {
            return Ok(0);
        }
        let mut allowed_tokens = 0u32;
        for candidate in 1..=requested_max_output_tokens {
            let due = self.compute_cumulative_due(input_tokens_processed, candidate)?;
            if due <= total_cap {
                allowed_tokens = candidate;
            } else {
                break;
            }
        }
        Ok(allowed_tokens.saturating_add(1))
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Milliwatts(pub i64);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Millivolts(pub u32);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Milliamps(pub i64);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BasisPoints(pub u16);

impl BasisPoints {
    pub fn new(bps: u16) -> Result<Self, PrimitivesError> {
        if bps > 10_000 {
            return Err(PrimitivesError::InvalidBasisPoints(bps));
        }
        Ok(Self(bps))
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MillidegreesCelsius(pub i32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnergyPolicyMode {
    SolarSurplusOnly,
    SolarPlusReservedBattery,
    GridAssisted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnergyTelemetrySnapshot {
    pub solar_power_mw: Milliwatts,
    pub battery_power_mw: Milliwatts,
    pub load_power_mw: Milliwatts,
    pub grid_power_mw: Milliwatts,
    pub battery_soc_bps: BasisPoints,
    pub panel_voltage_mv: Millivolts,
    pub panel_current_ma: Milliamps,
    pub temp_mc: MillidegreesCelsius,
    pub measured_at_monotonic_ms: MonotonicTimestampMs,
    pub source: String,
}

impl EnergyTelemetrySnapshot {
    pub fn net_surplus_mw(&self) -> Milliwatts {
        Milliwatts(self.solar_power_mw.0 - self.load_power_mw.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnergyPolicyConfig {
    pub mode: EnergyPolicyMode,
    pub min_surplus_mw_for_new_run: Milliwatts,
    pub min_battery_soc_bps_for_new_run: BasisPoints,
    pub hard_floor_soc_bps_abort: BasisPoints,
    pub max_telemetry_staleness_ms: u64,
    pub estimated_inference_load_mw: Milliwatts,
}

impl Default for EnergyPolicyConfig {
    fn default() -> Self {
        Self {
            mode: EnergyPolicyMode::SolarPlusReservedBattery,
            min_surplus_mw_for_new_run: Milliwatts(15_000),
            min_battery_soc_bps_for_new_run: BasisPoints(3_500),
            hard_floor_soc_bps_abort: BasisPoints(2_000),
            max_telemetry_staleness_ms: 15_000,
            estimated_inference_load_mw: Milliwatts(18_000),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementRail {
    AndoMppiArcV1,
    CircleGatewayX402,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIntegrityManifest {
    pub engine_name: String,
    pub airllm_commit: String,
    pub patch_sha256: Bytes32,
    pub model_id: String,
    pub model_revision: String,
    pub tokenizer_sha256: Bytes32,
    pub chat_template_sha256: Bytes32,
    pub original_manifest_sha256: Bytes32,
    pub split_manifest_sha256: Bytes32,
    pub runtime_device: String,
    pub runtime_dtype: String,
    pub tested_context_limit: u32,
    pub max_prefill_tokens: u32,
    pub uninterruptible_step_tokens: u32,
    pub trust_remote_code: bool,
}

impl ModelIntegrityManifest {
    pub fn manifest_commitment(&self) -> Bytes32 {
        let mut h = Sha256::new();
        h.update(b"autarqia:model-manifest:v0\x00");
        h.update(self.engine_name.as_bytes());
        h.update(b"\x00");
        h.update(self.airllm_commit.as_bytes());
        h.update(b"\x00");
        h.update(self.patch_sha256.0);
        h.update(self.model_id.as_bytes());
        h.update(b"\x00");
        h.update(self.model_revision.as_bytes());
        h.update(b"\x00");
        h.update(self.tokenizer_sha256.0);
        h.update(self.chat_template_sha256.0);
        h.update(self.original_manifest_sha256.0);
        h.update(self.split_manifest_sha256.0);
        let mut out = [0u8; 32];
        out.copy_from_slice(&h.finalize());
        Bytes32(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalWorkAllowance {
    pub run_id: RunId,
    pub worker_epoch: WorkerEpoch,
    pub owner_fence: OwnerFence,
    pub admission_epoch: AdmissionEpoch,
    pub allowance_seq: AllowanceSeq,
    pub max_output_seq_exclusive: u32,
    pub expires_at_monotonic_ms: MonotonicTimestampMs,
    pub ledger_version: LedgerVersion,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVoucher {
    pub channel_id: ChannelId,
    pub cumulative_amount: TokenAtomicAmount,
    pub signature: SignatureBytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct X402AuthorizationPayload {
    pub from: EvmAddress,
    pub to: EvmAddress,
    pub value: TokenAtomicAmount,
    pub valid_after: u64,
    pub valid_before: u64,
    pub nonce: Bytes32,
    pub signature: SignatureBytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAttestation {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub channel_id: ChannelId,
    pub input_tokens_processed: u32,
    pub output_seq: u32,
    pub cumulative_due_atomic: TokenAtomicAmount,
    pub prefix_commitment: Bytes32,
    pub model_manifest_hash: Bytes32,
    pub admission_epoch: AdmissionEpoch,
    pub issued_at_unix_ms: WallClockUnixMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedUsageAttestation {
    pub attestation: UsageAttestation,
    pub provider_signer: EvmAddress,
    pub signature: SignatureBytes,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAck {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub channel_id: ChannelId,
    pub acknowledged_output_seq: u32,
    pub acknowledged_cumulative_due_atomic: TokenAtomicAmount,
    pub voucher_cumulative_amount_atomic: TokenAtomicAmount,
    pub prefix_commitment: Bytes32,
    pub issued_at_unix_ms: WallClockUnixMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedUsageAck {
    pub ack: UsageAck,
    pub buyer_signer: EvmAddress,
    pub signature: SignatureBytes,
    pub voucher: Option<SignedVoucher>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminalStatus {
    Completed,
    CompletedMaxTokens,
    PausedPaymentRequired,
    AllowanceExhausted,
    EnergyAborted,
    CancelledByBuyer,
    WorkerError,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReceipt {
    pub receipt_id: ReceiptId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub rail: SettlementRail,
    pub channel_id: ChannelId,
    pub input_tokens_processed: u32,
    pub output_tokens_generated: u32,
    pub cumulative_due_atomic: TokenAtomicAmount,
    pub acknowledged_cumulative_due_atomic: TokenAtomicAmount,
    pub covered_voucher_cumulative_atomic: TokenAtomicAmount,
    pub settled_onchain_cumulative_atomic: TokenAtomicAmount,
    pub final_prefix_commitment: Bytes32,
    pub model_manifest_hash: Bytes32,
    pub energy_mode: EnergyPolicyMode,
    pub terminal_status: RunTerminalStatus,
    pub settlement_tx_hash: Option<Bytes32>,
    pub issued_at_unix_ms: WallClockUnixMs,
    pub provider_signer: EvmAddress,
    pub provider_signature: SignatureBytes,
}

pub fn keccak256(data: &[u8]) -> Bytes32 {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&hasher.finalize());
    Bytes32(out)
}

pub fn encode_u64_word(v: u64) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[24..32].copy_from_slice(&v.to_be_bytes());
    word
}

pub fn encode_u128_word(v: u128) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[16..32].copy_from_slice(&v.to_be_bytes());
    word
}

pub fn encode_u32_word(v: u32) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[28..32].copy_from_slice(&v.to_be_bytes());
    word
}

pub fn compute_channel_id(
    payer: EvmAddress,
    payee: EvmAddress,
    token: EvmAddress,
    salt: Bytes32,
    authorized_signer: EvmAddress,
    verifying_contract: EvmAddress,
    chain_id: u64,
) -> ChannelId {
    let mut buf = Vec::with_capacity(7 * 32);
    buf.extend_from_slice(&payer.to_word32());
    buf.extend_from_slice(&payee.to_word32());
    buf.extend_from_slice(&token.to_word32());
    buf.extend_from_slice(&salt.0);
    buf.extend_from_slice(&authorized_signer.to_word32());
    buf.extend_from_slice(&verifying_contract.to_word32());
    buf.extend_from_slice(&encode_u64_word(chain_id));
    keccak256(&buf)
}

pub fn compute_channel_id_with_operator(
    payer: EvmAddress,
    payee: EvmAddress,
    operator: EvmAddress,
    token: EvmAddress,
    salt: Bytes32,
    authorized_signer: EvmAddress,
    verifying_contract: EvmAddress,
    chain_id: u64,
) -> ChannelId {
    let mut buf = Vec::with_capacity(8 * 32);
    buf.extend_from_slice(&payer.to_word32());
    buf.extend_from_slice(&payee.to_word32());
    buf.extend_from_slice(&token.to_word32());
    buf.extend_from_slice(&salt.0);
    buf.extend_from_slice(&authorized_signer.to_word32());
    buf.extend_from_slice(&operator.to_word32());
    buf.extend_from_slice(&verifying_contract.to_word32());
    buf.extend_from_slice(&encode_u64_word(chain_id));
    keccak256(&buf)
}

pub fn eip712_domain_separator(
    name: &str,
    version: &str,
    chain_id: u64,
    verifying_contract: EvmAddress,
) -> Bytes32 {
    let type_hash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name_hash = keccak256(name.as_bytes());
    let version_hash = keccak256(version.as_bytes());
    let mut buf = Vec::with_capacity(5 * 32);
    buf.extend_from_slice(&type_hash.0);
    buf.extend_from_slice(&name_hash.0);
    buf.extend_from_slice(&version_hash.0);
    buf.extend_from_slice(&encode_u64_word(chain_id));
    buf.extend_from_slice(&verifying_contract.to_word32());
    keccak256(&buf)
}

pub fn ando_mppi_domain_separator(chain_id: u64, verifying_contract: EvmAddress) -> Bytes32 {
    eip712_domain_separator("AndoMPPI", "1", chain_id, verifying_contract)
}

pub fn ando_mppi_voucher_digest(
    domain_separator: Bytes32,
    channel_id: ChannelId,
    cumulative_amount: TokenAtomicAmount,
) -> Bytes32 {
    let voucher_typehash = keccak256(b"Voucher(bytes32 channelId,uint128 cumulativeAmount)");
    let mut struct_buf = Vec::with_capacity(3 * 32);
    struct_buf.extend_from_slice(&voucher_typehash.0);
    struct_buf.extend_from_slice(&channel_id.0);
    struct_buf.extend_from_slice(&encode_u128_word(cumulative_amount.0));
    let struct_hash = keccak256(&struct_buf);

    let mut digest_buf = Vec::with_capacity(66);
    digest_buf.extend_from_slice(b"\x19\x01");
    digest_buf.extend_from_slice(&domain_separator.0);
    digest_buf.extend_from_slice(&struct_hash.0);
    keccak256(&digest_buf)
}

pub fn autarqia_mppi_run_domain_separator(
    chain_id: u64,
    verifying_contract: EvmAddress,
) -> Bytes32 {
    eip712_domain_separator("AutarqiaMPPIRun", "0", chain_id, verifying_contract)
}

pub fn usage_attestation_digest(
    domain_separator: Bytes32,
    att: &UsageAttestation,
) -> Bytes32 {
    let typehash = keccak256(
        b"UsageAttestation(bytes32 sessionIdHash,bytes32 runIdHash,bytes32 channelId,uint32 inputTokensProcessed,uint32 outputSeq,uint128 cumulativeDueAtomic,bytes32 prefixCommitment,bytes32 modelManifestHash,uint64 admissionEpoch,uint64 issuedAtUnixMs)",
    );
    let session_hash = keccak256(att.session_id.as_str().as_bytes());
    let run_hash = keccak256(att.run_id.as_str().as_bytes());
    let mut struct_buf = Vec::with_capacity(11 * 32);
    struct_buf.extend_from_slice(&typehash.0);
    struct_buf.extend_from_slice(&session_hash.0);
    struct_buf.extend_from_slice(&run_hash.0);
    struct_buf.extend_from_slice(&att.channel_id.0);
    struct_buf.extend_from_slice(&encode_u32_word(att.input_tokens_processed));
    struct_buf.extend_from_slice(&encode_u32_word(att.output_seq));
    struct_buf.extend_from_slice(&encode_u128_word(att.cumulative_due_atomic.0));
    struct_buf.extend_from_slice(&att.prefix_commitment.0);
    struct_buf.extend_from_slice(&att.model_manifest_hash.0);
    struct_buf.extend_from_slice(&encode_u64_word(att.admission_epoch.0));
    struct_buf.extend_from_slice(&encode_u64_word(att.issued_at_unix_ms.0));
    let struct_hash = keccak256(&struct_buf);

    let mut digest_buf = Vec::with_capacity(66);
    digest_buf.extend_from_slice(b"\x19\x01");
    digest_buf.extend_from_slice(&domain_separator.0);
    digest_buf.extend_from_slice(&struct_hash.0);
    keccak256(&digest_buf)
}

pub fn usage_ack_digest(domain_separator: Bytes32, ack: &UsageAck) -> Bytes32 {
    let typehash = keccak256(
        b"UsageAck(bytes32 sessionIdHash,bytes32 runIdHash,bytes32 channelId,uint32 acknowledgedOutputSeq,uint128 acknowledgedCumulativeDueAtomic,uint128 voucherCumulativeAmountAtomic,bytes32 prefixCommitment,uint64 issuedAtUnixMs)",
    );
    let session_hash = keccak256(ack.session_id.as_str().as_bytes());
    let run_hash = keccak256(ack.run_id.as_str().as_bytes());
    let mut struct_buf = Vec::with_capacity(9 * 32);
    struct_buf.extend_from_slice(&typehash.0);
    struct_buf.extend_from_slice(&session_hash.0);
    struct_buf.extend_from_slice(&run_hash.0);
    struct_buf.extend_from_slice(&ack.channel_id.0);
    struct_buf.extend_from_slice(&encode_u32_word(ack.acknowledged_output_seq));
    struct_buf.extend_from_slice(&encode_u128_word(ack.acknowledged_cumulative_due_atomic.0));
    struct_buf.extend_from_slice(&encode_u128_word(ack.voucher_cumulative_amount_atomic.0));
    struct_buf.extend_from_slice(&ack.prefix_commitment.0);
    struct_buf.extend_from_slice(&encode_u64_word(ack.issued_at_unix_ms.0));
    let struct_hash = keccak256(&struct_buf);

    let mut digest_buf = Vec::with_capacity(66);
    digest_buf.extend_from_slice(b"\x19\x01");
    digest_buf.extend_from_slice(&domain_separator.0);
    digest_buf.extend_from_slice(&struct_hash.0);
    keccak256(&digest_buf)
}

pub fn gateway_wallet_batched_domain_separator(
    chain_id: u64,
    verifying_contract: EvmAddress,
) -> Bytes32 {
    eip712_domain_separator("GatewayWalletBatched", "1", chain_id, verifying_contract)
}

pub fn x402_transfer_with_authorization_digest(
    domain_separator: Bytes32,
    payload: &X402AuthorizationPayload,
) -> Bytes32 {
    let typehash = keccak256(
        b"TransferWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)",
    );
    let mut struct_buf = Vec::with_capacity(7 * 32);
    struct_buf.extend_from_slice(&typehash.0);
    struct_buf.extend_from_slice(&payload.from.to_word32());
    struct_buf.extend_from_slice(&payload.to.to_word32());
    struct_buf.extend_from_slice(&encode_u128_word(payload.value.0));
    struct_buf.extend_from_slice(&encode_u64_word(payload.valid_after));
    struct_buf.extend_from_slice(&encode_u64_word(payload.valid_before));
    struct_buf.extend_from_slice(&payload.nonce.0);
    let struct_hash = keccak256(&struct_buf);

    let mut digest_buf = Vec::with_capacity(66);
    digest_buf.extend_from_slice(b"\x19\x01");
    digest_buf.extend_from_slice(&domain_separator.0);
    digest_buf.extend_from_slice(&struct_hash.0);
    keccak256(&digest_buf)
}

pub fn compute_initial_prefix_state(run_id: &str) -> Bytes32 {
    let mut h = Sha256::new();
    h.update(b"autarqia:mppi:v0:prefix\x00");
    h.update(run_id.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    Bytes32(out)
}

pub fn advance_prefix_state(
    prev: Bytes32,
    output_seq: u32,
    token_id: u32,
    text_delta: &str,
) -> Bytes32 {
    let mut h = Sha256::new();
    h.update(prev.0);
    h.update(output_seq.to_be_bytes());
    h.update(token_id.to_be_bytes());
    h.update(text_delta.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&h.finalize());
    Bytes32(out)
}

pub fn address_from_signing_key(sk: &SigningKey) -> EvmAddress {
    address_from_verifying_key(sk.verifying_key())
}

pub fn address_from_verifying_key(vk: &VerifyingKey) -> EvmAddress {
    let uncompressed = vk.to_encoded_point(false);
    let pubkey_bytes = &uncompressed.as_bytes()[1..];
    let hash = keccak256(pubkey_bytes);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&hash.0[12..32]);
    EvmAddress(addr)
}

pub fn sign_eip712_digest(
    sk: &SigningKey,
    digest: Bytes32,
) -> Result<SignatureBytes, PrimitivesError> {
    let (sig, recid) = sk
        .sign_prehash_recoverable(&digest.0)
        .map_err(|e| PrimitivesError::SignatureError(e.to_string()))?;
    let mut out = Vec::with_capacity(65);
    out.extend_from_slice(&sig.to_bytes());
    out.push(recid.to_byte() + 27);
    Ok(SignatureBytes(out))
}

pub fn recover_eip712_signer(
    digest: Bytes32,
    sig_bytes: &SignatureBytes,
) -> Result<EvmAddress, PrimitivesError> {
    if sig_bytes.0.len() != 65 {
        return Err(PrimitivesError::InvalidLength {
            expected: 65,
            actual: sig_bytes.0.len(),
        });
    }
    let sig = Signature::from_slice(&sig_bytes.0[0..64])
        .map_err(|e| PrimitivesError::SignatureError(e.to_string()))?;
    if sig.normalize_s().is_some() {
        return Err(PrimitivesError::SignatureError(
            "malleable high-s signature rejected".to_owned(),
        ));
    }
    let v = sig_bytes.0[64];
    let rec_byte = match v {
        27 | 28 => v - 27,
        0 | 1 => v,
        _ => {
            return Err(PrimitivesError::SignatureError(format!(
                "invalid recovery id v={v}"
            )))
        }
    };
    let recid = RecoveryId::from_byte(rec_byte)
        .ok_or_else(|| PrimitivesError::SignatureError("invalid recovery id".to_owned()))?;
    let vk = VerifyingKey::recover_from_prehash(&digest.0, &sig, recid)
        .map_err(|e| PrimitivesError::SignatureError(e.to_string()))?;
    Ok(address_from_verifying_key(&vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_live_arc_testnet_ando_mppi_golden_vectors() {
        let proxy = EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap();
        let domain = ando_mppi_domain_separator(ARC_TESTNET_CHAIN_ID, proxy);
        assert_eq!(
            domain.to_hex(),
            "0x8c4a261dc84bafb5b349d4a1c8dbac75de4246972ac9531ade334e690b323768"
        );

        let payer = EvmAddress::from_hex("0x1111111111111111111111111111111111111111").unwrap();
        let payee = EvmAddress::from_hex("0x2222222222222222222222222222222222222222").unwrap();
        let operator = EvmAddress::from_hex("0x3333333333333333333333333333333333333333").unwrap();
        let token = EvmAddress::from_hex(ARC_TESTNET_USDC_ADDRESS).unwrap();
        let salt = Bytes32::from_hex(
            "0x0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap();
        let signer = EvmAddress::from_hex("0x4444444444444444444444444444444444444444").unwrap();

        let ch_id = compute_channel_id(payer, payee, token, salt, signer, proxy, ARC_TESTNET_CHAIN_ID);
        assert_eq!(
            ch_id.to_hex(),
            "0x2b6548a57025a137badb75792fa955609e8b2ccbf70e384d5d9ae40ffc5cc2cc"
        );

        let ch_id_op = compute_channel_id_with_operator(
            payer,
            payee,
            operator,
            token,
            salt,
            signer,
            proxy,
            ARC_TESTNET_CHAIN_ID,
        );
        assert_eq!(
            ch_id_op.to_hex(),
            "0x0c24302aa4f99f76a88f1f50da5bdd6e8b2f0cba4e28a5a35c4b10d53ab53d1d"
        );

        let voucher_digest =
            ando_mppi_voucher_digest(domain, ch_id, TokenAtomicAmount::new(1_500_000));
        assert_eq!(
            voucher_digest.to_hex(),
            "0xcda10c1faa36e57e4b0b139070e846447bc06908037fe77ddb3eb36d8d05febd"
        );
    }

    #[test]
    fn rational_tariff_ceiling_and_sign_recover_roundtrip() {
        let tariff = UsageTariff {
            prefill_rate: RationalRate::new(25, 10).unwrap(),
            output_rate: RationalRate::new(75, 10).unwrap(),
            min_run_fee: TokenAtomicAmount::new(100),
            uncovered_exposure_cap: TokenAtomicAmount::new(500),
            ack_stride_tokens: 4,
        };
        assert_eq!(tariff.compute_cumulative_due(3, 1).unwrap().0, 100 + 8 + 8);

        let sk = SigningKey::from_bytes((&[0x11u8; 32]).into()).unwrap();
        let addr = address_from_signing_key(&sk);
        let domain = autarqia_mppi_run_domain_separator(
            ARC_TESTNET_CHAIN_ID,
            EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap(),
        );
        let ack = UsageAck {
            session_id: SessionId::new("sess_1"),
            run_id: RunId::new("run_1"),
            channel_id: Bytes32::ZERO,
            acknowledged_output_seq: 4,
            acknowledged_cumulative_due_atomic: TokenAtomicAmount::new(250),
            voucher_cumulative_amount_atomic: TokenAtomicAmount::new(500),
            prefix_commitment: compute_initial_prefix_state("run_1"),
            issued_at_unix_ms: WallClockUnixMs::new(1_700_000_000_000),
        };
        let digest = usage_ack_digest(domain, &ack);
        let sig = sign_eip712_digest(&sk, digest).unwrap();
        let recovered = recover_eip712_signer(digest, &sig).unwrap();
        assert_eq!(recovered, addr);
    }
}
