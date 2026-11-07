use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use autarqia_cli::{
    start_local_openai_mppi_proxy, verify_workspace_dag, AutonomousBuyerClient, BuyerRunOptions,
};
use autarqia_node::{default_runtime_config, start_node_server};
use autarqia_payments::{
    default_arc_usdc_address, filter_arc_usdc_transfer_logs, partition_valid_settlement_batch,
    AndoMppiArcClient, RawEvmLog,
};
use autarqia_primitives::{
    address_from_signing_key, ando_mppi_domain_separator, ando_mppi_voucher_digest,
    compute_channel_id, encode_u128_word, gateway_wallet_batched_domain_separator, keccak256,
    sign_eip712_digest, x402_transfer_with_authorization_digest, BasisPoints, Bytes32, ChatMessage,
    EnergyPolicyMode, EnergyTelemetrySnapshot, EvmAddress, Milliamps, MillidegreesCelsius,
    Millivolts, Milliwatts, MonotonicTimestampMs, OrderId, RunTerminalStatus, SettlementRail,
    SignedVoucher, TokenAtomicAmount, WallClockUnixMs, X402AuthorizationPayload,
    ARC_EIP7708_SYSTEM_SENDER, ARC_TESTNET_ANDO_MPPI_PROXY, ARC_TESTNET_CHAIN_ID,
};
use autarqia_runtime::ClockContext;
use autarqia_store::{DurableLedgerStore, RocksDbLedgerStore};
use autarqia_subsystem_api::SettlementBatchItem;
use k256::ecdsa::SigningKey;

struct RealPythonWorkerGuard {
    child: Child,
}

impl Drop for RealPythonWorkerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn spawn_real_airllm_worker(uds_path: &Path, worker_epoch: u64) -> RealPythonWorkerGuard {
    let root = repo_root();
    let python_bin = root.join("workers/airllm/.venv/bin/python");
    let server_py = root.join("workers/airllm/server.py");
    let model_dir = root.join("data/models/Qwen--Qwen2.5-0.5B-Instruct");

    let mut child = Command::new(&python_bin)
        .arg(&server_py)
        .arg("--uds-path")
        .arg(uds_path)
        .arg("--model-dir")
        .arg(&model_dir)
        .arg("--worker-epoch")
        .arg(worker_epoch.to_string())
        .arg("--grace-wait-ms")
        .arg("500")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn real Python AirLLM worker");

    let stdout = child.stdout.take().expect("missing worker stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .expect("failed reading worker stdout");
        assert!(n > 0, "AirLLM worker exited before printing startup JSON");
        if line.contains("airllm_worker_started") {
            break;
        }
    }

    std::thread::spawn(move || {
        let mut sink_line = String::new();
        while reader.read_line(&mut sink_line).unwrap_or(0) > 0 {
            sink_line.clear();
        }
    });

    RealPythonWorkerGuard { child }
}

#[tokio::test]
async fn acceptance_scenarios_01_02_11_14_17_end_to_end_with_real_airllm() {
    let tmp = tempfile::tempdir().unwrap();
    let uds_path = tmp.path().join("airllm.sock");
    let db_path = tmp.path().join("rocksdb");
    let bufi_path = tmp.path().join("bufi-outbox.jsonl");

    let _worker = spawn_real_airllm_worker(&uds_path, 1);

    let provider_sk = SigningKey::from_bytes((&[0x22u8; 32]).into()).unwrap();
    let buyer_sk = SigningKey::from_bytes((&[0x33u8; 32]).into()).unwrap();

    let (node_addr, nucleus, _node_join) = start_node_server(
        "127.0.0.1:0".parse().unwrap(),
        &db_path,
        &uds_path,
        &bufi_path,
        "https://rpc.testnet.arc.network",
        provider_sk.clone(),
        None,
    )
    .await
    .unwrap();
    let base_url = format!("http://{node_addr}");
    let buyer = AutonomousBuyerClient::new(base_url.clone(), buyer_sk.clone());

    // Scenario 1: Full real AirLLM layer-streamed run with mid-run ACKs, vouchers, signed receipt, and BUFI outbox
    let outcome1 = buyer
        .execute_run(BuyerRunOptions {
            rail: SettlementRail::AndoMppiArcV1,
            model_id: "Qwen/Qwen2.5-0.5B-Instruct".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: "Reply with the single word: Photovoltaic.".to_owned(),
            }],
            max_output_tokens: 6,
            channel_deposit_atomic: TokenAtomicAmount::new(50_000),
            initial_voucher_atomic: TokenAtomicAmount::new(400),
            stop_acks_after_tokens: None,
            order_id: Some(OrderId::new("ord_accept_01")),
        })
        .await
        .unwrap();

    assert!(outcome1.tokens_received >= 1);
    assert!(!outcome1.assembled_text.trim().is_empty());
    assert!(outcome1.attestations_verified >= 1);
    assert!(outcome1.acks_sent >= 1);
    assert!(
        matches!(
            outcome1.receipt.terminal_status,
            RunTerminalStatus::Completed | RunTerminalStatus::CompletedMaxTokens
        ),
        "unexpected terminal status: {:?}",
        outcome1.receipt.terminal_status
    );
    assert!(bufi_path.exists(), "BUFI outbox file must be written");

    // Scenario 2a: Underfunded prefill (0 initial voucher when prefill > E_max) rejected at admission with HTTP 402
    let underfunded = buyer
        .execute_run(BuyerRunOptions {
            rail: SettlementRail::AndoMppiArcV1,
            model_id: "Qwen/Qwen2.5-0.5B-Instruct".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: "Count from one to twenty slowly.".to_owned(),
            }],
            max_output_tokens: 12,
            channel_deposit_atomic: TokenAtomicAmount::new(50_000),
            initial_voucher_atomic: TokenAtomicAmount::ZERO,
            stop_acks_after_tokens: Some(0),
            order_id: Some(OrderId::new("ord_accept_02a")),
        })
        .await;
    assert!(underfunded.is_err());

    // Scenario 2b: Hostile buyer funds initial stride (250 atomic) then stops sending ACKs & vouchers -> provider halts at bounded exposure
    let outcome2 = buyer
        .execute_run(BuyerRunOptions {
            rail: SettlementRail::AndoMppiArcV1,
            model_id: "Qwen/Qwen2.5-0.5B-Instruct".to_owned(),
            messages: vec![ChatMessage {
                role: "user".to_owned(),
                content: "Count from one to twenty slowly.".to_owned(),
            }],
            max_output_tokens: 12,
            channel_deposit_atomic: TokenAtomicAmount::new(50_000),
            initial_voucher_atomic: TokenAtomicAmount::new(250),
            stop_acks_after_tokens: Some(0),
            order_id: Some(OrderId::new("ord_accept_02b")),
        })
        .await
        .unwrap();

    assert_eq!(
        outcome2.receipt.terminal_status,
        RunTerminalStatus::PausedPaymentRequired
    );
    assert!(outcome2.payment_required_emitted);
    assert!(outcome2.tokens_received <= 2);
    assert!(
        outcome2
            .receipt
            .cumulative_due_atomic
            .saturating_sub(outcome2.receipt.covered_voucher_cumulative_atomic)
            .0
            <= 250
    );

    // Scenario 17: Local OpenAI-compatible MPP-I proxy end-to-end
    let (proxy_addr, _proxy_join) = start_local_openai_mppi_proxy(
        "127.0.0.1:0".parse().unwrap(),
        base_url.clone(),
        buyer_sk.clone(),
    )
    .await
    .unwrap();
    let http = reqwest::Client::new();
    let proxy_resp: serde_json::Value = http
        .post(format!("http://{proxy_addr}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen2.5-0.5B-Instruct",
            "messages": [{"role": "user", "content": "Say hello from solar proxy."}],
            "max_tokens": 4
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let proxy_text = proxy_resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!proxy_text.is_empty());
    assert!(proxy_resp["autarqia_mppi"]["receipt"]["receipt_id"].is_string());

    // Scenario 11: Energy drop below hard floor bumps AdmissionEpoch
    let health_before = nucleus.health_snapshot().await;
    let health_after = nucleus
        .inject_energy_telemetry(EnergyTelemetrySnapshot {
            solar_power_mw: Milliwatts(0),
            battery_power_mw: Milliwatts(-5_000),
            load_power_mw: Milliwatts(18_000),
            grid_power_mw: Milliwatts(0),
            battery_soc_bps: BasisPoints(1_200),
            panel_voltage_mv: Millivolts(0),
            panel_current_ma: Milliamps(0),
            temp_mc: MillidegreesCelsius(25_000),
            measured_at_monotonic_ms: MonotonicTimestampMs::new(10_000),
            source: "test_override".to_owned(),
        })
        .await
        .unwrap();
    assert!(health_after.admission_epoch > health_before.admission_epoch);

    // Scenario 14: Verify persisted RocksDB state (high-water voucher, orders, receipts)
    let ch_view = nucleus
        .get_channel_view(&outcome1.receipt.channel_id)
        .await
        .unwrap();
    assert_eq!(
        ch_view.highest_verified_voucher_atomic,
        outcome1.highest_voucher_signed_atomic
    );
    let rcpt_persisted = nucleus
        .get_receipt(&autarqia_primitives::RunId::new(outcome1.run_id.clone()))
        .await
        .unwrap();
    assert_eq!(rcpt_persisted.receipt_id, outcome1.receipt.receipt_id);
}

#[tokio::test]
async fn acceptance_scenarios_03_to_10_12_13_15_16_18_deterministic_and_arc_rpc() {
    let provider_sk = SigningKey::from_bytes((&[0x22u8; 32]).into()).unwrap();
    let buyer_sk = SigningKey::from_bytes((&[0x33u8; 32]).into()).unwrap();
    let buyer_addr = address_from_signing_key(&buyer_sk);
    let mut cfg = default_runtime_config(&provider_sk);
    cfg.energy_policy.mode = EnergyPolicyMode::SolarSurplusOnly;

    let mut rt = autarqia_runtime::RuntimeState::new(
        cfg.clone(),
        autarqia_primitives::WorkerEpoch::new(1),
        autarqia_primitives::OwnerFence::new(1),
    );
    let clock = ClockContext {
        monotonic_ms: MonotonicTimestampMs::new(5_000),
        unix_ms: WallClockUnixMs::new(1_700_000_005_000),
    };

    // Scenario 10: SolarSurplusOnly rejects grid import
    rt.latest_energy = Some(EnergyTelemetrySnapshot {
        solar_power_mw: Milliwatts(50_000),
        battery_power_mw: Milliwatts(5_000),
        load_power_mw: Milliwatts(10_000),
        grid_power_mw: Milliwatts(2_000),
        battery_soc_bps: BasisPoints(8_000),
        panel_voltage_mv: Millivolts(18_000),
        panel_current_ma: Milliamps(2_500),
        temp_mc: MillidegreesCelsius(25_000),
        measured_at_monotonic_ms: MonotonicTimestampMs::new(4_900),
        source: "test".to_owned(),
    });
    assert!(rt.evaluate_energy_for_new_run(clock).is_err());

    // Scenario 12: Stale telemetry rejected
    rt.latest_energy = Some(EnergyTelemetrySnapshot {
        solar_power_mw: Milliwatts(60_000),
        battery_power_mw: Milliwatts(5_000),
        load_power_mw: Milliwatts(10_000),
        grid_power_mw: Milliwatts(0),
        battery_soc_bps: BasisPoints(8_500),
        panel_voltage_mv: Millivolts(18_000),
        panel_current_ma: Milliamps(2_500),
        temp_mc: MillidegreesCelsius(25_000),
        measured_at_monotonic_ms: MonotonicTimestampMs::new(1),
        source: "test".to_owned(),
    });
    let stale_clock = ClockContext {
        monotonic_ms: MonotonicTimestampMs::new(50_000),
        unix_ms: clock.unix_ms,
    };
    assert!(rt.evaluate_energy_for_new_run(stale_clock).is_err());

    // Restore healthy solar surplus
    rt.latest_energy.as_mut().unwrap().measured_at_monotonic_ms = MonotonicTimestampMs::new(4_900);
    assert!(rt.evaluate_energy_for_new_run(clock).is_ok());

    let ch_id = compute_channel_id(
        buyer_addr,
        cfg.provider_payee,
        cfg.usdc_token,
        Bytes32::ZERO,
        buyer_addr,
        cfg.ando_mppi_proxy,
        cfg.chain_id,
    );
    let ando_domain = ando_mppi_domain_separator(cfg.chain_id, cfg.ando_mppi_proxy);

    // Scenario 3: Voucher exceeding deposit rejected
    let over_voucher_amt = TokenAtomicAmount::new(99_999);
    let over_sig = sign_eip712_digest(
        &buyer_sk,
        ando_mppi_voucher_digest(ando_domain, ch_id, over_voucher_amt),
    )
    .unwrap();
    let over_res = rt.register_session(
        autarqia_primitives::SessionId::new("sess_over"),
        SettlementRail::AndoMppiArcV1,
        ch_id,
        buyer_addr,
        buyer_addr,
        TokenAtomicAmount::new(1_000),
        Bytes32::ZERO,
        Some(&SignedVoucher {
            channel_id: ch_id,
            cumulative_amount: over_voucher_amt,
            signature: over_sig,
        }),
        None,
        clock,
    );
    assert!(over_res.is_err());

    // Scenario 4: Valid session + TopUp increases deposit and allows higher voucher
    rt.register_session(
        autarqia_primitives::SessionId::new("sess_ando"),
        SettlementRail::AndoMppiArcV1,
        ch_id,
        buyer_addr,
        buyer_addr,
        TokenAtomicAmount::new(1_000),
        Bytes32::ZERO,
        None,
        None,
        clock,
    )
    .unwrap();
    rt.reconcile_onchain_channel_state(
        ch_id,
        TokenAtomicAmount::new(5_000),
        TokenAtomicAmount::ZERO,
        None,
        false,
    )
    .unwrap();
    let valid_v_amt = TokenAtomicAmount::new(2_500);
    let valid_v_sig = sign_eip712_digest(
        &buyer_sk,
        ando_mppi_voucher_digest(ando_domain, ch_id, valid_v_amt),
    )
    .unwrap();
    rt.verify_and_apply_voucher(
        ch_id,
        &SignedVoucher {
            channel_id: ch_id,
            cumulative_amount: valid_v_amt,
            signature: valid_v_sig.clone(),
        },
    )
    .unwrap();

    // Scenario 7: Single rail per run enforced (mixing x402 auth on AndoMppiArcV1 session rejected)
    let now_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut x402_payload = X402AuthorizationPayload {
        from: buyer_addr,
        to: cfg.provider_payee,
        value: TokenAtomicAmount::new(1_000),
        valid_after: 0,
        valid_before: now_s + 3600,
        nonce: Bytes32([0x77; 32]),
        signature: Default::default(),
    };
    let gw_domain =
        gateway_wallet_batched_domain_separator(cfg.chain_id, cfg.gateway_wallet_proxy);
    x402_payload.signature = sign_eip712_digest(
        &buyer_sk,
        x402_transfer_with_authorization_digest(gw_domain, &x402_payload),
    )
    .unwrap();

    let mix_res = rt.admit_run(
        autarqia_primitives::RunId::new("run_mix"),
        autarqia_primitives::SessionId::new("sess_ando"),
        OrderId::new("ord_mix"),
        autarqia_primitives::PreparedInputId::new("prep_mix"),
        Bytes32::ZERO,
        10,
        4,
        None,
        Some(&x402_payload),
        clock,
    );
    assert!(matches!(
        mix_res,
        Err(autarqia_runtime::RuntimeError::RailMismatch { .. })
    ));

    // Scenario 9 & 8: CircleGatewayX402 session + run admission + nonce replay blocked + shared OrderId namespace
    let x402_ch_id = Bytes32([0x88; 32]);
    rt.register_session(
        autarqia_primitives::SessionId::new("sess_x402"),
        SettlementRail::CircleGatewayX402,
        x402_ch_id,
        buyer_addr,
        buyer_addr,
        TokenAtomicAmount::new(5_000),
        Bytes32::ZERO,
        None,
        Some(&x402_payload),
        clock,
    )
    .unwrap();
    // Replaying same x402 nonce is rejected
    assert!(rt
        .verify_and_apply_x402_authorization(x402_ch_id, &x402_payload, None, clock)
        .is_err());

    rt.admit_run(
        autarqia_primitives::RunId::new("run_x402_1"),
        autarqia_primitives::SessionId::new("sess_x402"),
        OrderId::new("ord_shared_01"),
        autarqia_primitives::PreparedInputId::new("prep_x402"),
        Bytes32::ZERO,
        10,
        4,
        None,
        None,
        clock,
    )
    .unwrap();
    rt.active_run_id = None;

    // Submitting `ord_shared_01` on `AndoMppiArcV1` is rejected by shared order idempotency namespace
    let dup_order = rt.admit_run(
        autarqia_primitives::RunId::new("run_ando_dup"),
        autarqia_primitives::SessionId::new("sess_ando"),
        OrderId::new("ord_shared_01"),
        autarqia_primitives::PreparedInputId::new("prep_ando"),
        Bytes32::ZERO,
        10,
        4,
        None,
        None,
        clock,
    );
    assert!(matches!(
        dup_order,
        Err(autarqia_runtime::RuntimeError::DuplicateOrderConflict(_))
    ));

    // Scenario 5: CloseRequested blocks new runs and queues settlement of highest verified voucher
    let close_effs = rt
        .reconcile_onchain_channel_state(
            ch_id,
            TokenAtomicAmount::new(5_000),
            TokenAtomicAmount::ZERO,
            Some(1_700_000_000),
            false,
        )
        .unwrap();
    assert!(close_effs
        .iter()
        .any(|e| matches!(e, autarqia_runtime::RuntimeEffect::QueueSettlementItem { .. })));
    let blocked_run = rt.admit_run(
        autarqia_primitives::RunId::new("run_on_closing"),
        autarqia_primitives::SessionId::new("sess_ando"),
        OrderId::new("ord_closing"),
        autarqia_primitives::PreparedInputId::new("prep_closing"),
        Bytes32::ZERO,
        10,
        4,
        None,
        None,
        clock,
    );
    assert!(matches!(
        blocked_run,
        Err(autarqia_runtime::RuntimeError::ChannelClosingOrClosed(_))
    ));

    // Scenario 6: settleBatch partitioner isolates bad channel
    let (valid_batch, quarantined_batch) = partition_valid_settlement_batch(
        cfg.chain_id,
        cfg.ando_mppi_proxy,
        &[
            SettlementBatchItem {
                channel_id: ch_id,
                cumulative_amount: valid_v_amt,
                signature: valid_v_sig,
            },
            SettlementBatchItem {
                channel_id: Bytes32([0x99; 32]),
                cumulative_amount: TokenAtomicAmount::new(500),
                signature: Default::default(),
            },
        ],
        &rt.channels.values().cloned().collect::<Vec<_>>(),
    );
    assert_eq!(valid_batch.len(), 1);
    assert_eq!(quarantined_batch.len(), 1);

    // Scenario 14 (reopen persistence): Reopening RocksDB store bumps fence and preserves state
    let db_dir = tempfile::tempdir().unwrap();
    {
        let s1 = RocksDbLedgerStore::open(db_dir.path()).unwrap();
        let (f1, e1) = s1.acquire_and_bump_fence().unwrap();
        assert_eq!((f1.0, e1.0), (1, 1));
        s1.commit_batch_sync(autarqia_store::AtomicCommitBatch {
            ledger_version: rt.ledger_version,
            runtime_checkpoint_json: Some(serde_json::to_string(&rt).unwrap()),
            channels: rt.channels.values().cloned().collect(),
            runs: vec![],
            receipts: vec![],
            orders: vec![],
            outbox_entries: vec![],
            wal_note: "crash_test".to_owned(),
        })
        .unwrap();
    }
    {
        let s2 = RocksDbLedgerStore::open(db_dir.path()).unwrap();
        let (f2, e2) = s2.acquire_and_bump_fence().unwrap();
        assert_eq!((f2.0, e2.0), (2, 2));
        let rec = s2.load_recovered_state().unwrap();
        assert!(rec.runtime_checkpoint_json.is_some());
        assert_eq!(rec.channels.len(), 2);
    }

    // Scenario 15: Filter EIP-7708 duplicate 18-dec transfer logs
    let usdc = default_arc_usdc_address();
    let eip7708 = EvmAddress::from_hex(ARC_EIP7708_SYSTEM_SENDER).unwrap();
    let topic = keccak256(b"Transfer(address,address,uint256)");
    let logs = vec![
        RawEvmLog {
            address: eip7708,
            topics: vec![topic, Bytes32::ZERO, Bytes32::ZERO],
            data: encode_u128_word(1_000_000_000_000_000_000).to_vec(),
        },
        RawEvmLog {
            address: usdc,
            topics: vec![topic, Bytes32::ZERO, Bytes32::ZERO],
            data: encode_u128_word(1_000_000).to_vec(),
        },
    ];
    let filtered = filter_arc_usdc_transfer_logs(&logs, usdc);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].amount_atomic_6dec.0, 1_000_000);

    // Scenario 16: Live Arc Testnet RPC verification of AndoMPPI proxy
    let arc_client = AndoMppiArcClient::arc_testnet_default();
    assert_eq!(arc_client.chain_id(), ARC_TESTNET_CHAIN_ID);
    assert_eq!(
        arc_client.proxy_address(),
        EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap()
    );
    let live_domain = arc_client.fetch_domain_separator().await.unwrap();
    assert_eq!(
        live_domain.to_hex(),
        "0x8c4a261dc84bafb5b349d4a1c8dbac75de4246972ac9531ade334e690b323768"
    );

    // Scenario 18: Workspace DAG verification
    let dag_report = verify_workspace_dag(&repo_root().join("Cargo.toml")).unwrap();
    assert!(dag_report.valid);
    assert_eq!(dag_report.checked_workspace_crates, 12);
}
