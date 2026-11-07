use std::path::{Path, PathBuf};

use autarqia_primitives::{
    AdmissionEpoch, AllowanceSeq, Bytes32, ChatMessage, InternalWorkAllowance, LedgerVersion,
    ModelIntegrityManifest, MonotonicTimestampMs, OwnerFence, PreparedInputId, RunId, WorkerEpoch,
};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

pub mod pb {
    tonic::include_proto!("autarqia.worker.v1");
}

use pb::air_llm_worker_service_client::AirLlmWorkerServiceClient;

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("rpc status error: {0}")]
    Status(#[from] tonic::Status),
    #[error("invalid hash or commitment from worker: {0}")]
    InvalidWorkerData(String),
}

#[derive(Clone, Debug)]
pub struct PreparedWorkerRun {
    pub prepared_input_id: PreparedInputId,
    pub run_id: RunId,
    pub input_token_count: u32,
    pub input_token_ids: Vec<u32>,
    pub prompt_sha256: Bytes32,
    pub tested_context_limit: u32,
}

#[derive(Clone)]
pub struct AirLlmUdsClient {
    uds_path: PathBuf,
    channel: Channel,
}

impl AirLlmUdsClient {
    pub async fn connect(uds_path: impl AsRef<Path>) -> Result<Self, InferenceError> {
        let path_buf = uds_path.as_ref().to_path_buf();
        let path_for_closure = path_buf.clone();
        let channel = Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(service_fn(move |_: Uri| {
                let p = path_for_closure.clone();
                async move {
                    let stream = UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await?;
        Ok(Self {
            uds_path: path_buf,
            channel,
        })
    }

    pub fn uds_path(&self) -> &Path {
        &self.uds_path
    }

    fn client(&self) -> AirLlmWorkerServiceClient<Channel> {
        AirLlmWorkerServiceClient::new(self.channel.clone())
    }

    pub async fn describe_worker(
        &self,
    ) -> Result<(pb::DescribeWorkerResponse, ModelIntegrityManifest), InferenceError> {
        let resp = self
            .client()
            .describe_worker(pb::DescribeWorkerRequest {})
            .await?
            .into_inner();
        let manifest = ModelIntegrityManifest {
            engine_name: resp.engine_name.clone(),
            airllm_commit: resp.airllm_commit.clone(),
            patch_sha256: Bytes32::from_hex(&resp.patch_sha256)
                .map_err(|e| InferenceError::InvalidWorkerData(e.to_string()))?,
            model_id: resp.model_id.clone(),
            model_revision: resp.model_revision.clone(),
            tokenizer_sha256: Bytes32::from_hex(&resp.tokenizer_sha256)
                .map_err(|e| InferenceError::InvalidWorkerData(e.to_string()))?,
            chat_template_sha256: Bytes32::from_hex(&resp.chat_template_sha256)
                .map_err(|e| InferenceError::InvalidWorkerData(e.to_string()))?,
            original_manifest_sha256: Bytes32::from_hex(&resp.original_manifest_sha256)
                .map_err(|e| InferenceError::InvalidWorkerData(e.to_string()))?,
            split_manifest_sha256: Bytes32::from_hex(&resp.split_manifest_sha256)
                .map_err(|e| InferenceError::InvalidWorkerData(e.to_string()))?,
            runtime_device: resp.runtime_device.clone(),
            runtime_dtype: resp.runtime_dtype.clone(),
            tested_context_limit: resp.tested_context_limit,
            max_prefill_tokens: resp.max_prefill_tokens,
            uninterruptible_step_tokens: resp.uninterruptible_step_tokens,
            trust_remote_code: resp.trust_remote_code,
        };
        Ok((resp, manifest))
    }

    pub async fn health(&self) -> Result<pb::HealthResponse, InferenceError> {
        Ok(self
            .client()
            .health(pb::HealthRequest {})
            .await?
            .into_inner())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prepare(
        &self,
        run_id: &RunId,
        worker_epoch: WorkerEpoch,
        owner_fence: OwnerFence,
        messages: &[ChatMessage],
        max_output_tokens: u32,
        temperature_milli: u32,
        top_p_milli: u32,
        seed: u64,
    ) -> Result<PreparedWorkerRun, InferenceError> {
        let req = pb::PrepareRequest {
            run_id: run_id.to_string(),
            worker_epoch: worker_epoch.0,
            owner_fence: owner_fence.0,
            messages: messages
                .iter()
                .map(|m| pb::ChatMessage {
                    role: m.role.clone(),
                    content: m.content.clone(),
                })
                .collect(),
            max_output_tokens,
            temperature_milli,
            top_p_milli,
            seed,
        };
        let resp = self.client().prepare(req).await?.into_inner();
        let prompt_sha256 = Bytes32::from_hex(&resp.prompt_sha256)
            .map_err(|e| InferenceError::InvalidWorkerData(e.to_string()))?;
        Ok(PreparedWorkerRun {
            prepared_input_id: PreparedInputId::new(resp.prepared_input_id),
            run_id: RunId::new(resp.run_id),
            input_token_count: resp.input_token_count,
            input_token_ids: resp.input_token_ids,
            prompt_sha256,
            tested_context_limit: resp.tested_context_limit,
        })
    }

    pub async fn generate(
        &self,
        run_id: &RunId,
        prepared_input_id: &PreparedInputId,
        initial_allowance: &InternalWorkAllowance,
    ) -> Result<tonic::Streaming<pb::GenerateEvent>, InferenceError> {
        let req = pb::GenerateRequest {
            run_id: run_id.to_string(),
            prepared_input_id: prepared_input_id.to_string(),
            initial_allowance: Some(to_pb_allowance(initial_allowance)),
        };
        Ok(self.client().generate(req).await?.into_inner())
    }

    pub async fn update_allowance(
        &self,
        allowance: &InternalWorkAllowance,
    ) -> Result<pb::UpdateAllowanceResponse, InferenceError> {
        let req = pb::UpdateAllowanceRequest {
            allowance: Some(to_pb_allowance(allowance)),
        };
        Ok(self.client().update_allowance(req).await?.into_inner())
    }

    pub async fn cancel(
        &self,
        run_id: &RunId,
        worker_epoch: WorkerEpoch,
        owner_fence: OwnerFence,
        admission_epoch: AdmissionEpoch,
        reason: &str,
    ) -> Result<pb::CancelResponse, InferenceError> {
        let req = pb::CancelRequest {
            run_id: run_id.to_string(),
            worker_epoch: worker_epoch.0,
            owner_fence: owner_fence.0,
            admission_epoch: admission_epoch.0,
            reason: reason.to_owned(),
        };
        Ok(self.client().cancel(req).await?.into_inner())
    }

    pub async fn drain(
        &self,
        worker_epoch: WorkerEpoch,
        owner_fence: OwnerFence,
        reason: &str,
    ) -> Result<pb::DrainResponse, InferenceError> {
        let req = pb::DrainRequest {
            worker_epoch: worker_epoch.0,
            owner_fence: owner_fence.0,
            reason: reason.to_owned(),
        };
        Ok(self.client().drain(req).await?.into_inner())
    }
}

pub fn to_pb_allowance(a: &InternalWorkAllowance) -> pb::InternalWorkAllowance {
    pb::InternalWorkAllowance {
        run_id: a.run_id.to_string(),
        worker_epoch: a.worker_epoch.0,
        owner_fence: a.owner_fence.0,
        admission_epoch: a.admission_epoch.0,
        allowance_seq: a.allowance_seq.0,
        max_output_seq_exclusive: a.max_output_seq_exclusive,
        expires_at_monotonic_ms: a.expires_at_monotonic_ms.0,
        ledger_version: a.ledger_version.0,
    }
}

pub fn from_pb_allowance(p: &pb::InternalWorkAllowance) -> InternalWorkAllowance {
    InternalWorkAllowance {
        run_id: RunId::new(p.run_id.clone()),
        worker_epoch: WorkerEpoch::new(p.worker_epoch),
        owner_fence: OwnerFence::new(p.owner_fence),
        admission_epoch: AdmissionEpoch::new(p.admission_epoch),
        allowance_seq: AllowanceSeq::new(p.allowance_seq),
        max_output_seq_exclusive: p.max_output_seq_exclusive,
        expires_at_monotonic_ms: MonotonicTimestampMs::new(p.expires_at_monotonic_ms),
        ledger_version: LedgerVersion::new(p.ledger_version),
    }
}
