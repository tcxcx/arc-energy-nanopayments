use std::path::Path;
use std::sync::Arc;

use autarqia_primitives::{
    ChannelId, LedgerVersion, OrderId, OutboxEntryId, OwnerFence, RunId, RunReceipt, WorkerEpoch,
};
use autarqia_subsystem_api::{BufiOutboxEntry, ChannelLedgerView, RunLedgerView};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, WriteOptions, DB};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

pub const CF_META: &str = "cf_meta";
pub const CF_WAL: &str = "cf_wal";
pub const CF_CHANNELS: &str = "cf_channels";
pub const CF_RUNS: &str = "cf_runs";
pub const CF_RECEIPTS: &str = "cf_receipts";
pub const CF_ORDERS: &str = "cf_orders";
pub const CF_BUFI_OUTBOX: &str = "cf_bufi_outbox";

const ALL_CFS: &[&str] = &[
    CF_META,
    CF_WAL,
    CF_CHANNELS,
    CF_RUNS,
    CF_RECEIPTS,
    CF_ORDERS,
    CF_BUFI_OUTBOX,
];

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("rocksdb error: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("missing column family: {0}")]
    MissingCf(&'static str),
    #[error("single-writer actor channel closed")]
    WriterClosed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomicCommitBatch {
    pub ledger_version: LedgerVersion,
    pub runtime_checkpoint_json: Option<String>,
    pub channels: Vec<ChannelLedgerView>,
    pub runs: Vec<RunLedgerView>,
    pub receipts: Vec<RunReceipt>,
    pub orders: Vec<(OrderId, RunId)>,
    pub outbox_entries: Vec<BufiOutboxEntry>,
    pub wal_note: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredStoreState {
    pub owner_fence: OwnerFence,
    pub worker_epoch: WorkerEpoch,
    pub ledger_version: LedgerVersion,
    pub runtime_checkpoint_json: Option<String>,
    pub channels: Vec<ChannelLedgerView>,
    pub runs: Vec<RunLedgerView>,
    pub receipts: Vec<RunReceipt>,
    pub pending_outbox: Vec<BufiOutboxEntry>,
}

pub trait DurableLedgerStore: Send + Sync {
    fn commit_batch_sync(&self, batch: AtomicCommitBatch) -> Result<u64, StoreError>;
    fn mark_outbox_delivered(&self, entry_id: &OutboxEntryId) -> Result<(), StoreError>;
    fn load_recovered_state(&self) -> Result<RecoveredStoreState, StoreError>;
    fn get_receipt(&self, run_id: &RunId) -> Result<Option<RunReceipt>, StoreError>;
    fn get_channel(&self, channel_id: &ChannelId) -> Result<Option<ChannelLedgerView>, StoreError>;
}

pub struct RocksDbLedgerStore {
    db: Arc<DB>,
}

impl RocksDbLedgerStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        let cf_descriptors: Vec<ColumnFamilyDescriptor> = ALL_CFS
            .iter()
            .map(|name| ColumnFamilyDescriptor::new(*name, Options::default()))
            .collect();

        let db = DB::open_cf_descriptors(&db_opts, path, cf_descriptors)?;
        Ok(Self { db: Arc::new(db) })
    }

    pub fn acquire_and_bump_fence(&self) -> Result<(OwnerFence, WorkerEpoch), StoreError> {
        let cf_meta = self.db.cf_handle(CF_META).ok_or(StoreError::MissingCf(CF_META))?;
        let prev_fence = self
            .db
            .get_cf(cf_meta, b"owner_fence")?
            .and_then(|b| b.try_into().ok().map(u64::from_be_bytes))
            .unwrap_or(0);
        let prev_epoch = self
            .db
            .get_cf(cf_meta, b"worker_epoch")?
            .and_then(|b| b.try_into().ok().map(u64::from_be_bytes))
            .unwrap_or(0);

        let next_fence = OwnerFence::new(prev_fence.saturating_add(1));
        let next_epoch = WorkerEpoch::new(prev_epoch.saturating_add(1));

        let mut wb = WriteBatch::default();
        wb.put_cf(cf_meta, b"owner_fence", next_fence.0.to_be_bytes());
        wb.put_cf(cf_meta, b"worker_epoch", next_epoch.0.to_be_bytes());

        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);
        self.db.write_opt(wb, &write_opts)?;
        Ok((next_fence, next_epoch))
    }
}

impl DurableLedgerStore for RocksDbLedgerStore {
    fn commit_batch_sync(&self, batch: AtomicCommitBatch) -> Result<u64, StoreError> {
        let cf_meta = self.db.cf_handle(CF_META).ok_or(StoreError::MissingCf(CF_META))?;
        let cf_wal = self.db.cf_handle(CF_WAL).ok_or(StoreError::MissingCf(CF_WAL))?;
        let cf_channels = self
            .db
            .cf_handle(CF_CHANNELS)
            .ok_or(StoreError::MissingCf(CF_CHANNELS))?;
        let cf_runs = self.db.cf_handle(CF_RUNS).ok_or(StoreError::MissingCf(CF_RUNS))?;
        let cf_receipts = self
            .db
            .cf_handle(CF_RECEIPTS)
            .ok_or(StoreError::MissingCf(CF_RECEIPTS))?;
        let cf_orders = self
            .db
            .cf_handle(CF_ORDERS)
            .ok_or(StoreError::MissingCf(CF_ORDERS))?;
        let cf_outbox = self
            .db
            .cf_handle(CF_BUFI_OUTBOX)
            .ok_or(StoreError::MissingCf(CF_BUFI_OUTBOX))?;

        let prev_wal_seq = self
            .db
            .get_cf(cf_meta, b"wal_seq")?
            .and_then(|b| b.try_into().ok().map(u64::from_be_bytes))
            .unwrap_or(0);
        let wal_seq = prev_wal_seq.saturating_add(1);

        let mut wb = WriteBatch::default();
        wb.put_cf(cf_meta, b"wal_seq", wal_seq.to_be_bytes());
        wb.put_cf(
            cf_meta,
            b"ledger_version",
            batch.ledger_version.0.to_be_bytes(),
        );
        if let Some(cp_json) = &batch.runtime_checkpoint_json {
            wb.put_cf(cf_meta, b"runtime_checkpoint", cp_json.as_bytes());
        }

        let wal_payload = serde_json::to_vec(&batch)?;
        wb.put_cf(cf_wal, wal_seq.to_be_bytes(), &wal_payload);

        for ch in &batch.channels {
            wb.put_cf(cf_channels, ch.channel_id.0, serde_json::to_vec(ch)?);
        }
        for run in &batch.runs {
            wb.put_cf(
                cf_runs,
                run.run_id.as_str().as_bytes(),
                serde_json::to_vec(run)?,
            );
        }
        for rcpt in &batch.receipts {
            wb.put_cf(
                cf_receipts,
                rcpt.run_id.as_str().as_bytes(),
                serde_json::to_vec(rcpt)?,
            );
        }
        for (order_id, run_id) in &batch.orders {
            wb.put_cf(
                cf_orders,
                order_id.as_str().as_bytes(),
                run_id.as_str().as_bytes(),
            );
        }
        for entry in &batch.outbox_entries {
            wb.put_cf(
                cf_outbox,
                entry.entry_id.as_str().as_bytes(),
                serde_json::to_vec(entry)?,
            );
        }

        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);
        self.db.write_opt(wb, &write_opts)?;
        Ok(wal_seq)
    }

    fn mark_outbox_delivered(&self, entry_id: &OutboxEntryId) -> Result<(), StoreError> {
        let cf_outbox = self
            .db
            .cf_handle(CF_BUFI_OUTBOX)
            .ok_or(StoreError::MissingCf(CF_BUFI_OUTBOX))?;
        if let Some(raw) = self.db.get_cf(cf_outbox, entry_id.as_str().as_bytes())? {
            let mut entry: BufiOutboxEntry = serde_json::from_slice(&raw)?;
            entry.delivered = true;
            entry.delivery_attempts = entry.delivery_attempts.saturating_add(1);
            let mut write_opts = WriteOptions::default();
            write_opts.set_sync(true);
            self.db.put_cf_opt(
                cf_outbox,
                entry_id.as_str().as_bytes(),
                serde_json::to_vec(&entry)?,
                &write_opts,
            )?;
        }
        Ok(())
    }

    fn load_recovered_state(&self) -> Result<RecoveredStoreState, StoreError> {
        let cf_meta = self.db.cf_handle(CF_META).ok_or(StoreError::MissingCf(CF_META))?;
        let cf_channels = self
            .db
            .cf_handle(CF_CHANNELS)
            .ok_or(StoreError::MissingCf(CF_CHANNELS))?;
        let cf_runs = self.db.cf_handle(CF_RUNS).ok_or(StoreError::MissingCf(CF_RUNS))?;
        let cf_receipts = self
            .db
            .cf_handle(CF_RECEIPTS)
            .ok_or(StoreError::MissingCf(CF_RECEIPTS))?;
        let cf_outbox = self
            .db
            .cf_handle(CF_BUFI_OUTBOX)
            .ok_or(StoreError::MissingCf(CF_BUFI_OUTBOX))?;

        let owner_fence = OwnerFence::new(
            self.db
                .get_cf(cf_meta, b"owner_fence")?
                .and_then(|b| b.try_into().ok().map(u64::from_be_bytes))
                .unwrap_or(1),
        );
        let worker_epoch = WorkerEpoch::new(
            self.db
                .get_cf(cf_meta, b"worker_epoch")?
                .and_then(|b| b.try_into().ok().map(u64::from_be_bytes))
                .unwrap_or(1),
        );
        let ledger_version = LedgerVersion::new(
            self.db
                .get_cf(cf_meta, b"ledger_version")?
                .and_then(|b| b.try_into().ok().map(u64::from_be_bytes))
                .unwrap_or(1),
        );
        let runtime_checkpoint_json = self
            .db
            .get_cf(cf_meta, b"runtime_checkpoint")?
            .and_then(|b| String::from_utf8(b).ok());

        let mut channels = Vec::new();
        for item in self.db.iterator_cf(cf_channels, rocksdb::IteratorMode::Start) {
            let (_k, v) = item?;
            channels.push(serde_json::from_slice(&v)?);
        }

        let mut runs = Vec::new();
        for item in self.db.iterator_cf(cf_runs, rocksdb::IteratorMode::Start) {
            let (_k, v) = item?;
            runs.push(serde_json::from_slice(&v)?);
        }

        let mut receipts = Vec::new();
        for item in self.db.iterator_cf(cf_receipts, rocksdb::IteratorMode::Start) {
            let (_k, v) = item?;
            receipts.push(serde_json::from_slice(&v)?);
        }

        let mut pending_outbox = Vec::new();
        for item in self.db.iterator_cf(cf_outbox, rocksdb::IteratorMode::Start) {
            let (_k, v) = item?;
            let entry: BufiOutboxEntry = serde_json::from_slice(&v)?;
            if !entry.delivered {
                pending_outbox.push(entry);
            }
        }

        Ok(RecoveredStoreState {
            owner_fence,
            worker_epoch,
            ledger_version,
            runtime_checkpoint_json,
            channels,
            runs,
            receipts,
            pending_outbox,
        })
    }

    fn get_receipt(&self, run_id: &RunId) -> Result<Option<RunReceipt>, StoreError> {
        let cf_receipts = self
            .db
            .cf_handle(CF_RECEIPTS)
            .ok_or(StoreError::MissingCf(CF_RECEIPTS))?;
        match self.db.get_cf(cf_receipts, run_id.as_str().as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    fn get_channel(&self, channel_id: &ChannelId) -> Result<Option<ChannelLedgerView>, StoreError> {
        let cf_channels = self
            .db
            .cf_handle(CF_CHANNELS)
            .ok_or(StoreError::MissingCf(CF_CHANNELS))?;
        match self.db.get_cf(cf_channels, channel_id.0)? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }
}

enum WriterCommand {
    CommitBatch {
        batch: AtomicCommitBatch,
        reply: oneshot::Sender<Result<u64, StoreError>>,
    },
    MarkOutboxDelivered {
        entry_id: OutboxEntryId,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
}

struct SingleWriterShared {
    tx: Option<mpsc::Sender<WriterCommand>>,
    join_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for SingleWriterShared {
    fn drop(&mut self) {
        let _ = self.tx.take();
        if let Ok(mut guard) = self.join_handle.lock() {
            if let Some(jh) = guard.take() {
                let _ = jh.join();
            }
        }
    }
}

#[derive(Clone)]
pub struct SingleWriterStoreHandle {
    shared: Arc<SingleWriterShared>,
    store: Arc<RocksDbLedgerStore>,
}

impl SingleWriterStoreHandle {
    pub fn spawn(store: Arc<RocksDbLedgerStore>) -> Self {
        let (tx, mut rx) = mpsc::channel::<WriterCommand>(256);
        let writer_store = Arc::clone(&store);
        let jh = std::thread::Builder::new()
            .name("autarqia-store-single-writer".to_owned())
            .spawn(move || {
                while let Some(cmd) = rx.blocking_recv() {
                    match cmd {
                        WriterCommand::CommitBatch { batch, reply } => {
                            let _ = reply.send(writer_store.commit_batch_sync(batch));
                        }
                        WriterCommand::MarkOutboxDelivered { entry_id, reply } => {
                            let _ = reply.send(writer_store.mark_outbox_delivered(&entry_id));
                        }
                    }
                }
            })
            .expect("failed to spawn single-writer store thread");
        Self {
            shared: Arc::new(SingleWriterShared {
                tx: Some(tx),
                join_handle: std::sync::Mutex::new(Some(jh)),
            }),
            store,
        }
    }

    fn sender(&self) -> Result<&mpsc::Sender<WriterCommand>, StoreError> {
        self.shared.tx.as_ref().ok_or(StoreError::WriterClosed)
    }

    pub async fn commit_batch(&self, batch: AtomicCommitBatch) -> Result<u64, StoreError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender()?
            .send(WriterCommand::CommitBatch {
                batch,
                reply: reply_tx,
            })
            .await
            .map_err(|_| StoreError::WriterClosed)?;
        reply_rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    pub async fn mark_outbox_delivered(&self, entry_id: OutboxEntryId) -> Result<(), StoreError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender()?
            .send(WriterCommand::MarkOutboxDelivered {
                entry_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| StoreError::WriterClosed)?;
        reply_rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    pub fn load_recovered_state(&self) -> Result<RecoveredStoreState, StoreError> {
        self.store.load_recovered_state()
    }

    pub fn get_receipt(&self, run_id: &RunId) -> Result<Option<RunReceipt>, StoreError> {
        self.store.get_receipt(run_id)
    }

    pub fn get_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Option<ChannelLedgerView>, StoreError> {
        self.store.get_channel(channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autarqia_primitives::{Bytes32, EvmAddress, SettlementRail, TokenAtomicAmount};

    #[tokio::test]
    async fn single_writer_persists_and_recovers_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let ch_id = Bytes32::from_hex(
            "0x2b6548a57025a137badb75792fa955609e8b2ccbf70e384d5d9ae40ffc5cc2cc",
        )
        .unwrap();

        {
            let store = Arc::new(RocksDbLedgerStore::open(dir.path()).unwrap());
            let (fence1, epoch1) = store.acquire_and_bump_fence().unwrap();
            assert_eq!(fence1.0, 1);
            assert_eq!(epoch1.0, 1);

            let handle = SingleWriterStoreHandle::spawn(store);
            let seq = handle
                .commit_batch(AtomicCommitBatch {
                    ledger_version: LedgerVersion::new(5),
                    runtime_checkpoint_json: Some("{\"test\":true}".to_owned()),
                    channels: vec![ChannelLedgerView {
                        channel_id: ch_id,
                        rail: SettlementRail::AndoMppiArcV1,
                        payer: EvmAddress::ZERO,
                        payee: EvmAddress::ZERO,
                        authorized_signer: EvmAddress::ZERO,
                        deposit_atomic: TokenAtomicAmount::new(5_000),
                        highest_verified_voucher_atomic: TokenAtomicAmount::new(1_200),
                        highest_voucher_signature: None,
                        settled_onchain_atomic: TokenAtomicAmount::ZERO,
                        accrued_cumulatively_atomic: TokenAtomicAmount::new(900),
                        acknowledged_cumulatively_atomic: TokenAtomicAmount::new(900),
                        close_requested_at_unix_s: None,
                        closed: false,
                    }],
                    runs: vec![],
                    receipts: vec![],
                    orders: vec![(OrderId::new("ord_1"), RunId::new("run_1"))],
                    outbox_entries: vec![],
                    wal_note: "unit_test_commit".to_owned(),
                })
                .await
                .unwrap();
            assert_eq!(seq, 1);
        }

        let reopened = RocksDbLedgerStore::open(dir.path()).unwrap();
        let (fence2, epoch2) = reopened.acquire_and_bump_fence().unwrap();
        assert_eq!(fence2.0, 2);
        assert_eq!(epoch2.0, 2);
        let recovered = reopened.load_recovered_state().unwrap();
        assert_eq!(recovered.ledger_version.0, 5);
        assert_eq!(recovered.channels.len(), 1);
        assert_eq!(
            recovered.channels[0].highest_verified_voucher_atomic.0,
            1_200
        );
    }
}
