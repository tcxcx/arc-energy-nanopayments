use std::convert::Infallible;
use std::sync::Arc;

use async_trait::async_trait;
use autarqia_primitives::{ChatMessage, EnergyTelemetrySnapshot, OrderId, RunId, SessionId};
use autarqia_subsystem_api::{
    CreateQuoteRequest, CreateQuoteResponse, CreateSessionRequest, CreateSessionResponse,
    NodeHealthSnapshot, ProviderDiscoveryDocument, RunControlCommand, RunControlResponse,
    RunReceipt, RunStreamFrame, StartRunRequest, StartRunResponse, SubsystemError,
};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;

#[async_trait]
pub trait NodeRpcFacade: Send + Sync + 'static {
    async fn discovery_document(&self) -> ProviderDiscoveryDocument;
    async fn create_quote(
        &self,
        req: CreateQuoteRequest,
    ) -> Result<CreateQuoteResponse, SubsystemError>;
    async fn create_session(
        &self,
        req: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, SubsystemError>;
    async fn start_run(
        &self,
        bearer_token: Option<&str>,
        req: StartRunRequest,
    ) -> Result<(StartRunResponse, broadcast::Receiver<RunStreamFrame>), SubsystemError>;
    async fn subscribe_run_events(
        &self,
        run_id: &RunId,
    ) -> Result<(Vec<RunStreamFrame>, broadcast::Receiver<RunStreamFrame>), SubsystemError>;
    async fn control_run(
        &self,
        run_id: &RunId,
        cmd: RunControlCommand,
    ) -> Result<RunControlResponse, SubsystemError>;
    async fn get_receipt(&self, run_id: &RunId) -> Result<RunReceipt, SubsystemError>;
    async fn inject_energy_telemetry(
        &self,
        snapshot: EnergyTelemetrySnapshot,
    ) -> Result<NodeHealthSnapshot, SubsystemError>;
    async fn health_snapshot(&self) -> NodeHealthSnapshot;
}

pub fn build_router(facade: Arc<dyn NodeRpcFacade>) -> Router {
    Router::new()
        .route("/.well-known/autarqia/provider.json", get(get_provider_doc))
        .route("/v1/models", get(get_models))
        .route("/v1/mppi/quotes", post(post_quote))
        .route("/v1/mppi/sessions", post(post_session))
        .route("/v1/mppi/runs", post(post_run))
        .route("/v1/mppi/runs/{id}/events", get(get_run_events))
        .route("/v1/mppi/runs/{id}/control", post(post_run_control))
        .route("/v1/mppi/runs/{id}/receipt", get(get_run_receipt))
        .route("/v1/chat/completions", post(post_chat_completions))
        .route("/v1/energy/telemetry", post(post_energy_telemetry))
        .route("/healthz", get(get_healthz))
        .route("/readyz", get(get_readyz))
        .route("/metrics", get(get_metrics))
        .layer(CorsLayer::permissive())
        .with_state(facade)
}

fn map_subsystem_err(err: SubsystemError) -> Response {
    let status = match &err {
        SubsystemError::NotFound(_) => StatusCode::NOT_FOUND,
        SubsystemError::Conflict(_) => StatusCode::CONFLICT,
        SubsystemError::EnergyRejected(_) => StatusCode::SERVICE_UNAVAILABLE,
        SubsystemError::Payment(_) => StatusCode::PAYMENT_REQUIRED,
        SubsystemError::RuntimeRejected(msg) if msg.contains("budget") || msg.contains("voucher") => {
            StatusCode::PAYMENT_REQUIRED
        }
        SubsystemError::RuntimeRejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
        SubsystemError::Store(_) | SubsystemError::Inference(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    (
        status,
        Json(json!({
            "error": err.to_string(),
        })),
    )
        .into_response()
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(ToOwned::to_owned)
}

async fn get_provider_doc(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
) -> Json<ProviderDiscoveryDocument> {
    Json(facade.discovery_document().await)
}

async fn get_models(State(facade): State<Arc<dyn NodeRpcFacade>>) -> Json<serde_json::Value> {
    let doc = facade.discovery_document().await;
    Json(json!({
        "object": "list",
        "data": [
            {
                "id": doc.model_manifest.model_id,
                "object": "model",
                "owned_by": "autarqia-airllm",
                "revision": doc.model_manifest.model_revision,
                "manifest_commitment": doc.model_manifest.manifest_commitment(),
                "runtime_device": doc.model_manifest.runtime_device,
                "runtime_dtype": doc.model_manifest.runtime_dtype,
                "tested_context_limit": doc.model_manifest.tested_context_limit,
            }
        ]
    }))
}

async fn post_quote(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    Json(req): Json<CreateQuoteRequest>,
) -> Response {
    match facade.create_quote(req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => map_subsystem_err(e),
    }
}

async fn post_session(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    match facade.create_session(req).await {
        Ok(resp) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(e) => map_subsystem_err(e),
    }
}

async fn post_run(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    headers: HeaderMap,
    Json(req): Json<StartRunRequest>,
) -> Response {
    let bearer = extract_bearer(&headers);
    match facade.start_run(bearer.as_deref(), req).await {
        Ok((resp, _rx)) => (StatusCode::CREATED, Json(resp)).into_response(),
        Err(e) => map_subsystem_err(e),
    }
}

async fn get_run_events(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    Path(id): Path<String>,
) -> Response {
    let run_id = RunId::new(id);
    let (history, rx) = match facade.subscribe_run_events(&run_id).await {
        Ok(pair) => pair,
        Err(e) => return map_subsystem_err(e),
    };

    let (tx_out, rx_out) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);
    tokio::spawn(async move {
        let mut seen_terminal = false;
        let mut max_token_seq_sent = 0u32;
        for frame in history {
            if let RunStreamFrame::TokenDelta { output_seq, .. } = &frame {
                max_token_seq_sent = max_token_seq_sent.max(*output_seq);
            }
            let is_terminal = matches!(&frame, RunStreamFrame::ReceiptReady { .. });
            if let Ok(json_str) = serde_json::to_string(&frame) {
                if tx_out
                    .send(Ok(Event::default().data(json_str)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if is_terminal {
                seen_terminal = true;
                break;
            }
        }
        if seen_terminal {
            return;
        }
        let mut bcast_stream = tokio_stream::wrappers::BroadcastStream::new(rx);
        while let Some(Ok(frame)) = bcast_stream.next().await {
            if matches!(&frame, RunStreamFrame::RunStarted { .. }) {
                continue;
            }
            if let RunStreamFrame::TokenDelta { output_seq, .. } = &frame {
                if *output_seq <= max_token_seq_sent {
                    continue;
                }
                max_token_seq_sent = *output_seq;
            }
            let is_terminal = matches!(&frame, RunStreamFrame::ReceiptReady { .. });
            if let Ok(json_str) = serde_json::to_string(&frame) {
                if tx_out
                    .send(Ok(Event::default().data(json_str)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            if is_terminal {
                break;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx_out);
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn post_run_control(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    Path(id): Path<String>,
    Json(cmd): Json<RunControlCommand>,
) -> Response {
    let run_id = RunId::new(id);
    match facade.control_run(&run_id, cmd).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => map_subsystem_err(e),
    }
}

async fn get_run_receipt(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    Path(id): Path<String>,
) -> Response {
    let run_id = RunId::new(id);
    match facade.get_receipt(&run_id).await {
        Ok(rcpt) => (StatusCode::OK, Json(rcpt)).into_response(),
        Err(e) => map_subsystem_err(e),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpenAiChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stream: Option<bool>,
}

async fn post_chat_completions(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    headers: HeaderMap,
    Json(req): Json<OpenAiChatRequest>,
) -> Response {
    let Some(session_hdr) = headers
        .get("x-autarqia-session-id")
        .and_then(|v| v.to_str().ok())
    else {
        let doc = facade.discovery_document().await;
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": "payment_required",
                "profile_id": doc.profile_id,
                "provider_discovery": doc,
            })),
        )
            .into_response();
    };

    let order_hdr = headers
        .get("x-autarqia-order-id")
        .and_then(|v| v.to_str().ok())
        .map(OrderId::new)
        .unwrap_or_else(|| OrderId::new(format!("ord_{}", uuid::Uuid::new_v4().simple())));
    let bearer = extract_bearer(&headers);

    let start_req = StartRunRequest {
        session_id: SessionId::new(session_hdr),
        order_id: order_hdr,
        model_id: req.model.clone(),
        messages: req.messages,
        max_output_tokens: req.max_tokens.unwrap_or(16),
        temperature_milli: req.temperature.map(|t| (t * 1000.0) as u32).unwrap_or(0),
        top_p_milli: 1000,
        seed: 42,
        initial_voucher: None,
        x402_authorization: None,
    };

    let (started, mut rx) = match facade.start_run(bearer.as_deref(), start_req).await {
        Ok(res) => res,
        Err(e) => return map_subsystem_err(e),
    };

    let mut content = String::new();
    let mut final_receipt = None;
    while let Ok(frame) = rx.recv().await {
        match frame {
            RunStreamFrame::TokenDelta { text_delta, .. } => {
                content.push_str(&text_delta);
            }
            RunStreamFrame::ReceiptReady { receipt } => {
                final_receipt = Some(receipt);
                break;
            }
            _ => {}
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "id": started.run_id,
            "object": "chat.completion",
            "model": req.model,
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                    },
                    "finish_reason": "stop",
                }
            ],
            "autarqia_receipt": final_receipt,
        })),
    )
        .into_response()
}

async fn post_energy_telemetry(
    State(facade): State<Arc<dyn NodeRpcFacade>>,
    Json(snapshot): Json<EnergyTelemetrySnapshot>,
) -> Response {
    match facade.inject_energy_telemetry(snapshot).await {
        Ok(health) => (StatusCode::OK, Json(health)).into_response(),
        Err(e) => map_subsystem_err(e),
    }
}

async fn get_healthz(State(facade): State<Arc<dyn NodeRpcFacade>>) -> Json<NodeHealthSnapshot> {
    Json(facade.health_snapshot().await)
}

async fn get_readyz(State(facade): State<Arc<dyn NodeRpcFacade>>) -> Response {
    let snap = facade.health_snapshot().await;
    let code = if snap.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(snap)).into_response()
}

async fn get_metrics(State(facade): State<Arc<dyn NodeRpcFacade>>) -> Response {
    let s = facade.health_snapshot().await;
    let body = format!(
        "autarqia_node_healthy {}\nautarqia_node_ready {}\nautarqia_worker_epoch {}\nautarqia_owner_fence {}\nautarqia_admission_epoch {}\nautarqia_ledger_version {}\nautarqia_active_runs {}\nautarqia_pending_bufi_outbox {}\n",
        u8::from(s.healthy),
        u8::from(s.ready),
        s.worker_epoch.0,
        s.owner_fence.0,
        s.admission_epoch.0,
        s.ledger_version.0,
        s.active_runs,
        s.pending_bufi_outbox,
    );
    (StatusCode::OK, body).into_response()
}
