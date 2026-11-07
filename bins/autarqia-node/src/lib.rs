use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use autarqia_bufi::JsonlFileBufiSink;
use autarqia_energy::EnergyTelemetryHub;
use autarqia_inference::AirLlmUdsClient;
use autarqia_nucleus::NucleusHandle;
use autarqia_payments::AndoMppiArcClient;
use autarqia_primitives::{
    address_from_signing_key, Bytes32, EnergyPolicyConfig, EnergyTelemetrySnapshot, EvmAddress,
    ModelIntegrityManifest, MonotonicTimestampMs, RationalRate, RunId, TokenAtomicAmount,
    UsageTariff, ARC_TESTNET_ANDO_MPPI_PROXY, ARC_TESTNET_CHAIN_ID,
    ARC_TESTNET_GATEWAY_WALLET_PROXY, ARC_TESTNET_USDC_ADDRESS,
};
use autarqia_rpc::{build_router, NodeRpcFacade};
use autarqia_runtime::RuntimeConfig;
use autarqia_store::RocksDbLedgerStore;
use autarqia_subsystem_api::{
    CreateQuoteRequest, CreateQuoteResponse, CreateSessionRequest, CreateSessionResponse,
    NodeHealthSnapshot, ProviderDiscoveryDocument, RunControlCommand, RunControlResponse,
    RunReceipt, RunStreamFrame, StartRunRequest, StartRunResponse, SubsystemError,
};
use k256::ecdsa::SigningKey;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct NucleusRpcAdapter {
    pub nucleus: NucleusHandle,
}

#[async_trait]
impl NodeRpcFacade for NucleusRpcAdapter {
    async fn discovery_document(&self) -> ProviderDiscoveryDocument {
        self.nucleus.discovery_document().await
    }

    async fn create_quote(
        &self,
        req: CreateQuoteRequest,
    ) -> Result<CreateQuoteResponse, SubsystemError> {
        self.nucleus.create_quote(req).await
    }

    async fn create_session(
        &self,
        req: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, SubsystemError> {
        self.nucleus.create_session(req).await
    }

    async fn start_run(
        &self,
        bearer_token: Option<&str>,
        req: StartRunRequest,
    ) -> Result<(StartRunResponse, broadcast::Receiver<RunStreamFrame>), SubsystemError> {
        self.nucleus.start_run(bearer_token, req).await
    }

    async fn subscribe_run_events(
        &self,
        run_id: &RunId,
    ) -> Result<(Vec<RunStreamFrame>, broadcast::Receiver<RunStreamFrame>), SubsystemError> {
        self.nucleus.subscribe_run_events(run_id).await
    }

    async fn control_run(
        &self,
        run_id: &RunId,
        cmd: RunControlCommand,
    ) -> Result<RunControlResponse, SubsystemError> {
        self.nucleus.control_run(run_id, cmd).await
    }

    async fn get_receipt(&self, run_id: &RunId) -> Result<RunReceipt, SubsystemError> {
        self.nucleus.get_receipt(run_id).await
    }

    async fn inject_energy_telemetry(
        &self,
        snapshot: EnergyTelemetrySnapshot,
    ) -> Result<NodeHealthSnapshot, SubsystemError> {
        self.nucleus.inject_energy_telemetry(snapshot).await
    }

    async fn health_snapshot(&self) -> NodeHealthSnapshot {
        self.nucleus.health_snapshot().await
    }
}

pub fn default_runtime_config(provider_sk: &SigningKey) -> RuntimeConfig {
    let provider_addr = address_from_signing_key(provider_sk);
    RuntimeConfig {
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
            uncovered_exposure_cap: TokenAtomicAmount::new(250),
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
    }
}

pub async fn start_node_server(
    bind_addr: SocketAddr,
    db_path: &Path,
    uds_path: &Path,
    bufi_outbox_path: &Path,
    arc_rpc_url: &str,
    provider_sk: SigningKey,
    config_override: Option<RuntimeConfig>,
) -> Result<(SocketAddr, NucleusHandle, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let store = Arc::new(RocksDbLedgerStore::open(db_path)?);
    let energy_hub = EnergyTelemetryHub::default_solar_healthy(MonotonicTimestampMs::new(1));
    let payments = AndoMppiArcClient::new(
        arc_rpc_url,
        ARC_TESTNET_CHAIN_ID,
        EvmAddress::from_hex(ARC_TESTNET_ANDO_MPPI_PROXY).unwrap(),
    );
    let worker = AirLlmUdsClient::connect(uds_path).await?;
    let bufi_sink = Arc::new(JsonlFileBufiSink::new(bufi_outbox_path));
    let cfg = config_override.unwrap_or_else(|| default_runtime_config(&provider_sk));

    let nucleus = NucleusHandle::boot(
        cfg,
        provider_sk,
        store,
        energy_hub,
        payments,
        worker,
        bufi_sink,
    )
    .await?;

    let adapter = Arc::new(NucleusRpcAdapter {
        nucleus: nucleus.clone(),
    });
    let router = build_router(adapter);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Ok((local_addr, nucleus, join))
}
