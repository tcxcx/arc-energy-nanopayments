use std::collections::BTreeMap;

use autarqia_primitives::{
    advance_prefix_state, ando_mppi_domain_separator, ando_mppi_voucher_digest,
    autarqia_mppi_run_domain_separator, compute_initial_prefix_state,
    gateway_wallet_batched_domain_separator, recover_eip712_signer, usage_ack_digest,
    x402_transfer_with_authorization_digest, AdmissionEpoch, AllowanceSeq, Bytes32, ChannelId,
    EnergyPolicyConfig, EnergyPolicyMode, EnergyTelemetrySnapshot, EvmAddress,
    InternalWorkAllowance, LedgerVersion, ModelIntegrityManifest, MonotonicTimestampMs, OrderId,
    OutboxEntryId, OwnerFence, PreparedInputId, PrimitivesError, RunId, RunReceipt,
    RunTerminalStatus, SessionId, SettlementRail, SignatureBytes, SignedUsageAck, SignedVoucher,
    TokenAtomicAmount, UsageAttestation, UsageTariff, WallClockUnixMs, WorkerEpoch,
    X402AuthorizationPayload,
};
use autarqia_subsystem_api::{
    BufiEntryKind, BufiOutboxEntry, ChannelLedgerView, RunLedgerView, SettlementBatchItem,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockContext {
    pub monotonic_ms: MonotonicTimestampMs,
    pub unix_ms: WallClockUnixMs,
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeError {
    #[error("primitives error: {0}")]
    Primitives(String),
    #[error("energy admission denied: {0}")]
    EnergyAdmissionDenied(String),
    #[error("worker not ready or busy: {0}")]
    WorkerNotReady(String),
    #[error("stale epoch or fence: {0}")]
    StaleEpochOrFence(String),
    #[error("context or model constraint violated: {0}")]
    ModelConstraintViolated(String),
    #[error("rail mismatch: session rail {session_rail:?} != requested rail {requested_rail:?}")]
    RailMismatch {
        session_rail: SettlementRail,
        requested_rail: SettlementRail,
    },
    #[error("duplicate order_id {0} with conflicting parameters or rail")]
    DuplicateOrderConflict(String),
    #[error("channel {0} not found")]
    ChannelNotFound(String),
    #[error("session {0} not found")]
    SessionNotFound(String),
    #[error("run {0} not found")]
    RunNotFound(String),
    #[error("channel {0} is closing or closed")]
    ChannelClosingOrClosed(String),
    #[error("insufficient covered budget or deposit: {0}")]
    InsufficientBudget(String),
    #[error("invalid signature or signer mismatch: {0}")]
    InvalidSignature(String),
    #[error("prefix commitment mismatch at seq {seq}: expected {expected}, got {actual}")]
    PrefixCommitmentMismatch {
        seq: u32,
        expected: String,
        actual: String,
    },
    #[error("invalid token sequence: expected {expected}, got {actual}")]
    InvalidTokenSequence { expected: u32, actual: u32 },
}

impl From<PrimitivesError> for RuntimeError {
    fn from(value: PrimitivesError) -> Self {
        Self::Primitives(value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: SessionId,
    pub rail: SettlementRail,
    pub channel_id: ChannelId,
    pub payer: EvmAddress,
    pub buyer_signer: EvmAddress,
    pub bearer_token_sha256: Bytes32,
    pub created_at_unix_ms: WallClockUnixMs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRecord {
    pub order_id: OrderId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub rail: SettlementRail,
    pub prompt_sha256: Bytes32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRunRecord {
    pub view: RunLedgerView,
    pub prefix_history: BTreeMap<u32, Bytes32>,
    pub prior_channel_accrued_atomic: TokenAtomicAmount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub chain_id: u64,
    pub ando_mppi_proxy: EvmAddress,
    pub gateway_wallet_proxy: EvmAddress,
    pub usdc_token: EvmAddress,
    pub provider_payee: EvmAddress,
    pub provider_operator: EvmAddress,
    pub provider_signer: EvmAddress,
    pub tariff: UsageTariff,
    pub energy_policy: EnergyPolicyConfig,
    pub model_manifest: ModelIntegrityManifest,
    pub allowance_ttl_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeState {
    pub config: RuntimeConfig,
    pub ledger_version: LedgerVersion,
    pub worker_epoch: WorkerEpoch,
    pub owner_fence: OwnerFence,
    pub admission_epoch: AdmissionEpoch,
    pub worker_ready: bool,
    pub active_run_id: Option<RunId>,
    pub latest_energy: Option<EnergyTelemetrySnapshot>,
    pub channels: BTreeMap<ChannelId, ChannelLedgerView>,
    pub sessions: BTreeMap<SessionId, SessionRecord>,
    pub orders: BTreeMap<OrderId, OrderRecord>,
    pub runs: BTreeMap<RunId, ActiveRunRecord>,
    pub outbox: BTreeMap<OutboxEntryId, BufiOutboxEntry>,
    pub consumed_x402_nonces: BTreeMap<Bytes32, RunId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeEffect {
    PersistStateCheckpoint {
        ledger_version: LedgerVersion,
    },
    StartWorkerGeneration {
        run_id: RunId,
        prepared_input_id: PreparedInputId,
        initial_allowance: InternalWorkAllowance,
    },
    UpdateWorkerAllowance {
        allowance: InternalWorkAllowance,
    },
    CancelWorkerRun {
        run_id: RunId,
        worker_epoch: WorkerEpoch,
        owner_fence: OwnerFence,
        admission_epoch: AdmissionEpoch,
        reason: String,
    },
    DrainWorker {
        worker_epoch: WorkerEpoch,
        owner_fence: OwnerFence,
        reason: String,
    },
    EmitUsageAttestation {
        run_id: RunId,
        attestation: UsageAttestation,
        require_ack_by_output_seq: u32,
    },
    EmitPaymentRequired {
        run_id: RunId,
        current_output_seq: u32,
        cumulative_due_atomic: TokenAtomicAmount,
        covered_voucher_cumulative_atomic: TokenAtomicAmount,
        uncovered_exposure_cap_atomic: TokenAtomicAmount,
        reason: String,
    },
    SignAndFinalizeReceipt {
        run_id: RunId,
        unsigned_receipt: RunReceipt,
    },
    QueueSettlementItem {
        rail: SettlementRail,
        item: SettlementBatchItem,
    },
    AppendBufiOutbox {
        entry: BufiOutboxEntry,
    },
}

impl RuntimeState {
    pub fn new(config: RuntimeConfig, worker_epoch: WorkerEpoch, owner_fence: OwnerFence) -> Self {
        Self {
            config,
            ledger_version: LedgerVersion::new(1),
            worker_epoch,
            owner_fence,
            admission_epoch: AdmissionEpoch::new(1),
            worker_ready: true,
            active_run_id: None,
            latest_energy: None,
            channels: BTreeMap::new(),
            sessions: BTreeMap::new(),
            orders: BTreeMap::new(),
            runs: BTreeMap::new(),
            outbox: BTreeMap::new(),
            consumed_x402_nonces: BTreeMap::new(),
        }
    }

    fn bump_ledger(&mut self) -> LedgerVersion {
        self.ledger_version = self.ledger_version.next();
        self.ledger_version
    }

    pub fn evaluate_energy_for_new_run(
        &self,
        clock: ClockContext,
    ) -> Result<&EnergyTelemetrySnapshot, RuntimeError> {
        let snap = self.latest_energy.as_ref().ok_or_else(|| {
            RuntimeError::EnergyAdmissionDenied("missing energy telemetry snapshot".to_owned())
        })?;
        let age_ms = clock
            .monotonic_ms
            .0
            .saturating_sub(snap.measured_at_monotonic_ms.0);
        if age_ms > self.config.energy_policy.max_telemetry_staleness_ms {
            return Err(RuntimeError::EnergyAdmissionDenied(format!(
                "stale energy telemetry: age_ms={age_ms} > max={}",
                self.config.energy_policy.max_telemetry_staleness_ms
            )));
        }

        let policy = &self.config.energy_policy;
        if snap.battery_soc_bps < policy.hard_floor_soc_bps_abort {
            return Err(RuntimeError::EnergyAdmissionDenied(format!(
                "battery SoC {} bps below hard floor {} bps",
                snap.battery_soc_bps.0, policy.hard_floor_soc_bps_abort.0
            )));
        }

        match policy.mode {
            EnergyPolicyMode::SolarSurplusOnly => {
                if snap.grid_power_mw.0 > 0 {
                    return Err(RuntimeError::EnergyAdmissionDenied(
                        "grid import detected under solar_surplus_only mode".to_owned(),
                    ));
                }
                if snap.battery_soc_bps < policy.min_battery_soc_bps_for_new_run {
                    return Err(RuntimeError::EnergyAdmissionDenied(format!(
                        "battery SoC {} bps below minimum {} bps for solar_surplus_only",
                        snap.battery_soc_bps.0, policy.min_battery_soc_bps_for_new_run.0
                    )));
                }
                let required_surplus =
                    policy.min_surplus_mw_for_new_run.0 + policy.estimated_inference_load_mw.0;
                if snap.net_surplus_mw().0 < required_surplus {
                    return Err(RuntimeError::EnergyAdmissionDenied(format!(
                        "net solar surplus {} mW below required {} mW",
                        snap.net_surplus_mw().0,
                        required_surplus
                    )));
                }
            }
            EnergyPolicyMode::SolarPlusReservedBattery => {
                if snap.battery_soc_bps < policy.min_battery_soc_bps_for_new_run {
                    return Err(RuntimeError::EnergyAdmissionDenied(format!(
                        "battery SoC {} bps below reserved floor {} bps",
                        snap.battery_soc_bps.0, policy.min_battery_soc_bps_for_new_run.0
                    )));
                }
                let has_surplus =
                    snap.net_surplus_mw().0 >= policy.min_surplus_mw_for_new_run.0;
                let has_battery_headroom = snap.battery_soc_bps.0
                    >= policy.min_battery_soc_bps_for_new_run.0.saturating_add(500);
                if !has_surplus && !has_battery_headroom {
                    return Err(RuntimeError::EnergyAdmissionDenied(
                        "insufficient solar surplus and battery reserve headroom".to_owned(),
                    ));
                }
            }
            EnergyPolicyMode::GridAssisted => {}
        }

        Ok(snap)
    }

    pub fn ingest_energy_telemetry(
        &mut self,
        snapshot: EnergyTelemetrySnapshot,
        clock: ClockContext,
    ) -> Vec<RuntimeEffect> {
        let should_abort_active = match self.config.energy_policy.mode {
            EnergyPolicyMode::SolarSurplusOnly => {
                snapshot.battery_soc_bps < self.config.energy_policy.hard_floor_soc_bps_abort
                    || snapshot.net_surplus_mw().0 < 0
                    || snapshot.grid_power_mw.0 > 0
            }
            EnergyPolicyMode::SolarPlusReservedBattery | EnergyPolicyMode::GridAssisted => {
                snapshot.battery_soc_bps < self.config.energy_policy.hard_floor_soc_bps_abort
            }
        };

        self.latest_energy = Some(snapshot);
        let mut effects = Vec::new();

        if should_abort_active {
            self.admission_epoch = self.admission_epoch.next();
            let lv = self.bump_ledger();
            if let Some(active_id) = self.active_run_id.clone() {
                if let Some(effs) = self.abort_run_internal(
                    &active_id,
                    RunTerminalStatus::EnergyAborted,
                    "energy_policy_floor_breached",
                    clock,
                ) {
                    effects.extend(effs);
                }
            }
            effects.push(RuntimeEffect::PersistStateCheckpoint { ledger_version: lv });
        }

        effects
    }

    pub fn verify_and_apply_voucher(
        &mut self,
        channel_id: ChannelId,
        voucher: &SignedVoucher,
    ) -> Result<TokenAtomicAmount, RuntimeError> {
        if voucher.channel_id != channel_id {
            return Err(RuntimeError::InvalidSignature(
                "voucher channel_id mismatch".to_owned(),
            ));
        }
        let domain =
            ando_mppi_domain_separator(self.config.chain_id, self.config.ando_mppi_proxy);
        let digest =
            ando_mppi_voucher_digest(domain, voucher.channel_id, voucher.cumulative_amount);
        let recovered = recover_eip712_signer(digest, &voucher.signature)
            .map_err(|e| RuntimeError::InvalidSignature(e.to_string()))?;

        let channel = self
            .channels
            .get_mut(&channel_id)
            .ok_or_else(|| RuntimeError::ChannelNotFound(channel_id.to_hex()))?;
        if channel.closed || channel.close_requested_at_unix_s.is_some() {
            return Err(RuntimeError::ChannelClosingOrClosed(channel_id.to_hex()));
        }
        if recovered != channel.authorized_signer {
            return Err(RuntimeError::InvalidSignature(format!(
                "voucher signer {} != channel authorized_signer {}",
                recovered, channel.authorized_signer
            )));
        }
        if voucher.cumulative_amount > channel.deposit_atomic {
            return Err(RuntimeError::InsufficientBudget(format!(
                "voucher cumulative_amount {} exceeds channel deposit {}",
                voucher.cumulative_amount, channel.deposit_atomic
            )));
        }
        if voucher.cumulative_amount < channel.highest_verified_voucher_atomic {
            return Err(RuntimeError::InsufficientBudget(format!(
                "non-monotonic voucher: {} < highest {}",
                voucher.cumulative_amount, channel.highest_verified_voucher_atomic
            )));
        }
        if voucher.cumulative_amount > channel.highest_verified_voucher_atomic {
            channel.highest_verified_voucher_atomic = voucher.cumulative_amount;
            channel.highest_voucher_signature = Some(voucher.signature.clone());
        }
        Ok(channel.highest_verified_voucher_atomic)
    }

    pub fn verify_and_apply_x402_authorization(
        &mut self,
        channel_id: ChannelId,
        auth: &X402AuthorizationPayload,
        run_id_hint: Option<RunId>,
        clock: ClockContext,
    ) -> Result<TokenAtomicAmount, RuntimeError> {
        if self.consumed_x402_nonces.contains_key(&auth.nonce) {
            return Err(RuntimeError::DuplicateOrderConflict(format!(
                "x402 authorization nonce {} already consumed",
                auth.nonce
            )));
        }
        let now_s = clock.unix_ms.0 / 1000;
        if now_s < auth.valid_after || now_s > auth.valid_before {
            return Err(RuntimeError::InvalidSignature(format!(
                "x402 authorization outside validity window [{}, {}] at {}",
                auth.valid_after, auth.valid_before, now_s
            )));
        }
        if auth.to != self.config.provider_payee {
            return Err(RuntimeError::InvalidSignature(format!(
                "x402 authorization recipient {} != provider_payee {}",
                auth.to, self.config.provider_payee
            )));
        }
        let domain = gateway_wallet_batched_domain_separator(
            self.config.chain_id,
            self.config.gateway_wallet_proxy,
        );
        let digest = x402_transfer_with_authorization_digest(domain, auth);
        let recovered = recover_eip712_signer(digest, &auth.signature)
            .map_err(|e| RuntimeError::InvalidSignature(e.to_string()))?;

        let channel = self
            .channels
            .get_mut(&channel_id)
            .ok_or_else(|| RuntimeError::ChannelNotFound(channel_id.to_hex()))?;
        if recovered != channel.authorized_signer && recovered != channel.payer {
            return Err(RuntimeError::InvalidSignature(format!(
                "x402 signer {} != payer {}",
                recovered, channel.payer
            )));
        }
        let new_covered = channel
            .highest_verified_voucher_atomic
            .checked_add(auth.value)?;
        channel.deposit_atomic = channel.deposit_atomic.max(new_covered);
        channel.highest_verified_voucher_atomic = new_covered;
        channel.highest_voucher_signature = Some(auth.signature.clone());
        if let Some(rid) = run_id_hint {
            self.consumed_x402_nonces.insert(auth.nonce, rid);
        } else {
            self.consumed_x402_nonces
                .insert(auth.nonce, RunId::new("session_open"));
        }
        Ok(new_covered)
    }

    pub fn register_session(
        &mut self,
        session_id: SessionId,
        rail: SettlementRail,
        channel_id: ChannelId,
        payer: EvmAddress,
        buyer_signer: EvmAddress,
        onchain_deposit_atomic: TokenAtomicAmount,
        bearer_token_sha256: Bytes32,
        initial_voucher: Option<&SignedVoucher>,
        x402_auth: Option<&X402AuthorizationPayload>,
        clock: ClockContext,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        if initial_voucher.is_some() && x402_auth.is_some() {
            return Err(RuntimeError::RailMismatch {
                session_rail: rail,
                requested_rail: SettlementRail::CircleGatewayX402,
            });
        }
        let entry = self.channels.entry(channel_id).or_insert(ChannelLedgerView {
            channel_id,
            rail,
            payer,
            payee: self.config.provider_payee,
            authorized_signer: buyer_signer,
            deposit_atomic: onchain_deposit_atomic,
            highest_verified_voucher_atomic: TokenAtomicAmount::ZERO,
            highest_voucher_signature: None,
            settled_onchain_atomic: TokenAtomicAmount::ZERO,
            accrued_cumulatively_atomic: TokenAtomicAmount::ZERO,
            acknowledged_cumulatively_atomic: TokenAtomicAmount::ZERO,
            close_requested_at_unix_s: None,
            closed: false,
        });
        if entry.rail != rail {
            return Err(RuntimeError::RailMismatch {
                session_rail: entry.rail,
                requested_rail: rail,
            });
        }
        if onchain_deposit_atomic > entry.deposit_atomic {
            entry.deposit_atomic = onchain_deposit_atomic;
        }

        match rail {
            SettlementRail::AndoMppiArcV1 => {
                if x402_auth.is_some() {
                    return Err(RuntimeError::RailMismatch {
                        session_rail: rail,
                        requested_rail: SettlementRail::CircleGatewayX402,
                    });
                }
                if let Some(v) = initial_voucher {
                    self.verify_and_apply_voucher(channel_id, v)?;
                }
            }
            SettlementRail::CircleGatewayX402 => {
                if initial_voucher.is_some() {
                    return Err(RuntimeError::RailMismatch {
                        session_rail: rail,
                        requested_rail: SettlementRail::AndoMppiArcV1,
                    });
                }
                if let Some(auth) = x402_auth {
                    self.verify_and_apply_x402_authorization(channel_id, auth, None, clock)?;
                }
            }
        }

        self.sessions.insert(
            session_id.clone(),
            SessionRecord {
                session_id,
                rail,
                channel_id,
                payer,
                buyer_signer,
                bearer_token_sha256,
                created_at_unix_ms: clock.unix_ms,
            },
        );
        let lv = self.bump_ledger();
        Ok(vec![RuntimeEffect::PersistStateCheckpoint {
            ledger_version: lv,
        }])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn admit_run(
        &mut self,
        run_id: RunId,
        session_id: SessionId,
        order_id: OrderId,
        prepared_input_id: PreparedInputId,
        prompt_sha256: Bytes32,
        input_token_count: u32,
        requested_max_output_tokens: u32,
        initial_voucher: Option<&SignedVoucher>,
        x402_auth: Option<&X402AuthorizationPayload>,
        clock: ClockContext,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| RuntimeError::SessionNotFound(session_id.to_string()))?
            .clone();

        if let Some(existing) = self.orders.get(&order_id) {
            if existing.session_id != session_id
                || existing.rail != session.rail
                || existing.prompt_sha256 != prompt_sha256
            {
                return Err(RuntimeError::DuplicateOrderConflict(order_id.to_string()));
            }
            return Err(RuntimeError::DuplicateOrderConflict(format!(
                "order {} already admitted as run {}",
                order_id, existing.run_id
            )));
        }

        self.evaluate_energy_for_new_run(clock)?;

        if !self.worker_ready || self.active_run_id.is_some() {
            return Err(RuntimeError::WorkerNotReady(
                "worker is busy with an active run or not ready".to_owned(),
            ));
        }

        if self.config.model_manifest.trust_remote_code {
            return Err(RuntimeError::ModelConstraintViolated(
                "trust_remote_code=true is rejected".to_owned(),
            ));
        }
        if input_token_count == 0
            || input_token_count > self.config.model_manifest.max_prefill_tokens
        {
            return Err(RuntimeError::ModelConstraintViolated(format!(
                "input_token_count {input_token_count} out of range 1..={}",
                self.config.model_manifest.max_prefill_tokens
            )));
        }
        let max_out = requested_max_output_tokens.max(1);
        if input_token_count.saturating_add(max_out)
            > self.config.model_manifest.tested_context_limit
        {
            return Err(RuntimeError::ModelConstraintViolated(format!(
                "context {} exceeds tested_context_limit {}",
                input_token_count.saturating_add(max_out),
                self.config.model_manifest.tested_context_limit
            )));
        }

        match session.rail {
            SettlementRail::AndoMppiArcV1 => {
                if x402_auth.is_some() {
                    return Err(RuntimeError::RailMismatch {
                        session_rail: session.rail,
                        requested_rail: SettlementRail::CircleGatewayX402,
                    });
                }
                if let Some(v) = initial_voucher {
                    self.verify_and_apply_voucher(session.channel_id, v)?;
                }
            }
            SettlementRail::CircleGatewayX402 => {
                if initial_voucher.is_some() {
                    return Err(RuntimeError::RailMismatch {
                        session_rail: session.rail,
                        requested_rail: SettlementRail::AndoMppiArcV1,
                    });
                }
                if let Some(auth) = x402_auth {
                    self.verify_and_apply_x402_authorization(
                        session.channel_id,
                        auth,
                        Some(run_id.clone()),
                        clock,
                    )?;
                }
            }
        }

        let channel = self
            .channels
            .get(&session.channel_id)
            .ok_or_else(|| RuntimeError::ChannelNotFound(session.channel_id.to_hex()))?;
        if channel.closed || channel.close_requested_at_unix_s.is_some() {
            return Err(RuntimeError::ChannelClosingOrClosed(
                session.channel_id.to_hex(),
            ));
        }

        let prior_accrued = channel.accrued_cumulatively_atomic;
        let available_covered_for_run = channel
            .highest_verified_voucher_atomic
            .saturating_sub(prior_accrued);

        let budget_max_seq_excl = self.config.tariff.max_output_seq_exclusive_for_budget(
            input_token_count,
            available_covered_for_run,
            max_out,
        )?;
        if budget_max_seq_excl == 0 {
            return Err(RuntimeError::InsufficientBudget(
                "prefill cost exceeds covered voucher headroom plus uncovered exposure cap"
                    .to_owned(),
            ));
        }

        let stride_window_excl = self.config.tariff.ack_stride_tokens.saturating_add(1);
        let initial_max_seq_excl = budget_max_seq_excl
            .min(stride_window_excl)
            .min(max_out.saturating_add(1));

        let lv = self.bump_ledger();
        let initial_allowance = InternalWorkAllowance {
            run_id: run_id.clone(),
            worker_epoch: self.worker_epoch,
            owner_fence: self.owner_fence,
            admission_epoch: self.admission_epoch,
            allowance_seq: AllowanceSeq::new(1),
            max_output_seq_exclusive: initial_max_seq_excl,
            expires_at_monotonic_ms: MonotonicTimestampMs::new(
                clock.monotonic_ms.0.saturating_add(self.config.allowance_ttl_ms),
            ),
            ledger_version: lv,
        };

        let init_prefix = compute_initial_prefix_state(run_id.as_str());
        let mut prefix_history = BTreeMap::new();
        prefix_history.insert(0, init_prefix);

        let view = RunLedgerView {
            run_id: run_id.clone(),
            session_id: session_id.clone(),
            order_id: order_id.clone(),
            rail: session.rail,
            channel_id: session.channel_id,
            prepared_input_id: prepared_input_id.clone(),
            prompt_sha256,
            input_tokens_processed: input_token_count,
            output_tokens_generated: 0,
            accrued_atomic: TokenAtomicAmount::ZERO,
            acknowledged_output_seq: 0,
            acknowledged_atomic: TokenAtomicAmount::ZERO,
            latest_prefix_commitment: init_prefix,
            active_allowance: initial_allowance.clone(),
            terminal_status: None,
            receipt: None,
        };

        self.orders.insert(
            order_id.clone(),
            OrderRecord {
                order_id,
                session_id,
                run_id: run_id.clone(),
                rail: session.rail,
                prompt_sha256,
            },
        );
        self.runs.insert(
            run_id.clone(),
            ActiveRunRecord {
                view,
                prefix_history,
                prior_channel_accrued_atomic: prior_accrued,
            },
        );
        self.active_run_id = Some(run_id.clone());

        Ok(vec![
            RuntimeEffect::PersistStateCheckpoint { ledger_version: lv },
            RuntimeEffect::StartWorkerGeneration {
                run_id,
                prepared_input_id,
                initial_allowance,
            },
        ])
    }

    pub fn on_worker_prefill_completed(
        &mut self,
        run_id: &RunId,
        input_tokens_processed: u32,
    ) -> Result<TokenAtomicAmount, RuntimeError> {
        let rec = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| RuntimeError::RunNotFound(run_id.to_string()))?;
        rec.view.input_tokens_processed = input_tokens_processed;
        let prefill_due = self
            .config
            .tariff
            .compute_cumulative_due(input_tokens_processed, 0)?;
        rec.view.accrued_atomic = prefill_due;
        if let Some(ch) = self.channels.get_mut(&rec.view.channel_id) {
            ch.accrued_cumulatively_atomic =
                rec.prior_channel_accrued_atomic.checked_add(prefill_due)?;
        }
        self.bump_ledger();
        Ok(prefill_due)
    }

    pub fn on_worker_token_delta(
        &mut self,
        run_id: &RunId,
        output_seq: u32,
        token_id: u32,
        text_delta: &str,
        worker_prefix_commitment: Bytes32,
        clock: ClockContext,
    ) -> Result<(TokenAtomicAmount, Vec<RuntimeEffect>), RuntimeError> {
        let rec = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| RuntimeError::RunNotFound(run_id.to_string()))?;
        let expected_seq = rec.view.output_tokens_generated.saturating_add(1);
        if output_seq != expected_seq {
            return Err(RuntimeError::InvalidTokenSequence {
                expected: expected_seq,
                actual: output_seq,
            });
        }
        let expected_commitment = advance_prefix_state(
            rec.view.latest_prefix_commitment,
            output_seq,
            token_id,
            text_delta,
        );
        if expected_commitment != worker_prefix_commitment {
            return Err(RuntimeError::PrefixCommitmentMismatch {
                seq: output_seq,
                expected: expected_commitment.to_hex(),
                actual: worker_prefix_commitment.to_hex(),
            });
        }

        rec.view.output_tokens_generated = output_seq;
        rec.view.latest_prefix_commitment = expected_commitment;
        rec.prefix_history.insert(output_seq, expected_commitment);

        let due = self
            .config
            .tariff
            .compute_cumulative_due(rec.view.input_tokens_processed, output_seq)?;
        rec.view.accrued_atomic = due;

        let channel_id = rec.view.channel_id;
        let session_id = rec.view.session_id.clone();
        let max_seq_excl = rec.view.active_allowance.max_output_seq_exclusive;
        let prior_accrued = rec.prior_channel_accrued_atomic;

        if let Some(ch) = self.channels.get_mut(&channel_id) {
            ch.accrued_cumulatively_atomic = prior_accrued.checked_add(due)?;
        }

        let mut effects = Vec::new();
        let stride = self.config.tariff.ack_stride_tokens.max(1);
        let at_stride_boundary = output_seq % stride == 0;
        let at_allowance_boundary = output_seq.saturating_add(1) >= max_seq_excl;

        if at_stride_boundary || at_allowance_boundary {
            let att = UsageAttestation {
                session_id,
                run_id: run_id.clone(),
                channel_id,
                input_tokens_processed: self.runs[run_id].view.input_tokens_processed,
                output_seq,
                cumulative_due_atomic: due,
                prefix_commitment: expected_commitment,
                model_manifest_hash: self.config.model_manifest.manifest_commitment(),
                admission_epoch: self.admission_epoch,
                issued_at_unix_ms: clock.unix_ms,
            };
            effects.push(RuntimeEffect::EmitUsageAttestation {
                run_id: run_id.clone(),
                attestation: att,
                require_ack_by_output_seq: max_seq_excl.saturating_sub(1),
            });
        }

        self.bump_ledger();
        Ok((due, effects))
    }

    pub fn on_buyer_usage_ack(
        &mut self,
        run_id: &RunId,
        signed_ack: &SignedUsageAck,
        requested_max_output_tokens: u32,
        clock: ClockContext,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let ack = &signed_ack.ack;
        if &ack.run_id != run_id {
            return Err(RuntimeError::InvalidSignature(
                "ack run_id does not match target run".to_owned(),
            ));
        }

        let session = self
            .sessions
            .get(&ack.session_id)
            .ok_or_else(|| RuntimeError::SessionNotFound(ack.session_id.to_string()))?
            .clone();
        let domain =
            autarqia_mppi_run_domain_separator(self.config.chain_id, self.config.ando_mppi_proxy);
        let digest = usage_ack_digest(domain, ack);
        let recovered = recover_eip712_signer(digest, &signed_ack.signature)
            .map_err(|e| RuntimeError::InvalidSignature(e.to_string()))?;
        if recovered != session.buyer_signer {
            return Err(RuntimeError::InvalidSignature(format!(
                "UsageAck signer {} != session buyer_signer {}",
                recovered, session.buyer_signer
            )));
        }

        let (channel_id, prior_accrued, input_tokens) = {
            let rec = self
                .runs
                .get(run_id)
                .ok_or_else(|| RuntimeError::RunNotFound(run_id.to_string()))?;
            let expected_prefix = rec
                .prefix_history
                .get(&ack.acknowledged_output_seq)
                .copied()
                .ok_or_else(|| RuntimeError::InvalidTokenSequence {
                    expected: rec.view.output_tokens_generated,
                    actual: ack.acknowledged_output_seq,
                })?;
            if expected_prefix != ack.prefix_commitment {
                return Err(RuntimeError::PrefixCommitmentMismatch {
                    seq: ack.acknowledged_output_seq,
                    expected: expected_prefix.to_hex(),
                    actual: ack.prefix_commitment.to_hex(),
                });
            }
            (
                rec.view.channel_id,
                rec.prior_channel_accrued_atomic,
                rec.view.input_tokens_processed,
            )
        };

        let mut effects = Vec::new();
        if let Some(voucher) = &signed_ack.voucher {
            if session.rail != SettlementRail::AndoMppiArcV1 {
                return Err(RuntimeError::RailMismatch {
                    session_rail: session.rail,
                    requested_rail: SettlementRail::AndoMppiArcV1,
                });
            }
            let high_water = self.verify_and_apply_voucher(channel_id, voucher)?;
            effects.push(RuntimeEffect::QueueSettlementItem {
                rail: SettlementRail::AndoMppiArcV1,
                item: SettlementBatchItem {
                    channel_id,
                    cumulative_amount: high_water,
                    signature: voucher.signature.clone(),
                },
            });
        }

        let expected_due_at_ack = self
            .config
            .tariff
            .compute_cumulative_due(input_tokens, ack.acknowledged_output_seq)?;
        if ack.acknowledged_cumulative_due_atomic < expected_due_at_ack {
            return Err(RuntimeError::InsufficientBudget(format!(
                "acknowledged_cumulative_due_atomic {} < expected {}",
                ack.acknowledged_cumulative_due_atomic, expected_due_at_ack
            )));
        }

        let covered_for_run = {
            let ch = self
                .channels
                .get_mut(&channel_id)
                .ok_or_else(|| RuntimeError::ChannelNotFound(channel_id.to_hex()))?;
            let next_ack_cum = prior_accrued.checked_add(ack.acknowledged_cumulative_due_atomic)?;
            if next_ack_cum > ch.acknowledged_cumulatively_atomic {
                ch.acknowledged_cumulatively_atomic = next_ack_cum;
            }
            ch.highest_verified_voucher_atomic
                .saturating_sub(prior_accrued)
        };

        let budget_max_seq_excl = self.config.tariff.max_output_seq_exclusive_for_budget(
            input_tokens,
            covered_for_run,
            requested_max_output_tokens,
        )?;

        let lv = self.bump_ledger();
        let rec = self.runs.get_mut(run_id).unwrap();
        if ack.acknowledged_output_seq > rec.view.acknowledged_output_seq {
            rec.view.acknowledged_output_seq = ack.acknowledged_output_seq;
        }
        if ack.acknowledged_cumulative_due_atomic > rec.view.acknowledged_atomic {
            rec.view.acknowledged_atomic = ack.acknowledged_cumulative_due_atomic;
        }

        let next_stride_cap = ack
            .acknowledged_output_seq
            .saturating_add(self.config.tariff.ack_stride_tokens.saturating_mul(2))
            .saturating_add(1);
        let new_max_seq_excl = budget_max_seq_excl
            .min(next_stride_cap)
            .min(requested_max_output_tokens.saturating_add(1));

        if new_max_seq_excl > rec.view.active_allowance.max_output_seq_exclusive {
            let new_allowance = InternalWorkAllowance {
                run_id: run_id.clone(),
                worker_epoch: self.worker_epoch,
                owner_fence: self.owner_fence,
                admission_epoch: self.admission_epoch,
                allowance_seq: rec.view.active_allowance.allowance_seq.next(),
                max_output_seq_exclusive: new_max_seq_excl,
                expires_at_monotonic_ms: MonotonicTimestampMs::new(
                    clock.monotonic_ms.0.saturating_add(self.config.allowance_ttl_ms),
                ),
                ledger_version: lv,
            };
            rec.view.active_allowance = new_allowance.clone();
            effects.push(RuntimeEffect::UpdateWorkerAllowance {
                allowance: new_allowance,
            });
        }
        effects.insert(
            0,
            RuntimeEffect::PersistStateCheckpoint { ledger_version: lv },
        );
        Ok(effects)
    }

    pub fn finalize_run_from_worker(
        &mut self,
        run_id: &RunId,
        worker_status: &str,
        output_tokens_generated: u32,
        final_prefix_commitment: Bytes32,
        clock: ClockContext,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let terminal_status = match worker_status {
            "completed" => RunTerminalStatus::Completed,
            "completed_max_tokens" => RunTerminalStatus::CompletedMaxTokens,
            "allowance_exhausted" | "allowance_expired" => RunTerminalStatus::PausedPaymentRequired,
            "drained" => RunTerminalStatus::EnergyAborted,
            "cancelled" => RunTerminalStatus::CancelledByBuyer,
            _ => RunTerminalStatus::WorkerError,
        };

        let rec = self
            .runs
            .get_mut(run_id)
            .ok_or_else(|| RuntimeError::RunNotFound(run_id.to_string()))?;
        rec.view.output_tokens_generated = output_tokens_generated;
        rec.view.latest_prefix_commitment = final_prefix_commitment;
        let final_due = self.config.tariff.compute_cumulative_due(
            rec.view.input_tokens_processed,
            output_tokens_generated,
        )?;
        rec.view.accrued_atomic = final_due;
        rec.view.terminal_status = Some(terminal_status);

        let channel_id = rec.view.channel_id;
        let session_id = rec.view.session_id.clone();
        let rail = rec.view.rail;
        let input_tokens = rec.view.input_tokens_processed;
        let ack_atomic = rec.view.acknowledged_atomic;
        let prior_accrued = rec.prior_channel_accrued_atomic;

        let (covered_voucher, settled_onchain) = if let Some(ch) = self.channels.get_mut(&channel_id)
        {
            ch.accrued_cumulatively_atomic = prior_accrued.checked_add(final_due)?;
            (
                ch.highest_verified_voucher_atomic,
                ch.settled_onchain_atomic,
            )
        } else {
            (TokenAtomicAmount::ZERO, TokenAtomicAmount::ZERO)
        };

        if self.active_run_id.as_ref() == Some(run_id) {
            self.active_run_id = None;
        }

        let lv = self.bump_ledger();
        let unsigned_receipt = RunReceipt {
            receipt_id: autarqia_primitives::ReceiptId::new(format!("rcpt_{}", run_id)),
            session_id,
            run_id: run_id.clone(),
            rail,
            channel_id,
            input_tokens_processed: input_tokens,
            output_tokens_generated,
            cumulative_due_atomic: final_due,
            acknowledged_cumulative_due_atomic: ack_atomic,
            covered_voucher_cumulative_atomic: covered_voucher,
            settled_onchain_cumulative_atomic: settled_onchain,
            final_prefix_commitment,
            model_manifest_hash: self.config.model_manifest.manifest_commitment(),
            energy_mode: self.config.energy_policy.mode,
            terminal_status,
            settlement_tx_hash: None,
            issued_at_unix_ms: clock.unix_ms,
            provider_signer: self.config.provider_signer,
            provider_signature: SignatureBytes::default(),
        };

        let outbox_id = OutboxEntryId::new(format!("bufi_rcpt_{}_{}", run_id, lv.0));
        let outbox_entry = BufiOutboxEntry {
            entry_id: outbox_id.clone(),
            ledger_version: lv,
            kind: BufiEntryKind::RunReceiptCommitted,
            run_id: Some(run_id.clone()),
            channel_id: Some(channel_id),
            payload_json: serde_json::to_string(&unsigned_receipt).unwrap_or_default(),
            created_at_unix_ms: clock.unix_ms,
            delivered: false,
            delivery_attempts: 0,
        };
        self.outbox.insert(outbox_id, outbox_entry.clone());

        let mut effects = vec![RuntimeEffect::PersistStateCheckpoint { ledger_version: lv }];
        if terminal_status == RunTerminalStatus::PausedPaymentRequired {
            effects.push(RuntimeEffect::EmitPaymentRequired {
                run_id: run_id.clone(),
                current_output_seq: output_tokens_generated,
                cumulative_due_atomic: final_due,
                covered_voucher_cumulative_atomic: covered_voucher,
                uncovered_exposure_cap_atomic: self.config.tariff.uncovered_exposure_cap,
                reason: "allowance_exhausted_without_timely_ack_or_voucher".to_owned(),
            });
        }
        effects.push(RuntimeEffect::SignAndFinalizeReceipt {
            run_id: run_id.clone(),
            unsigned_receipt,
        });
        effects.push(RuntimeEffect::AppendBufiOutbox {
            entry: outbox_entry,
        });
        Ok(effects)
    }

    pub fn reconcile_onchain_channel_state(
        &mut self,
        channel_id: ChannelId,
        onchain_deposit_atomic: TokenAtomicAmount,
        onchain_settled_atomic: TokenAtomicAmount,
        close_requested_at_unix_s: Option<u64>,
        closed: bool,
    ) -> Result<Vec<RuntimeEffect>, RuntimeError> {
        let ch = self
            .channels
            .get_mut(&channel_id)
            .ok_or_else(|| RuntimeError::ChannelNotFound(channel_id.to_hex()))?;
        ch.deposit_atomic = onchain_deposit_atomic;
        if onchain_settled_atomic > ch.settled_onchain_atomic {
            ch.settled_onchain_atomic = onchain_settled_atomic;
        }
        ch.close_requested_at_unix_s = close_requested_at_unix_s;
        ch.closed = closed;

        let mut effects = Vec::new();
        if (close_requested_at_unix_s.is_some() || closed)
            && ch.highest_verified_voucher_atomic > ch.settled_onchain_atomic
        {
            if let Some(sig) = ch.highest_voucher_signature.clone() {
                effects.push(RuntimeEffect::QueueSettlementItem {
                    rail: ch.rail,
                    item: SettlementBatchItem {
                        channel_id,
                        cumulative_amount: ch.highest_verified_voucher_atomic,
                        signature: sig,
                    },
                });
            }
        }
        let lv = self.bump_ledger();
        effects.insert(
            0,
            RuntimeEffect::PersistStateCheckpoint { ledger_version: lv },
        );
        Ok(effects)
    }

    fn abort_run_internal(
        &mut self,
        run_id: &RunId,
        status: RunTerminalStatus,
        reason: &str,
        _clock: ClockContext,
    ) -> Option<Vec<RuntimeEffect>> {
        let rec = self.runs.get_mut(run_id)?;
        if rec.view.terminal_status.is_some() {
            return None;
        }
        rec.view.terminal_status = Some(status);
        Some(vec![
            RuntimeEffect::DrainWorker {
                worker_epoch: self.worker_epoch,
                owner_fence: self.owner_fence,
                reason: reason.to_owned(),
            },
            RuntimeEffect::CancelWorkerRun {
                run_id: run_id.clone(),
                worker_epoch: self.worker_epoch,
                owner_fence: self.owner_fence,
                admission_epoch: self.admission_epoch,
                reason: reason.to_owned(),
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autarqia_primitives::{
        address_from_signing_key, sign_eip712_digest, BasisPoints, Milliamps,
        MillidegreesCelsius, Millivolts, Milliwatts, RationalRate, UsageAck,
        ARC_TESTNET_ANDO_MPPI_PROXY, ARC_TESTNET_CHAIN_ID, ARC_TESTNET_GATEWAY_WALLET_PROXY,
        ARC_TESTNET_USDC_ADDRESS,
    };
    use k256::ecdsa::SigningKey;

    fn sample_runtime() -> (RuntimeState, SigningKey, SigningKey) {
        let provider_sk = SigningKey::from_bytes((&[0x22u8; 32]).into()).unwrap();
        let buyer_sk = SigningKey::from_bytes((&[0x33u8; 32]).into()).unwrap();
        let provider_addr = address_from_signing_key(&provider_sk);
        let config = RuntimeConfig {
            chain_id: ARC_TESTNET_CHAIN_ID,
            ando_mppi_proxy: EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap(),
            gateway_wallet_proxy: EvmAddress::from_hex(ARC_TESTNET_GATEWAY_WALLET_PROXY).unwrap(),
            usdc_token: EvmAddress::from_hex(ARC_TESTNET_USDC_ADDRESS).unwrap(),
            provider_payee: provider_addr,
            provider_operator: provider_addr,
            provider_signer: provider_addr,
            tariff: UsageTariff {
                prefill_rate: RationalRate::new(10, 1).unwrap(),
                output_rate: RationalRate::new(50, 1).unwrap(),
                min_run_fee: TokenAtomicAmount::new(100),
                uncovered_exposure_cap: TokenAtomicAmount::new(300),
                ack_stride_tokens: 2,
            },
            energy_policy: EnergyPolicyConfig::default(),
            model_manifest: ModelIntegrityManifest {
                engine_name: "airllm".to_owned(),
                airllm_commit: "c4c9de3d41e73770b95716bd447e53f049a2bc85".to_owned(),
                patch_sha256: Bytes32::ZERO,
                model_id: "Qwen/Qwen2.5-0.5B-Instruct".to_owned(),
                model_revision: "7ae557604adf67be50417f59c2c2f167def9a775".to_owned(),
                tokenizer_sha256: Bytes32::ZERO,
                chat_template_sha256: Bytes32::ZERO,
                original_manifest_sha256: Bytes32::ZERO,
                split_manifest_sha256: Bytes32::ZERO,
                runtime_device: "mps".to_owned(),
                runtime_dtype: "bfloat16".to_owned(),
                tested_context_limit: 2048,
                max_prefill_tokens: 1536,
                uninterruptible_step_tokens: 1,
                trust_remote_code: false,
            },
            allowance_ttl_ms: 30_000,
        };
        let mut state = RuntimeState::new(config, WorkerEpoch::new(1), OwnerFence::new(1));
        state.latest_energy = Some(EnergyTelemetrySnapshot {
            solar_power_mw: Milliwatts(45_000),
            battery_power_mw: Milliwatts(10_000),
            load_power_mw: Milliwatts(12_000),
            grid_power_mw: Milliwatts(0),
            battery_soc_bps: BasisPoints(8_000),
            panel_voltage_mv: Millivolts(18_500),
            panel_current_ma: Milliamps(2_430),
            temp_mc: MillidegreesCelsius(28_000),
            measured_at_monotonic_ms: MonotonicTimestampMs::new(1_000),
            source: "esp32_serial".to_owned(),
        });
        (state, provider_sk, buyer_sk)
    }

    #[test]
    fn deterministic_admission_streaming_ack_and_voucher_extension() {
        let (mut rt, _provider_sk, buyer_sk) = sample_runtime();
        let buyer_addr = address_from_signing_key(&buyer_sk);
        let ch_id = Bytes32::from_hex(
            "0x2b6548a57025a137badb75792fa955609e8b2ccbf70e384d5d9ae40ffc5cc2cc",
        )
        .unwrap();
        let clock = ClockContext {
            monotonic_ms: MonotonicTimestampMs::new(1_500),
            unix_ms: WallClockUnixMs::new(1_700_000_001_500),
        };

        rt.register_session(
            SessionId::new("sess_01"),
            SettlementRail::AndoMppiArcV1,
            ch_id,
            buyer_addr,
            buyer_addr,
            TokenAtomicAmount::new(10_000),
            Bytes32::ZERO,
            None,
            None,
            clock,
        )
        .unwrap();

        let run_id = RunId::new("run_01");
        let effects = rt
            .admit_run(
                run_id.clone(),
                SessionId::new("sess_01"),
                OrderId::new("ord_01"),
                PreparedInputId::new("prep_01"),
                Bytes32::ZERO,
                10,
                10,
                None,
                None,
                clock,
            )
            .unwrap();
        assert_eq!(effects.len(), 2);
        assert_eq!(
            rt.runs[&run_id].view.active_allowance.max_output_seq_exclusive,
            3
        );

        rt.on_worker_prefill_completed(&run_id, 10).unwrap();
        let p0 = compute_initial_prefix_state("run_01");
        let p1 = advance_prefix_state(p0, 1, 101, "Solar");
        rt.on_worker_token_delta(&run_id, 1, 101, "Solar", p1, clock)
            .unwrap();
        let p2 = advance_prefix_state(p1, 2, 102, " panels");
        let (due2, effs2) = rt
            .on_worker_token_delta(&run_id, 2, 102, " panels", p2, clock)
            .unwrap();
        assert_eq!(due2.0, 300);
        assert_eq!(effs2.len(), 1);

        let ando_domain =
            ando_mppi_domain_separator(ARC_TESTNET_CHAIN_ID, rt.config.ando_mppi_proxy);
        let voucher_amt = TokenAtomicAmount::new(1_000);
        let v_digest = ando_mppi_voucher_digest(ando_domain, ch_id, voucher_amt);
        let v_sig = sign_eip712_digest(&buyer_sk, v_digest).unwrap();

        let ack_struct = UsageAck {
            session_id: SessionId::new("sess_01"),
            run_id: run_id.clone(),
            channel_id: ch_id,
            acknowledged_output_seq: 2,
            acknowledged_cumulative_due_atomic: due2,
            voucher_cumulative_amount_atomic: voucher_amt,
            prefix_commitment: p2,
            issued_at_unix_ms: clock.unix_ms,
        };
        let run_domain =
            autarqia_mppi_run_domain_separator(ARC_TESTNET_CHAIN_ID, rt.config.ando_mppi_proxy);
        let ack_sig =
            sign_eip712_digest(&buyer_sk, usage_ack_digest(run_domain, &ack_struct)).unwrap();
        let signed_ack = SignedUsageAck {
            ack: ack_struct,
            buyer_signer: buyer_addr,
            signature: ack_sig,
            voucher: Some(SignedVoucher {
                channel_id: ch_id,
                cumulative_amount: voucher_amt,
                signature: v_sig,
            }),
        };

        let ack_effects = rt
            .on_buyer_usage_ack(&run_id, &signed_ack, 10, clock)
            .unwrap();
        assert!(ack_effects
            .iter()
            .any(|e| matches!(e, RuntimeEffect::UpdateWorkerAllowance { .. })));
        assert!(rt.runs[&run_id].view.active_allowance.max_output_seq_exclusive > 3);
    }
}
