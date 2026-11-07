use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use autarqia_bufi::BufiSink;
use autarqia_energy::EnergyTelemetryHub;
use autarqia_inference::{pb as worker_pb, AirLlmUdsClient};
use autarqia_payments::{partition_valid_settlement_batch, AndoMppiArcClient};
use autarqia_primitives::{
    autarqia_mppi_run_domain_separator, keccak256, sign_eip712_digest, usage_attestation_digest,
    BatchId, Bytes32, ChannelId, EnergyTelemetrySnapshot, MonotonicTimestampMs, QuoteId, RunId,
    RunReceipt, RunTerminalStatus, SessionId, SettlementRail, SignedUsageAttestation,
    TokenAtomicAmount, WallClockUnixMs, AUTARQIA_MPPI_PROFILE_ID, AUTARQIA_MPPI_PROFILE_VERSION,
};
use autarqia_runtime::{ClockContext, RuntimeConfig, RuntimeEffect, RuntimeState};
use autarqia_store::{
    AtomicCommitBatch, DurableLedgerStore, RocksDbLedgerStore, SingleWriterStoreHandle,
};
use autarqia_subsystem_api::{
    ChannelLedgerView, CreateQuoteRequest, CreateQuoteResponse, CreateSessionRequest,
    CreateSessionResponse, NodeHealthSnapshot, ProviderDiscoveryDocument, RunControlCommand,
    RunControlResponse, RunLedgerView, RunStreamFrame, SettlementBatchItem,
    SettlementSubmissionReceipt, StartRunRequest, StartRunResponse, SubsystemError,
};
use k256::ecdsa::SigningKey;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

pub struct CriticalTaskSupervisor {
    spawned_count: AtomicU64,
}

impl Default for CriticalTaskSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl CriticalTaskSupervisor {
    pub fn new() -> Self {
        Self {
            spawned_count: AtomicU64::new(0),
        }
    }

    pub fn spawn_critical<F>(&self, _name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.spawned_count.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(fut)
    }

    pub fn spawned_count(&self) -> u64 {
        self.spawned_count.load(Ordering::SeqCst)
    }
}

enum ControlPlaneCommand {
    RunControl {
        run_id: RunId,
        cmd: RunControlCommand,
        reply: oneshot::Sender<Result<RunControlResponse, SubsystemError>>,
    },
    InjectEnergy {
        snapshot: EnergyTelemetrySnapshot,
        reply: oneshot::Sender<Result<NodeHealthSnapshot, SubsystemError>>,
    },
}

struct RunStreamState {
    history: Vec<RunStreamFrame>,
    tx: broadcast::Sender<RunStreamFrame>,
    requested_max_output_tokens: u32,
}

struct NucleusInner {
    boot_instant: Instant,
    provider_sk: SigningKey,
    runtime: Mutex<RuntimeState>,
    store: SingleWriterStoreHandle,
    energy_hub: EnergyTelemetryHub,
    payments: AndoMppiArcClient,
    worker: AirLlmUdsClient,
    bufi_sink: Arc<dyn BufiSink>,
    supervisor: CriticalTaskSupervisor,
    run_streams: Mutex<BTreeMap<RunId, RunStreamState>>,
    pending_settlements: Mutex<Vec<SettlementBatchItem>>,
    control_tx: mpsc::Sender<ControlPlaneCommand>,
}

#[derive(Clone)]
pub struct NucleusHandle {
    inner: Arc<NucleusInner>,
}

impl NucleusHandle {
    pub async fn boot(
        mut config: RuntimeConfig,
        provider_sk: SigningKey,
        rocksdb_store: Arc<RocksDbLedgerStore>,
        energy_hub: EnergyTelemetryHub,
        payments: AndoMppiArcClient,
        worker: AirLlmUdsClient,
        bufi_sink: Arc<dyn BufiSink>,
    ) -> Result<Self, SubsystemError> {
        let (owner_fence, persisted_epoch) = rocksdb_store
            .acquire_and_bump_fence()
            .map_err(|e| SubsystemError::Store(e.to_string()))?;
        let recovered = rocksdb_store
            .load_recovered_state()
            .map_err(|e| SubsystemError::Store(e.to_string()))?;

        let (worker_desc, live_manifest) = worker
            .describe_worker()
            .await
            .map_err(|e| SubsystemError::Inference(e.to_string()))?;
        config.model_manifest = live_manifest;
        let active_worker_epoch = autarqia_primitives::WorkerEpoch::new(worker_desc.worker_epoch);

        let mut rt_state = if let Some(cp_json) = &recovered.runtime_checkpoint_json {
            let mut loaded: RuntimeState = serde_json::from_str(cp_json)
                .map_err(|e| SubsystemError::Store(format!("corrupt runtime checkpoint: {e}")))?;
            loaded.config = config.clone();
            loaded.owner_fence = owner_fence;
            loaded.worker_epoch = active_worker_epoch;
            loaded.active_run_id = None;
            for (_rid, run_rec) in loaded.runs.iter_mut() {
                if run_rec.view.terminal_status.is_none() {
                    run_rec.view.terminal_status = Some(RunTerminalStatus::WorkerError);
                }
            }
            loaded
        } else {
            let _ = persisted_epoch;
            RuntimeState::new(config, active_worker_epoch, owner_fence)
        };

        let init_energy = energy_hub.latest_snapshot().await;
        rt_state.latest_energy = Some(init_energy);

        let single_writer = SingleWriterStoreHandle::spawn(rocksdb_store);
        let (control_tx, mut control_rx) = mpsc::channel::<ControlPlaneCommand>(128);

        let inner = Arc::new(NucleusInner {
            boot_instant: Instant::now(),
            provider_sk,
            runtime: Mutex::new(rt_state),
            store: single_writer,
            energy_hub,
            payments,
            worker,
            bufi_sink,
            supervisor: CriticalTaskSupervisor::new(),
            run_streams: Mutex::new(BTreeMap::new()),
            pending_settlements: Mutex::new(Vec::new()),
            control_tx,
        });

        let handle = Self {
            inner: Arc::clone(&inner),
        };

        let loop_handle = handle.clone();
        inner
            .supervisor
            .spawn_critical("control-plane-overseer", async move {
                while let Some(cmd) = control_rx.recv().await {
                    match cmd {
                        ControlPlaneCommand::RunControl { run_id, cmd, reply } => {
                            let res = loop_handle.execute_run_control(&run_id, cmd).await;
                            let _ = reply.send(res);
                        }
                        ControlPlaneCommand::InjectEnergy { snapshot, reply } => {
                            let res = loop_handle.execute_inject_energy(snapshot).await;
                            let _ = reply.send(res);
                        }
                    }
                }
            });

        let _ = handle.flush_bufi_outbox().await;
        Ok(handle)
    }

    fn now_clock(&self) -> ClockContext {
        let mono_ms = self.inner.boot_instant.elapsed().as_millis() as u64;
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        ClockContext {
            monotonic_ms: MonotonicTimestampMs::new(mono_ms.max(1)),
            unix_ms: WallClockUnixMs::new(unix_ms),
        }
    }

    async fn emit_frame(&self, run_id: &RunId, frame: RunStreamFrame) {
        let mut streams = self.inner.run_streams.lock().await;
        if let Some(st) = streams.get_mut(run_id) {
            st.history.push(frame.clone());
            let _ = st.tx.send(frame);
        }
    }

    async fn commit_runtime_snapshot(&self, rt: &RuntimeState, note: &str) -> Result<(), SubsystemError> {
        let cp_json =
            serde_json::to_string(rt).map_err(|e| SubsystemError::Store(e.to_string()))?;
        let batch = AtomicCommitBatch {
            ledger_version: rt.ledger_version,
            runtime_checkpoint_json: Some(cp_json),
            channels: rt.channels.values().cloned().collect(),
            runs: rt.runs.values().map(|r| r.view.clone()).collect(),
            receipts: rt
                .runs
                .values()
                .filter_map(|r| r.view.receipt.clone())
                .collect(),
            orders: rt
                .orders
                .values()
                .map(|o| (o.order_id.clone(), o.run_id.clone()))
                .collect(),
            outbox_entries: rt
                .outbox
                .values()
                .filter(|e| !e.delivered)
                .cloned()
                .collect(),
            wal_note: note.to_owned(),
        };
        self.inner
            .store
            .commit_batch(batch)
            .await
            .map_err(|e| SubsystemError::Store(e.to_string()))?;
        Ok(())
    }

    pub async fn discovery_document(&self) -> ProviderDiscoveryDocument {
        let rt = self.inner.runtime.lock().await;
        ProviderDiscoveryDocument {
            profile_id: AUTARQIA_MPPI_PROFILE_ID.to_owned(),
            profile_version: AUTARQIA_MPPI_PROFILE_VERSION.to_owned(),
            chain_id: rt.config.chain_id,
            ando_mppi_proxy: rt.config.ando_mppi_proxy,
            ando_mppi_implementation: autarqia_primitives::EvmAddress::from_hex(
                autarqia_primitives::ARC_TESTNET_ANDO_MPPI_IMPL,
            )
            .unwrap(),
            gateway_wallet_proxy: rt.config.gateway_wallet_proxy,
            usdc_token: rt.config.usdc_token,
            provider_payee: rt.config.provider_payee,
            provider_operator: rt.config.provider_operator,
            provider_signer: rt.config.provider_signer,
            supported_rails: vec![
                SettlementRail::AndoMppiArcV1,
                SettlementRail::CircleGatewayX402,
            ],
            model_manifest: rt.config.model_manifest.clone(),
            tariff: rt.config.tariff.clone(),
            energy_policy: rt.config.energy_policy.clone(),
            latest_energy: rt.latest_energy.clone(),
        }
    }

    pub async fn create_quote(
        &self,
        req: CreateQuoteRequest,
    ) -> Result<CreateQuoteResponse, SubsystemError> {
        let rt = self.inner.runtime.lock().await;
        if req.model_id != rt.config.model_manifest.model_id {
            return Err(SubsystemError::NotFound(format!(
                "unknown model_id {}",
                req.model_id
            )));
        }
        let est = rt
            .config
            .tariff
            .compute_cumulative_due(req.estimated_input_tokens.max(1), req.max_output_tokens)
            .map_err(|e| SubsystemError::RuntimeRejected(e.to_string()))?;
        let clock = self.now_clock();
        Ok(CreateQuoteResponse {
            quote_id: QuoteId::new(format!("qt_{}", uuid::Uuid::new_v4().simple())),
            profile_id: AUTARQIA_MPPI_PROFILE_ID.to_owned(),
            rail: req.rail,
            model_id: req.model_id,
            model_manifest_hash: rt.config.model_manifest.manifest_commitment(),
            tariff: rt.config.tariff.clone(),
            estimated_total_atomic: est,
            required_initial_deposit_atomic: est,
            energy_mode: rt.config.energy_policy.mode,
            expires_at_unix_ms: WallClockUnixMs::new(clock.unix_ms.0.saturating_add(300_000)),
        })
    }

    pub async fn create_session(
        &self,
        req: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, SubsystemError> {
        let session_id = SessionId::new(format!("sess_{}", uuid::Uuid::new_v4().simple()));
        let bearer_token = format!("autarqia_bt_{}", uuid::Uuid::new_v4().simple());
        let mut h = Sha256::new();
        h.update(bearer_token.as_bytes());
        let mut token_hash = [0u8; 32];
        token_hash.copy_from_slice(&h.finalize());

        let clock = self.now_clock();
        let covered = {
            let mut rt = self.inner.runtime.lock().await;
            rt.register_session(
                session_id.clone(),
                req.rail,
                req.channel_id,
                req.payer,
                req.buyer_signer,
                req.onchain_deposit_atomic,
                Bytes32(token_hash),
                req.initial_voucher.as_ref(),
                req.x402_authorization.as_ref(),
                clock,
            )
            .map_err(|e| SubsystemError::RuntimeRejected(e.to_string()))?;
            self.commit_runtime_snapshot(&rt, "register_session").await?;
            rt.channels[&req.channel_id].highest_verified_voucher_atomic
        };

        Ok(CreateSessionResponse {
            session_id,
            rail: req.rail,
            channel_id: req.channel_id,
            covered_voucher_cumulative_atomic: covered,
            onchain_deposit_atomic: req.onchain_deposit_atomic,
            bearer_token,
        })
    }

    pub async fn start_run(
        &self,
        bearer_token: Option<&str>,
        req: StartRunRequest,
    ) -> Result<(StartRunResponse, broadcast::Receiver<RunStreamFrame>), SubsystemError> {
        let run_id = RunId::new(format!("run_{}", uuid::Uuid::new_v4().simple()));

        let (worker_epoch, owner_fence, model_id, manifest_hash, admission_epoch, session_rail) = {
            let mut rt = self.inner.runtime.lock().await;
            if rt.latest_energy.is_none() {
                let mut snap = self.inner.energy_hub.latest_snapshot().await;
                snap.measured_at_monotonic_ms = self.now_clock().monotonic_ms;
                rt.latest_energy = Some(snap);
            }
            let sess = rt
                .sessions
                .get(&req.session_id)
                .ok_or_else(|| SubsystemError::NotFound(req.session_id.to_string()))?;
            if let Some(bt) = bearer_token {
                let mut h = Sha256::new();
                h.update(bt.as_bytes());
                let mut th = [0u8; 32];
                th.copy_from_slice(&h.finalize());
                if Bytes32(th) != sess.bearer_token_sha256 {
                    return Err(SubsystemError::RuntimeRejected(
                        "invalid session bearer token".to_owned(),
                    ));
                }
            }
            if req.model_id != rt.config.model_manifest.model_id {
                return Err(SubsystemError::NotFound(format!(
                    "model_id {} does not match active worker model {}",
                    req.model_id, rt.config.model_manifest.model_id
                )));
            }
            (
                rt.worker_epoch,
                rt.owner_fence,
                rt.config.model_manifest.model_id.clone(),
                rt.config.model_manifest.manifest_commitment(),
                rt.admission_epoch,
                sess.rail,
            )
        };

        let prepared = self
            .inner
            .worker
            .prepare(
                &run_id,
                worker_epoch,
                owner_fence,
                &req.messages,
                req.max_output_tokens,
                req.temperature_milli,
                req.top_p_milli,
                req.seed,
            )
            .await
            .map_err(|e| SubsystemError::Inference(e.to_string()))?;

        let clock = self.now_clock();
        let initial_allowance = {
            let mut rt = self.inner.runtime.lock().await;
            let effects = rt
                .admit_run(
                    run_id.clone(),
                    req.session_id.clone(),
                    req.order_id.clone(),
                    prepared.prepared_input_id.clone(),
                    prepared.prompt_sha256,
                    prepared.input_token_count,
                    req.max_output_tokens,
                    req.initial_voucher.as_ref(),
                    req.x402_authorization.as_ref(),
                    clock,
                )
                .map_err(|e| match e {
                    autarqia_runtime::RuntimeError::EnergyAdmissionDenied(msg) => {
                        SubsystemError::EnergyRejected(msg)
                    }
                    autarqia_runtime::RuntimeError::DuplicateOrderConflict(msg) => {
                        SubsystemError::Conflict(msg)
                    }
                    other => SubsystemError::RuntimeRejected(other.to_string()),
                })?;
            self.commit_runtime_snapshot(&rt, "admit_run").await?;
            effects
                .into_iter()
                .find_map(|eff| match eff {
                    RuntimeEffect::StartWorkerGeneration {
                        initial_allowance, ..
                    } => Some(initial_allowance),
                    _ => None,
                })
                .expect("StartWorkerGeneration effect guaranteed by admit_run")
        };

        let (tx, rx) = broadcast::channel(512);
        let started_frame = RunStreamFrame::RunStarted {
            run_id: run_id.clone(),
            session_id: req.session_id.clone(),
            rail: session_rail,
            model_id,
            model_manifest_hash: manifest_hash,
            admission_epoch,
        };
        {
            let mut streams = self.inner.run_streams.lock().await;
            let _ = tx.send(started_frame.clone());
            streams.insert(
                run_id.clone(),
                RunStreamState {
                    history: vec![started_frame],
                    tx,
                    requested_max_output_tokens: req.max_output_tokens.max(1),
                },
            );
        }

        let task_handle = self.clone();
        let task_run_id = run_id.clone();
        let task_prep_id = prepared.prepared_input_id.clone();
        let task_init_allow = initial_allowance.clone();
        self.inner
            .supervisor
            .spawn_critical("airllm-generation-driver", async move {
                task_handle
                    .drive_worker_generation(task_run_id, task_prep_id, task_init_allow)
                    .await;
            });

        Ok((
            StartRunResponse {
                run_id,
                session_id: req.session_id,
                order_id: req.order_id,
                rail: session_rail,
                input_token_count: prepared.input_token_count,
                initial_allowance,
            },
            rx,
        ))
    }

    async fn drive_worker_generation(
        &self,
        run_id: RunId,
        prepared_input_id: autarqia_primitives::PreparedInputId,
        initial_allowance: autarqia_primitives::InternalWorkAllowance,
    ) {
        let mut stream = match self
            .inner
            .worker
            .generate(&run_id, &prepared_input_id, &initial_allowance)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("worker generate failed for {}: {}", run_id, e);
                let _ = self
                    .finalize_run_internal(&run_id, "worker_error", 0, Bytes32::ZERO)
                    .await;
                return;
            }
        };

        while let Ok(Some(ev)) = stream.message().await {
            let Some(inner_ev) = ev.event else {
                continue;
            };
            match inner_ev {
                worker_pb::generate_event::Event::PrefillCompleted(pf) => {
                    let due = {
                        let mut rt = self.inner.runtime.lock().await;
                        rt.on_worker_prefill_completed(&run_id, pf.input_tokens_processed)
                            .unwrap_or(TokenAtomicAmount::ZERO)
                    };
                    self.emit_frame(
                        &run_id,
                        RunStreamFrame::PrefillCompleted {
                            run_id: run_id.clone(),
                            input_tokens_processed: pf.input_tokens_processed,
                            cumulative_due_atomic: due,
                            elapsed_monotonic_ms: pf.elapsed_monotonic_ms,
                        },
                    )
                    .await;
                }
                worker_pb::generate_event::Event::TokenDelta(td) => {
                    let Ok(prefix) = Bytes32::from_hex(&td.prefix_commitment) else {
                        continue;
                    };
                    let clock = self.now_clock();
                    let (due, effects, chain_id, proxy, signer_addr) = {
                        let mut rt = self.inner.runtime.lock().await;
                        match rt.on_worker_token_delta(
                            &run_id,
                            td.output_seq,
                            td.token_id,
                            &td.text_delta,
                            prefix,
                            clock,
                        ) {
                            Ok((due, effs)) => (
                                due,
                                effs,
                                rt.config.chain_id,
                                rt.config.ando_mppi_proxy,
                                rt.config.provider_signer,
                            ),
                            Err(e) => {
                                tracing::error!("token delta runtime error: {}", e);
                                break;
                            }
                        }
                    };

                    self.emit_frame(
                        &run_id,
                        RunStreamFrame::TokenDelta {
                            run_id: run_id.clone(),
                            output_seq: td.output_seq,
                            token_id: td.token_id,
                            text_delta: td.text_delta,
                            prefix_commitment: prefix,
                            cumulative_due_atomic: due,
                            allowance_seq_used: autarqia_primitives::AllowanceSeq::new(
                                td.allowance_seq_used,
                            ),
                        },
                    )
                    .await;

                    for eff in effects {
                        if let RuntimeEffect::EmitUsageAttestation {
                            attestation,
                            require_ack_by_output_seq,
                            ..
                        } = eff
                        {
                            let domain = autarqia_mppi_run_domain_separator(chain_id, proxy);
                            let digest = usage_attestation_digest(domain, &attestation);
                            if let Ok(sig) = sign_eip712_digest(&self.inner.provider_sk, digest) {
                                self.emit_frame(
                                    &run_id,
                                    RunStreamFrame::UsageAttestation {
                                        signed_attestation: SignedUsageAttestation {
                                            attestation,
                                            provider_signer: signer_addr,
                                            signature: sig,
                                        },
                                        require_ack_by_output_seq,
                                    },
                                )
                                .await;
                            }
                        }
                    }
                }
                worker_pb::generate_event::Event::LowHeadroom(_lh) => {}
                worker_pb::generate_event::Event::Terminal(term) => {
                    let final_prefix =
                        Bytes32::from_hex(&term.final_prefix_commitment).unwrap_or(Bytes32::ZERO);
                    let _ = self
                        .finalize_run_internal(
                            &run_id,
                            &term.status,
                            term.output_tokens_generated,
                            final_prefix,
                        )
                        .await;
                    break;
                }
            }
        }
    }

    async fn finalize_run_internal(
        &self,
        run_id: &RunId,
        worker_status: &str,
        output_tokens_generated: u32,
        final_prefix_commitment: Bytes32,
    ) -> Result<(), SubsystemError> {
        let clock = self.now_clock();
        let (effects, signed_receipt) = {
            let mut rt = self.inner.runtime.lock().await;
            let effs = rt
                .finalize_run_from_worker(
                    run_id,
                    worker_status,
                    output_tokens_generated,
                    final_prefix_commitment,
                    clock,
                )
                .map_err(|e| SubsystemError::RuntimeRejected(e.to_string()))?;

            let mut final_rcpt = None;
            for eff in &effs {
                if let RuntimeEffect::SignAndFinalizeReceipt {
                    unsigned_receipt, ..
                } = eff
                {
                    let mut rcpt = unsigned_receipt.clone();
                    let payload = serde_json::to_vec(&rcpt).unwrap_or_default();
                    let digest = keccak256(&payload);
                    if let Ok(sig) = sign_eip712_digest(&self.inner.provider_sk, digest) {
                        rcpt.provider_signature = sig;
                    }
                    if let Some(rec) = rt.runs.get_mut(run_id) {
                        rec.view.receipt = Some(rcpt.clone());
                    }
                    final_rcpt = Some(rcpt);
                }
            }
            self.commit_runtime_snapshot(&rt, "finalize_run").await?;
            (effs, final_rcpt)
        };

        for eff in effects {
            if let RuntimeEffect::EmitPaymentRequired {
                run_id,
                current_output_seq,
                cumulative_due_atomic,
                covered_voucher_cumulative_atomic,
                uncovered_exposure_cap_atomic,
                reason,
            } = eff
            {
                self.emit_frame(
                    &run_id,
                    RunStreamFrame::PaymentRequired {
                        run_id: run_id.clone(),
                        current_output_seq,
                        cumulative_due_atomic,
                        covered_voucher_cumulative_atomic,
                        uncovered_exposure_cap_atomic,
                        reason,
                    },
                )
                .await;
            }
        }

        if let Some(receipt) = signed_receipt {
            self.emit_frame(&run_id, RunStreamFrame::ReceiptReady { receipt })
                .await;
        }
        let _ = self.flush_bufi_outbox().await;
        Ok(())
    }

    pub async fn control_run(
        &self,
        run_id: &RunId,
        cmd: RunControlCommand,
    ) -> Result<RunControlResponse, SubsystemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .control_tx
            .send(ControlPlaneCommand::RunControl {
                run_id: run_id.clone(),
                cmd,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SubsystemError::RuntimeRejected("control plane closed".to_owned()))?;
        reply_rx
            .await
            .map_err(|_| SubsystemError::RuntimeRejected("control plane dropped".to_owned()))?
    }

    async fn execute_run_control(
        &self,
        run_id: &RunId,
        cmd: RunControlCommand,
    ) -> Result<RunControlResponse, SubsystemError> {
        let clock = self.now_clock();
        let req_max_tokens = {
            let streams = self.inner.run_streams.lock().await;
            streams
                .get(run_id)
                .map(|s| s.requested_max_output_tokens)
                .unwrap_or(64)
        };

        match cmd {
            RunControlCommand::AckUsage { signed_ack } => {
                let (effects, covered) = {
                    let mut rt = self.inner.runtime.lock().await;
                    let effs = rt
                        .on_buyer_usage_ack(run_id, &signed_ack, req_max_tokens, clock)
                        .map_err(|e| SubsystemError::RuntimeRejected(e.to_string()))?;
                    self.commit_runtime_snapshot(&rt, "buyer_usage_ack").await?;
                    let ch_id = rt.runs[run_id].view.channel_id;
                    let cov = rt.channels[&ch_id].highest_verified_voucher_atomic;
                    (effs, cov)
                };

                let mut updated_allowance = None;
                for eff in effects {
                    match eff {
                        RuntimeEffect::UpdateWorkerAllowance { allowance } => {
                            let _ = self.inner.worker.update_allowance(&allowance).await;
                            self.emit_frame(
                                run_id,
                                RunStreamFrame::AllowanceExtended {
                                    run_id: run_id.clone(),
                                    allowance: allowance.clone(),
                                },
                            )
                            .await;
                            updated_allowance = Some(allowance);
                        }
                        RuntimeEffect::QueueSettlementItem { item, .. } => {
                            self.inner.pending_settlements.lock().await.push(item);
                        }
                        _ => {}
                    }
                }

                Ok(RunControlResponse {
                    run_id: run_id.clone(),
                    accepted: true,
                    detail: "ack_verified".to_owned(),
                    updated_allowance,
                    covered_voucher_cumulative_atomic: covered,
                })
            }
            RunControlCommand::TopUpChannel {
                new_onchain_deposit_atomic,
                voucher,
            } => {
                let covered = {
                    let mut rt = self.inner.runtime.lock().await;
                    let ch_id = rt
                        .runs
                        .get(run_id)
                        .ok_or_else(|| SubsystemError::NotFound(run_id.to_string()))?
                        .view
                        .channel_id;
                    if let Some(ch) = rt.channels.get_mut(&ch_id) {
                        if new_onchain_deposit_atomic > ch.deposit_atomic {
                            ch.deposit_atomic = new_onchain_deposit_atomic;
                        }
                    }
                    if let Some(v) = &voucher {
                        rt.verify_and_apply_voucher(ch_id, v)
                            .map_err(|e| SubsystemError::RuntimeRejected(e.to_string()))?;
                    }
                    self.commit_runtime_snapshot(&rt, "top_up_channel").await?;
                    rt.channels[&ch_id].highest_verified_voucher_atomic
                };
                Ok(RunControlResponse {
                    run_id: run_id.clone(),
                    accepted: true,
                    detail: "channel_topped_up".to_owned(),
                    updated_allowance: None,
                    covered_voucher_cumulative_atomic: covered,
                })
            }
            RunControlCommand::Cancel { reason } => {
                let (worker_epoch, owner_fence, admission_epoch, covered) = {
                    let rt = self.inner.runtime.lock().await;
                    let rec = rt
                        .runs
                        .get(run_id)
                        .ok_or_else(|| SubsystemError::NotFound(run_id.to_string()))?;
                    let cov = rt
                        .channels
                        .get(&rec.view.channel_id)
                        .map(|c| c.highest_verified_voucher_atomic)
                        .unwrap_or(TokenAtomicAmount::ZERO);
                    (rt.worker_epoch, rt.owner_fence, rt.admission_epoch, cov)
                };
                let _ = self
                    .inner
                    .worker
                    .cancel(run_id, worker_epoch, owner_fence, admission_epoch, &reason)
                    .await;
                Ok(RunControlResponse {
                    run_id: run_id.clone(),
                    accepted: true,
                    detail: "cancel_dispatched".to_owned(),
                    updated_allowance: None,
                    covered_voucher_cumulative_atomic: covered,
                })
            }
        }
    }

    pub async fn inject_energy_telemetry(
        &self,
        mut snapshot: EnergyTelemetrySnapshot,
    ) -> Result<NodeHealthSnapshot, SubsystemError> {
        snapshot.measured_at_monotonic_ms = self.now_clock().monotonic_ms;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .control_tx
            .send(ControlPlaneCommand::InjectEnergy {
                snapshot,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SubsystemError::RuntimeRejected("control plane closed".to_owned()))?;
        reply_rx
            .await
            .map_err(|_| SubsystemError::RuntimeRejected("control plane dropped".to_owned()))?
    }

    async fn execute_inject_energy(
        &self,
        snapshot: EnergyTelemetrySnapshot,
    ) -> Result<NodeHealthSnapshot, SubsystemError> {
        self.inner
            .energy_hub
            .update_snapshot(snapshot.clone())
            .await;
        let clock = self.now_clock();
        let effects = {
            let mut rt = self.inner.runtime.lock().await;
            let effs = rt.ingest_energy_telemetry(snapshot, clock);
            if !effs.is_empty() {
                self.commit_runtime_snapshot(&rt, "energy_policy_abort")
                    .await?;
            }
            effs
        };

        for eff in effects {
            if let RuntimeEffect::CancelWorkerRun {
                run_id,
                worker_epoch,
                owner_fence,
                admission_epoch,
                reason,
            } = eff
            {
                let _ = self
                    .inner
                    .worker
                    .cancel(&run_id, worker_epoch, owner_fence, admission_epoch, &reason)
                    .await;
            }
        }
        Ok(self.health_snapshot().await)
    }

    pub async fn subscribe_run_events(
        &self,
        run_id: &RunId,
    ) -> Result<(Vec<RunStreamFrame>, broadcast::Receiver<RunStreamFrame>), SubsystemError> {
        let streams = self.inner.run_streams.lock().await;
        let st = streams
            .get(run_id)
            .ok_or_else(|| SubsystemError::NotFound(run_id.to_string()))?;
        Ok((st.history.clone(), st.tx.subscribe()))
    }

    pub async fn get_receipt(&self, run_id: &RunId) -> Result<RunReceipt, SubsystemError> {
        {
            let rt = self.inner.runtime.lock().await;
            if let Some(rec) = rt.runs.get(run_id) {
                if let Some(rcpt) = &rec.view.receipt {
                    return Ok(rcpt.clone());
                }
            }
        }
        self.inner
            .store
            .get_receipt(run_id)
            .map_err(|e| SubsystemError::Store(e.to_string()))?
            .ok_or_else(|| SubsystemError::NotFound(format!("receipt not ready for {run_id}")))
    }

    pub async fn get_run_view(&self, run_id: &RunId) -> Result<RunLedgerView, SubsystemError> {
        let rt = self.inner.runtime.lock().await;
        rt.runs
            .get(run_id)
            .map(|r| r.view.clone())
            .ok_or_else(|| SubsystemError::NotFound(run_id.to_string()))
    }

    pub async fn get_channel_view(
        &self,
        channel_id: &ChannelId,
    ) -> Result<ChannelLedgerView, SubsystemError> {
        let rt = self.inner.runtime.lock().await;
        rt.channels
            .get(channel_id)
            .cloned()
            .ok_or_else(|| SubsystemError::NotFound(channel_id.to_hex()))
    }

    pub async fn reconcile_onchain_channel(
        &self,
        channel_id: ChannelId,
        onchain_deposit_atomic: TokenAtomicAmount,
        onchain_settled_atomic: TokenAtomicAmount,
        close_requested_at_unix_s: Option<u64>,
        closed: bool,
    ) -> Result<ChannelLedgerView, SubsystemError> {
        let mut rt = self.inner.runtime.lock().await;
        let effects = rt
            .reconcile_onchain_channel_state(
                channel_id,
                onchain_deposit_atomic,
                onchain_settled_atomic,
                close_requested_at_unix_s,
                closed,
            )
            .map_err(|e| SubsystemError::RuntimeRejected(e.to_string()))?;
        self.commit_runtime_snapshot(&rt, "reconcile_onchain_channel")
            .await?;
        for eff in effects {
            if let RuntimeEffect::QueueSettlementItem { item, .. } = eff {
                self.inner.pending_settlements.lock().await.push(item);
            }
        }
        Ok(rt.channels[&channel_id].clone())
    }

    pub async fn flush_settlement_batch(
        &self,
    ) -> Result<Option<SettlementSubmissionReceipt>, SubsystemError> {
        let items = {
            let mut pending = self.inner.pending_settlements.lock().await;
            if pending.is_empty() {
                return Ok(None);
            }
            std::mem::take(&mut *pending)
        };

        let (chain_id, proxy, channels) = {
            let rt = self.inner.runtime.lock().await;
            (
                rt.config.chain_id,
                rt.config.ando_mppi_proxy,
                rt.channels.values().cloned().collect::<Vec<_>>(),
            )
        };

        let (valid, _quarantined) =
            partition_valid_settlement_batch(chain_id, proxy, &items, &channels);
        if valid.is_empty() {
            return Ok(None);
        }

        let batch_id = BatchId::new(format!("batch_{}", uuid::Uuid::new_v4().simple()));
        let receipt = self.inner.payments.build_simulated_batch_receipt(
            batch_id,
            SettlementRail::AndoMppiArcV1,
            &valid,
            1,
        );

        let mut rt = self.inner.runtime.lock().await;
        for (ch_id, settled_amt) in &receipt.settled_channels {
            if let Some(ch) = rt.channels.get_mut(ch_id) {
                if *settled_amt > ch.settled_onchain_atomic {
                    ch.settled_onchain_atomic = *settled_amt;
                }
            }
        }
        self.commit_runtime_snapshot(&rt, "flush_settlement_batch")
            .await?;
        Ok(Some(receipt))
    }

    pub async fn flush_bufi_outbox(&self) -> Result<usize, SubsystemError> {
        let pending = {
            let rt = self.inner.runtime.lock().await;
            rt.outbox
                .values()
                .filter(|e| !e.delivered)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut count = 0usize;
        for entry in pending {
            if self.inner.bufi_sink.publish_entry(&entry).await.is_ok() {
                let _ = self
                    .inner
                    .store
                    .mark_outbox_delivered(entry.entry_id.clone())
                    .await;
                let mut rt = self.inner.runtime.lock().await;
                if let Some(e) = rt.outbox.get_mut(&entry.entry_id) {
                    e.delivered = true;
                    e.delivery_attempts = e.delivery_attempts.saturating_add(1);
                }
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn health_snapshot(&self) -> NodeHealthSnapshot {
        let rt = self.inner.runtime.lock().await;
        let pending_bufi = rt.outbox.values().filter(|e| !e.delivered).count();
        NodeHealthSnapshot {
            healthy: true,
            ready: rt.worker_ready,
            worker_ready: rt.worker_ready,
            worker_epoch: rt.worker_epoch,
            owner_fence: rt.owner_fence,
            admission_epoch: rt.admission_epoch,
            ledger_version: rt.ledger_version,
            active_runs: usize::from(rt.active_run_id.is_some()),
            energy_mode: rt.config.energy_policy.mode,
            latest_energy: rt.latest_energy.clone(),
            pending_bufi_outbox: pending_bufi,
        }
    }
}
