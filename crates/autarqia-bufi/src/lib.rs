use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use autarqia_primitives::OutboxEntryId;
use autarqia_subsystem_api::BufiOutboxEntry;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum BufiError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http sink error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[async_trait]
pub trait BufiSink: Send + Sync {
    async fn publish_entry(&self, entry: &BufiOutboxEntry) -> Result<(), BufiError>;
}

#[derive(Clone)]
pub struct JsonlFileBufiSink {
    path: PathBuf,
    delivered: Arc<Mutex<Vec<OutboxEntryId>>>,
}

impl JsonlFileBufiSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            delivered: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn delivered_ids(&self) -> Vec<OutboxEntryId> {
        self.delivered.lock().await.clone()
    }
}

#[async_trait]
impl BufiSink for JsonlFileBufiSink {
    async fn publish_entry(&self, entry: &BufiOutboxEntry) -> Result<(), BufiError> {
        let mut guard = self.delivered.lock().await;
        if guard.contains(&entry.entry_id) {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        let line = serde_json::to_string(entry)? + "\n";
        f.write_all(line.as_bytes()).await?;
        f.flush().await?;
        guard.push(entry.entry_id.clone());
        Ok(())
    }
}
