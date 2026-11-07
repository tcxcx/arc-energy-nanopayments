use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use autarqia_cli::{AutonomousBuyerClient, BuyerRunOptions};
use autarqia_node::{default_runtime_config, start_node_server};
use autarqia_primitives::{
    address_from_signing_key, advance_prefix_state, ando_mppi_domain_separator,
    ando_mppi_voucher_digest, autarqia_mppi_run_domain_separator, compute_channel_id,
    compute_initial_prefix_state, sign_eip712_digest, usage_ack_digest, Bytes32, ChatMessage,
    MonotonicTimestampMs, OrderId, PreparedInputId, RunId, SessionId, SettlementRail,
    SignedUsageAck, SignedVoucher, TokenAtomicAmount, UsageAck, WallClockUnixMs,
};
use autarqia_runtime::{ClockContext, RuntimeState};
use k256::ecdsa::SigningKey;

struct WorkerGuard {
    child: Child,
}

impl Drop for WorkerGuard {
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

fn spawn_worker(uds_path: &Path) -> WorkerGuard {
    let root = repo_root();
    let mut child = Command::new(root.join("workers/airllm/.venv/bin/python"))
        .arg(root.join("workers/airllm/server.py"))
        .arg("--uds-path")
        .arg(uds_path)
        .arg("--model-dir")
        .arg(root.join("data/models/Qwen--Qwen2.5-0.5B-Instruct"))
        .arg("--worker-epoch")
        .arg("1")
        .arg("--grace-wait-ms")
        .arg("500")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0);
        if line.contains("airllm_worker_started") {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut s = String::new();
        while reader.read_line(&mut s).unwrap_or(0) > 0 {
            s.clear();
        }
    });
    WorkerGuard { child }
}

fn percentile_us(sorted: &[u128], pct: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

#[tokio::test]
async fn benchmark_gateway_runtime_and_real_airllm_e2e() {
    let provider_sk = SigningKey::from_bytes((&[0x22u8; 32]).into()).unwrap();
    let buyer_sk = SigningKey::from_bytes((&[0x33u8; 32]).into()).unwrap();
    let buyer_addr = address_from_signing_key(&buyer_sk);
    let cfg = default_runtime_config(&provider_sk);

    let mut rt = RuntimeState::new(
        cfg.clone(),
        autarqia_primitives::WorkerEpoch::new(1),
        autarqia_primitives::OwnerFence::new(1),
    );
    let clock = ClockContext {
        monotonic_ms: MonotonicTimestampMs::new(1_000),
        unix_ms: WallClockUnixMs::new(1_700_000_001_000),
    };
    let bench_energy = autarqia_primitives::EnergyTelemetrySnapshot {
        solar_power_mw: autarqia_primitives::Milliwatts(55_000),
        battery_power_mw: autarqia_primitives::Milliwatts(12_000),
        load_power_mw: autarqia_primitives::Milliwatts(14_000),
        grid_power_mw: autarqia_primitives::Milliwatts(0),
        battery_soc_bps: autarqia_primitives::BasisPoints(8_500),
        panel_voltage_mv: autarqia_primitives::Millivolts(18_500),
        panel_current_ma: autarqia_primitives::Milliamps(2_600),
        temp_mc: autarqia_primitives::MillidegreesCelsius(28_000),
        measured_at_monotonic_ms: MonotonicTimestampMs::new(990),
        source: "bench".to_owned(),
    };
    rt.latest_energy = Some(bench_energy.clone());

    let ch_id = compute_channel_id(
        buyer_addr,
        cfg.provider_payee,
        cfg.usdc_token,
        Bytes32::ZERO,
        buyer_addr,
        cfg.ando_mppi_proxy,
        cfg.chain_id,
    );
    rt.register_session(
        SessionId::new("sess_bench"),
        SettlementRail::AndoMppiArcV1,
        ch_id,
        buyer_addr,
        buyer_addr,
        TokenAtomicAmount::new(10_000_000),
        Bytes32::ZERO,
        None,
        None,
        clock,
    )
    .unwrap();

    let ando_domain = ando_mppi_domain_separator(cfg.chain_id, cfg.ando_mppi_proxy);
    let run_domain = autarqia_mppi_run_domain_separator(cfg.chain_id, cfg.ando_mppi_proxy);

    let mut step_latencies_us = Vec::with_capacity(200);
    for i in 0..220u32 {
        let run_id_str = format!("run_bench_{i}");
        let run_id = RunId::new(&run_id_str);
        let ord_id = OrderId::new(format!("ord_bench_{i}"));
        let p0 = compute_initial_prefix_state(&run_id_str);
        let p1 = advance_prefix_state(p0, 1, 101, "tok1");
        let p2 = advance_prefix_state(p1, 2, 102, "tok2");

        let v_amt = TokenAtomicAmount::new(u128::from((i + 1) * 1_000));
        let v_sig =
            sign_eip712_digest(&buyer_sk, ando_mppi_voucher_digest(ando_domain, ch_id, v_amt))
                .unwrap();
        let ack = UsageAck {
            session_id: SessionId::new("sess_bench"),
            run_id: run_id.clone(),
            channel_id: ch_id,
            acknowledged_output_seq: 2,
            acknowledged_cumulative_due_atomic: TokenAtomicAmount::new(300),
            voucher_cumulative_amount_atomic: v_amt,
            prefix_commitment: p2,
            issued_at_unix_ms: clock.unix_ms,
        };
        let ack_sig = sign_eip712_digest(&buyer_sk, usage_ack_digest(run_domain, &ack)).unwrap();
        let signed_ack = SignedUsageAck {
            ack,
            buyer_signer: buyer_addr,
            signature: ack_sig,
            voucher: Some(SignedVoucher {
                channel_id: ch_id,
                cumulative_amount: v_amt,
                signature: v_sig,
            }),
        };

        let t0 = Instant::now();
        rt.admit_run(
            run_id.clone(),
            SessionId::new("sess_bench"),
            ord_id,
            PreparedInputId::new("prep_bench"),
            Bytes32::ZERO,
            10,
            6,
            None,
            None,
            clock,
        )
        .unwrap();
        rt.on_worker_prefill_completed(&run_id, 10).unwrap();
        rt.on_worker_token_delta(&run_id, 1, 101, "tok1", p1, clock)
            .unwrap();
        rt.on_worker_token_delta(&run_id, 2, 102, "tok2", p2, clock)
            .unwrap();
        rt.on_buyer_usage_ack(&run_id, &signed_ack, 6, clock)
            .unwrap();
        rt.finalize_run_from_worker(&run_id, "completed", 2, p2, clock)
            .unwrap();
        let elapsed_us = t0.elapsed().as_micros();
        if i >= 20 {
            step_latencies_us.push(elapsed_us);
        }
    }
    step_latencies_us.sort_unstable();

    let tmp = tempfile::tempdir().unwrap();
    let uds_path = tmp.path().join("bench-airllm.sock");
    let db_path = tmp.path().join("bench-rocksdb");
    let bufi_path = tmp.path().join("bench-bufi.jsonl");

    let _worker = spawn_worker(&uds_path);
    let (node_addr, _nucleus, _join) = start_node_server(
        "127.0.0.1:0".parse().unwrap(),
        &db_path,
        &uds_path,
        &bufi_path,
        "https://rpc.testnet.arc.network",
        provider_sk,
        None,
    )
    .await
    .unwrap();
    let buyer = AutonomousBuyerClient::new(format!("http://{node_addr}"), buyer_sk);

    let mut e2e_latencies_ms = Vec::new();
    let mut per_token_latencies_ms = Vec::new();
    for idx in 0..6u32 {
        _nucleus
            .inject_energy_telemetry(bench_energy.clone())
            .await
            .unwrap();
        let t0 = Instant::now();
        let out = buyer
            .execute_run(BuyerRunOptions {
                rail: SettlementRail::AndoMppiArcV1,
                model_id: "Qwen/Qwen2.5-0.5B-Instruct".to_owned(),
                messages: vec![ChatMessage {
                    role: "user".to_owned(),
                    content: "State one word about solar energy.".to_owned(),
                }],
                max_output_tokens: 4,
                channel_deposit_atomic: TokenAtomicAmount::new(50_000),
                initial_voucher_atomic: TokenAtomicAmount::new(500),
                stop_acks_after_tokens: None,
                order_id: Some(OrderId::new(format!("ord_e2e_bench_{idx}"))),
            })
            .await
            .unwrap();
        let elapsed_ms = t0.elapsed().as_millis();
        assert!(out.tokens_received >= 1);
        if idx >= 1 {
            e2e_latencies_ms.push(elapsed_ms);
            per_token_latencies_ms.push(elapsed_ms / u128::from(out.tokens_received));
        }
    }
    e2e_latencies_ms.sort_unstable();
    per_token_latencies_ms.sort_unstable();

    let report = serde_json::json!({
        "hardware": "Apple M3 Pro (18 GB unified memory, macOS arm64, MPS bfloat16)",
        "model": {
            "id": "Qwen/Qwen2.5-0.5B-Instruct",
            "revision": "7ae557604adf67be50417f59c2c2f167def9a775",
            "split_shards": 26,
            "airllm_commit": "c4c9de3d41e73770b95716bd447e53f049a2bc85"
        },
        "runtime_control_plane_microbench_us": {
            "warmup_iterations": 20,
            "measured_iterations": 200,
            "min_us": step_latencies_us[0],
            "p50_us": percentile_us(&step_latencies_us, 50),
            "p95_us": percentile_us(&step_latencies_us, 95),
            "max_us": *step_latencies_us.last().unwrap(),
        },
        "real_airllm_e2e_4tok_run_ms": {
            "warmup_runs": 1,
            "measured_runs": 5,
            "min_ms": e2e_latencies_ms[0],
            "p50_ms": percentile_us(&e2e_latencies_ms, 50),
            "p95_ms": percentile_us(&e2e_latencies_ms, 95),
            "max_ms": *e2e_latencies_ms.last().unwrap(),
            "per_token_p50_ms": percentile_us(&per_token_latencies_ms, 50),
        }
    });
    println!("BENCHMARK_REPORT_JSON={}", serde_json::to_string_pretty(&report).unwrap());
}
