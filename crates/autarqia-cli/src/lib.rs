use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use autarqia_primitives::{
    address_from_signing_key, advance_prefix_state, ando_mppi_domain_separator,
    ando_mppi_voucher_digest, autarqia_mppi_run_domain_separator, compute_channel_id,
    compute_initial_prefix_state, gateway_wallet_batched_domain_separator, recover_eip712_signer,
    sign_eip712_digest, usage_ack_digest, usage_attestation_digest,
    x402_transfer_with_authorization_digest, Bytes32, ChannelId, ChatMessage, EvmAddress, OrderId,
    RunReceipt, SettlementRail, SignedUsageAck, SignedVoucher, TokenAtomicAmount, UsageAck,
    WallClockUnixMs, X402AuthorizationPayload, AUTARQIA_MPPI_PROFILE_ID,
};
use autarqia_subsystem_api::{
    CreateQuoteRequest, CreateQuoteResponse, CreateSessionRequest, CreateSessionResponse,
    ProviderDiscoveryDocument, RunControlCommand, RunControlResponse, RunStreamFrame,
    StartRunRequest, StartRunResponse,
};
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("DAG verification failed: {0}")]
    DagViolation(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol verification error: {0}")]
    Protocol(String),
}

const FORBIDDEN_LAYER12_IO_CRATES: &[&str] = &[
    "tokio",
    "mio",
    "hyper",
    "reqwest",
    "axum",
    "tonic",
    "rocksdb",
    "librocksdb-sys",
    "rusqlite",
    "sqlx",
    "pyo3",
];

const FORBIDDEN_LAYER4_RPC_CRATES: &[&str] = &[
    "autarqia-store",
    "autarqia-inference",
    "autarqia-payments",
    "rocksdb",
    "librocksdb-sys",
    "tonic",
];

pub fn crate_layer(name: &str) -> Option<(u8, bool)> {
    match name {
        "autarqia-primitives" | "autarqia-subsystem-api" => Some((1, false)),
        "autarqia-runtime" => Some((2, false)),
        "autarqia-store"
        | "autarqia-energy"
        | "autarqia-payments"
        | "autarqia-inference"
        | "autarqia-bufi" => Some((3, true)),
        "autarqia-nucleus" => Some((3, false)),
        "autarqia-rpc" => Some((4, false)),
        "autarqia-cli" | "autarqia-node" => Some((5, false)),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DagVerificationReport {
    pub valid: bool,
    pub checked_workspace_crates: usize,
    pub violations: Vec<String>,
}

pub fn verify_cargo_metadata_json(metadata_json: &str) -> Result<DagVerificationReport, CliError> {
    let v: serde_json::Value = serde_json::from_str(metadata_json)?;
    let packages = v
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| CliError::DagViolation("missing packages array".to_owned()))?;
    let resolve_nodes = v
        .get("resolve")
        .and_then(|r| r.get("nodes"))
        .and_then(|n| n.as_array())
        .ok_or_else(|| CliError::DagViolation("missing resolve.nodes array".to_owned()))?;

    let mut id_to_name: BTreeMap<String, String> = BTreeMap::new();
    for pkg in packages {
        if let (Some(id), Some(name)) = (
            pkg.get("id").and_then(|x| x.as_str()),
            pkg.get("name").and_then(|x| x.as_str()),
        ) {
            id_to_name.insert(id.to_owned(), name.to_owned());
        }
    }

    let mut normal_adj: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in resolve_nodes {
        let Some(id) = node.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(pkg_name) = id_to_name.get(id).cloned() else {
            continue;
        };
        let mut deps_out = Vec::new();
        if let Some(deps) = node.get("deps").and_then(|d| d.as_array()) {
            for dep in deps {
                let Some(dep_pkg_id) = dep.get("pkg").and_then(|x| x.as_str()) else {
                    continue;
                };
                let is_normal = dep
                    .get("dep_kinds")
                    .and_then(|dk| dk.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|k| k.get("kind").map(|v| v.is_null()).unwrap_or(true))
                    })
                    .unwrap_or(true);
                if is_normal {
                    if let Some(dep_name) = id_to_name.get(dep_pkg_id) {
                        deps_out.push(dep_name.clone());
                    }
                }
            }
        }
        normal_adj.insert(pkg_name, deps_out);
    }

    let mut violations = Vec::new();
    let mut workspace_count = 0usize;

    for (src_name, direct_deps) in &normal_adj {
        let Some((src_layer, src_is_leaf_l3)) = crate_layer(src_name) else {
            continue;
        };
        workspace_count += 1;

        for dst_name in direct_deps {
            if let Some((dst_layer, dst_is_leaf_l3)) = crate_layer(dst_name) {
                if dst_layer > src_layer {
                    violations.push(format!(
                        "upward layer violation: {src_name} (L{src_layer}) depends on {dst_name} (L{dst_layer})"
                    ));
                }
                if src_layer == 1 && dst_name != "autarqia-primitives" {
                    violations.push(format!(
                        "layer 1 violation: {src_name} depends on {dst_name}"
                    ));
                }
                if src_is_leaf_l3 && dst_layer == 3 {
                    let _ = dst_is_leaf_l3;
                    violations.push(format!(
                        "layer 3 leaf isolation violation: leaf subsystem {src_name} depends on layer 3 crate {dst_name}"
                    ));
                }
            }
        }

        let closure = transitive_closure(src_name, &normal_adj);
        if src_layer <= 2 {
            for forbidden in FORBIDDEN_LAYER12_IO_CRATES {
                if closure.contains(*forbidden) {
                    violations.push(format!(
                        "zero-I/O closure violation: {src_name} (L{src_layer}) transitively depends on forbidden I/O crate `{forbidden}`"
                    ));
                }
            }
        }
        if src_name == "autarqia-rpc" {
            for forbidden in FORBIDDEN_LAYER4_RPC_CRATES {
                if closure.contains(*forbidden) {
                    violations.push(format!(
                        "layer 4 RPC isolation violation: autarqia-rpc transitively depends on `{forbidden}`"
                    ));
                }
            }
        }
    }

    Ok(DagVerificationReport {
        valid: violations.is_empty(),
        checked_workspace_crates: workspace_count,
        violations,
    })
}

fn transitive_closure(start: &str, adj: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    let mut visited = BTreeSet::new();
    let mut q = VecDeque::new();
    if let Some(first) = adj.get(start) {
        for d in first {
            if visited.insert(d.clone()) {
                q.push_back(d.clone());
            }
        }
    }
    while let Some(curr) = q.pop_front() {
        if let Some(nexts) = adj.get(&curr) {
            for n in nexts {
                if visited.insert(n.clone()) {
                    q.push_back(n.clone());
                }
            }
        }
    }
    visited
}

pub fn verify_workspace_dag(manifest_path: &Path) -> Result<DagVerificationReport, CliError> {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()
        .map_err(|e| CliError::DagViolation(format!("failed to execute cargo metadata: {e}")))?;
    if !output.status.success() {
        return Err(CliError::DagViolation(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let json_str = String::from_utf8_lossy(&output.stdout);
    let report = verify_cargo_metadata_json(&json_str)?;
    if !report.valid {
        return Err(CliError::DagViolation(report.violations.join("; ")));
    }
    Ok(report)
}

#[derive(Clone, Debug)]
pub struct BuyerRunOptions {
    pub rail: SettlementRail,
    pub model_id: String,
    pub messages: Vec<ChatMessage>,
    pub max_output_tokens: u32,
    pub channel_deposit_atomic: TokenAtomicAmount,
    pub initial_voucher_atomic: TokenAtomicAmount,
    pub stop_acks_after_tokens: Option<u32>,
    pub order_id: Option<OrderId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuyerRunOutcome {
    pub session_id: String,
    pub run_id: String,
    pub rail: SettlementRail,
    pub assembled_text: String,
    pub tokens_received: u32,
    pub attestations_verified: u32,
    pub acks_sent: u32,
    pub highest_voucher_signed_atomic: TokenAtomicAmount,
    pub payment_required_emitted: bool,
    pub receipt: RunReceipt,
}

#[derive(Clone)]
pub struct AutonomousBuyerClient {
    base_url: String,
    buyer_sk: SigningKey,
    http: reqwest::Client,
}

impl AutonomousBuyerClient {
    pub fn new(base_url: impl Into<String>, buyer_sk: SigningKey) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            buyer_sk,
            http: reqwest::Client::new(),
        }
    }

    pub fn buyer_address(&self) -> EvmAddress {
        address_from_signing_key(&self.buyer_sk)
    }

    pub async fn fetch_discovery(&self) -> Result<ProviderDiscoveryDocument, CliError> {
        let url = format!("{}/.well-known/autarqia/provider.json", self.base_url);
        let doc: ProviderDiscoveryDocument = self.http.get(url).send().await?.json().await?;
        if doc.profile_id != AUTARQIA_MPPI_PROFILE_ID {
            return Err(CliError::Protocol(format!(
                "unexpected profile_id {}",
                doc.profile_id
            )));
        }
        if doc.model_manifest.trust_remote_code {
            return Err(CliError::Protocol(
                "provider advertises trust_remote_code=true".to_owned(),
            ));
        }
        Ok(doc)
    }

    pub async fn execute_run(&self, opts: BuyerRunOptions) -> Result<BuyerRunOutcome, CliError> {
        let doc = self.fetch_discovery().await?;
        let buyer_addr = self.buyer_address();

        let quote_req = CreateQuoteRequest {
            model_id: opts.model_id.clone(),
            rail: opts.rail,
            estimated_input_tokens: 32,
            max_output_tokens: opts.max_output_tokens,
        };
        let quote: CreateQuoteResponse = self
            .http
            .post(format!("{}/v1/mppi/quotes", self.base_url))
            .json(&quote_req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut salt_bytes = [0u8; 32];
        salt_bytes[0..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let salt = Bytes32(salt_bytes);
        let channel_id: ChannelId = compute_channel_id(
            buyer_addr,
            doc.provider_payee,
            doc.usdc_token,
            salt,
            buyer_addr,
            doc.ando_mppi_proxy,
            doc.chain_id,
        );

        let ando_domain = ando_mppi_domain_separator(doc.chain_id, doc.ando_mppi_proxy);
        let run_domain = autarqia_mppi_run_domain_separator(doc.chain_id, doc.ando_mppi_proxy);

        let (initial_voucher, x402_auth, mut highest_voucher_atomic) = match opts.rail {
            SettlementRail::AndoMppiArcV1 => {
                if opts.initial_voucher_atomic.0 > 0 {
                    let digest = ando_mppi_voucher_digest(
                        ando_domain,
                        channel_id,
                        opts.initial_voucher_atomic,
                    );
                    let sig = sign_eip712_digest(&self.buyer_sk, digest)
                        .map_err(|e| CliError::Protocol(e.to_string()))?;
                    (
                        Some(SignedVoucher {
                            channel_id,
                            cumulative_amount: opts.initial_voucher_atomic,
                            signature: sig,
                        }),
                        None,
                        opts.initial_voucher_atomic,
                    )
                } else {
                    (None, None, TokenAtomicAmount::ZERO)
                }
            }
            SettlementRail::CircleGatewayX402 => {
                let now_s = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let mut nonce_b = [0u8; 32];
                nonce_b[0..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
                let mut payload = X402AuthorizationPayload {
                    from: buyer_addr,
                    to: doc.provider_payee,
                    value: opts.initial_voucher_atomic,
                    valid_after: now_s.saturating_sub(60),
                    valid_before: now_s.saturating_add(3600),
                    nonce: Bytes32(nonce_b),
                    signature: Default::default(),
                };
                let gw_domain = gateway_wallet_batched_domain_separator(
                    doc.chain_id,
                    doc.gateway_wallet_proxy,
                );
                let digest = x402_transfer_with_authorization_digest(gw_domain, &payload);
                payload.signature = sign_eip712_digest(&self.buyer_sk, digest)
                    .map_err(|e| CliError::Protocol(e.to_string()))?;
                (None, Some(payload), opts.initial_voucher_atomic)
            }
        };

        let sess_req = CreateSessionRequest {
            quote_id: Some(quote.quote_id),
            rail: opts.rail,
            payer: buyer_addr,
            buyer_signer: buyer_addr,
            channel_id,
            onchain_deposit_atomic: opts.channel_deposit_atomic,
            initial_voucher,
            x402_authorization: x402_auth,
        };
        let sess: CreateSessionResponse = self
            .http
            .post(format!("{}/v1/mppi/sessions", self.base_url))
            .json(&sess_req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let order_id = opts
            .order_id
            .unwrap_or_else(|| OrderId::new(format!("ord_{}", uuid::Uuid::new_v4().simple())));
        let start_req = StartRunRequest {
            session_id: sess.session_id.clone(),
            order_id,
            model_id: opts.model_id.clone(),
            messages: opts.messages,
            max_output_tokens: opts.max_output_tokens,
            temperature_milli: 0,
            top_p_milli: 1000,
            seed: 42,
            initial_voucher: None,
            x402_authorization: None,
        };
        let started: StartRunResponse = self
            .http
            .post(format!("{}/v1/mppi/runs", self.base_url))
            .bearer_auth(&sess.bearer_token)
            .json(&start_req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let sse_resp = self
            .http
            .get(format!(
                "{}/v1/mppi/runs/{}/events",
                self.base_url, started.run_id
            ))
            .send()
            .await?
            .error_for_status()?;

        let mut byte_stream = sse_resp.bytes_stream();
        let mut buffer = String::new();
        let mut rolling_prefix = compute_initial_prefix_state(started.run_id.as_str());
        let mut assembled_text = String::new();
        let mut tokens_received = 0u32;
        let mut attestations_verified = 0u32;
        let mut acks_sent = 0u32;
        let mut payment_required_emitted = false;
        let mut final_receipt: Option<RunReceipt> = None;

        while let Some(chunk) = byte_stream.next().await {
            let bytes = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buffer.find("\n\n") {
                let raw_event = buffer[..pos].to_owned();
                buffer.drain(..pos + 2);
                for line in raw_event.lines() {
                    let Some(data_json) = line.strip_prefix("data:").map(str::trim) else {
                        continue;
                    };
                    if data_json.is_empty() {
                        continue;
                    }
                    let frame: RunStreamFrame = serde_json::from_str(data_json)?;
                    match frame {
                        RunStreamFrame::TokenDelta {
                            output_seq,
                            token_id,
                            text_delta,
                            prefix_commitment,
                            ..
                        } => {
                            if output_seq == tokens_received + 1 {
                                rolling_prefix = advance_prefix_state(
                                    rolling_prefix,
                                    output_seq,
                                    token_id,
                                    &text_delta,
                                );
                                if rolling_prefix != prefix_commitment {
                                    return Err(CliError::Protocol(format!(
                                        "buyer detected prefix commitment mismatch at seq {output_seq}"
                                    )));
                                }
                                tokens_received = output_seq;
                                assembled_text.push_str(&text_delta);
                            }
                        }
                        RunStreamFrame::UsageAttestation {
                            signed_attestation, ..
                        } => {
                            let att = &signed_attestation.attestation;
                            let digest = usage_attestation_digest(run_domain, att);
                            let recovered =
                                recover_eip712_signer(digest, &signed_attestation.signature)
                                    .map_err(|e| CliError::Protocol(e.to_string()))?;
                            if recovered != doc.provider_signer {
                                return Err(CliError::Protocol(format!(
                                    "attestation signer mismatch: {recovered} != {}",
                                    doc.provider_signer
                                )));
                            }
                            attestations_verified += 1;

                            if let Some(stop_after) = opts.stop_acks_after_tokens {
                                if att.output_seq > stop_after {
                                    continue;
                                }
                            }

                            let stride_headroom = doc
                                .tariff
                                .output_rate
                                .ceil_charge(doc.tariff.ack_stride_tokens.saturating_mul(2))
                                .unwrap_or(TokenAtomicAmount::new(200));
                            let desired_voucher = att
                                .cumulative_due_atomic
                                .checked_add(stride_headroom)
                                .unwrap_or(att.cumulative_due_atomic)
                                .min(opts.channel_deposit_atomic);

                            let voucher_opt = if opts.rail == SettlementRail::AndoMppiArcV1
                                && desired_voucher > highest_voucher_atomic
                            {
                                let v_digest = ando_mppi_voucher_digest(
                                    ando_domain,
                                    channel_id,
                                    desired_voucher,
                                );
                                let v_sig = sign_eip712_digest(&self.buyer_sk, v_digest)
                                    .map_err(|e| CliError::Protocol(e.to_string()))?;
                                highest_voucher_atomic = desired_voucher;
                                Some(SignedVoucher {
                                    channel_id,
                                    cumulative_amount: desired_voucher,
                                    signature: v_sig,
                                })
                            } else {
                                None
                            };

                            let now_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let ack = UsageAck {
                                session_id: sess.session_id.clone(),
                                run_id: started.run_id.clone(),
                                channel_id,
                                acknowledged_output_seq: att.output_seq,
                                acknowledged_cumulative_due_atomic: att.cumulative_due_atomic,
                                voucher_cumulative_amount_atomic: highest_voucher_atomic,
                                prefix_commitment: att.prefix_commitment,
                                issued_at_unix_ms: WallClockUnixMs::new(now_ms),
                            };
                            let ack_sig =
                                sign_eip712_digest(&self.buyer_sk, usage_ack_digest(run_domain, &ack))
                                    .map_err(|e| CliError::Protocol(e.to_string()))?;
                            let ctrl_cmd = RunControlCommand::AckUsage {
                                signed_ack: SignedUsageAck {
                                    ack,
                                    buyer_signer: buyer_addr,
                                    signature: ack_sig,
                                    voucher: voucher_opt,
                                },
                            };
                            let ctrl_resp: RunControlResponse = self
                                .http
                                .post(format!(
                                    "{}/v1/mppi/runs/{}/control",
                                    self.base_url, started.run_id
                                ))
                                .json(&ctrl_cmd)
                                .send()
                                .await?
                                .error_for_status()?
                                .json()
                                .await?;
                            if ctrl_resp.accepted {
                                acks_sent += 1;
                            }
                        }
                        RunStreamFrame::PaymentRequired { .. } => {
                            payment_required_emitted = true;
                        }
                        RunStreamFrame::ReceiptReady { receipt } => {
                            final_receipt = Some(receipt);
                            break;
                        }
                        _ => {}
                    }
                }
                if final_receipt.is_some() {
                    break;
                }
            }
            if final_receipt.is_some() {
                break;
            }
        }

        let receipt = match final_receipt {
            Some(r) => r,
            None => {
                self.http
                    .get(format!(
                        "{}/v1/mppi/runs/{}/receipt",
                        self.base_url, started.run_id
                    ))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?
            }
        };

        Ok(BuyerRunOutcome {
            session_id: sess.session_id.to_string(),
            run_id: started.run_id.to_string(),
            rail: opts.rail,
            assembled_text,
            tokens_received,
            attestations_verified,
            acks_sent,
            highest_voucher_signed_atomic: highest_voucher_atomic,
            payment_required_emitted,
            receipt,
        })
    }
}

#[derive(Clone, Deserialize)]
struct ProxyChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    max_tokens: Option<u32>,
}

pub async fn start_local_openai_mppi_proxy(
    bind_addr: SocketAddr,
    provider_url: String,
    buyer_sk: SigningKey,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let client = Arc::new(AutonomousBuyerClient::new(provider_url, buyer_sk));
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(buyer): State<Arc<AutonomousBuyerClient>>,
                 Json(req): Json<ProxyChatRequest>| async move {
                    let opts = BuyerRunOptions {
                        rail: SettlementRail::AndoMppiArcV1,
                        model_id: req.model.clone(),
                        messages: req.messages,
                        max_output_tokens: req.max_tokens.unwrap_or(8),
                        channel_deposit_atomic: TokenAtomicAmount::new(50_000),
                        initial_voucher_atomic: TokenAtomicAmount::new(1_000),
                        stop_acks_after_tokens: None,
                        order_id: None,
                    };
                    match buyer.execute_run(opts).await {
                        Ok(outcome) => (
                            axum::http::StatusCode::OK,
                            Json(json!({
                                "id": outcome.run_id,
                                "object": "chat.completion",
                                "model": req.model,
                                "choices": [
                                    {
                                        "index": 0,
                                        "message": {
                                            "role": "assistant",
                                            "content": outcome.assembled_text,
                                        },
                                        "finish_reason": "stop",
                                    }
                                ],
                                "autarqia_mppi": {
                                    "session_id": outcome.session_id,
                                    "tokens_received": outcome.tokens_received,
                                    "acks_sent": outcome.acks_sent,
                                    "highest_voucher_signed_atomic": outcome.highest_voucher_signed_atomic,
                                    "receipt": outcome.receipt,
                                }
                            })),
                        ),
                        Err(e) => (
                            axum::http::StatusCode::BAD_GATEWAY,
                            Json(json!({ "error": e.to_string() })),
                        ),
                    }
                },
            ),
        )
        .with_state(client);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((local_addr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_fixture_rejects_layer1_tokio_dependency() {
        let bad_meta = r#"{
            "packages": [
                {"id": "p1", "name": "autarqia-primitives"},
                {"id": "t1", "name": "tokio"}
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "p1",
                        "deps": [{"pkg": "t1", "dep_kinds": [{"kind": null}]}]
                    },
                    {"id": "t1", "deps": []}
                ]
            }
        }"#;
        let report = verify_cargo_metadata_json(bad_meta).unwrap();
        assert!(!report.valid);
        assert!(report
            .violations
            .iter()
            .any(|v| v.contains("zero-I/O closure violation") && v.contains("tokio")));
    }

    #[test]
    fn negative_fixture_rejects_layer2_to_layer3_and_leaf_to_leaf() {
        let bad_meta = r#"{
            "packages": [
                {"id": "r1", "name": "autarqia-runtime"},
                {"id": "s1", "name": "autarqia-store"},
                {"id": "p1", "name": "autarqia-payments"}
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "r1",
                        "deps": [{"pkg": "s1", "dep_kinds": [{"kind": null}]}]
                    },
                    {
                        "id": "s1",
                        "deps": [{"pkg": "p1", "dep_kinds": [{"kind": null}]}]
                    },
                    {"id": "p1", "deps": []}
                ]
            }
        }"#;
        let report = verify_cargo_metadata_json(bad_meta).unwrap();
        assert!(!report.valid);
        assert!(report
            .violations
            .iter()
            .any(|v| v.contains("upward layer violation")));
        assert!(report
            .violations
            .iter()
            .any(|v| v.contains("layer 3 leaf isolation violation")));
    }

    #[test]
    fn negative_fixture_rejects_layer4_rpc_to_rocksdb() {
        let bad_meta = r#"{
            "packages": [
                {"id": "rpc1", "name": "autarqia-rpc"},
                {"id": "s1", "name": "autarqia-store"}
            ],
            "resolve": {
                "nodes": [
                    {
                        "id": "rpc1",
                        "deps": [{"pkg": "s1", "dep_kinds": [{"kind": null}]}]
                    },
                    {"id": "s1", "deps": []}
                ]
            }
        }"#;
        let report = verify_cargo_metadata_json(bad_meta).unwrap();
        assert!(!report.valid);
        assert!(report
            .violations
            .iter()
            .any(|v| v.contains("layer 4 RPC isolation violation")));
    }
}
