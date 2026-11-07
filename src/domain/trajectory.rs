use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use super::codec;
use super::event::{Entry, Event, Seq, Timestamp};

/// A stored record. The store assigns sequence and time; contents are opaque.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredRecord {
    pub seq: Seq,
    pub timestamp: Timestamp,
    pub data: String,
}

#[derive(Debug, thiserror::Error)]
#[error("record store: {0}")]
pub struct StoreError(pub String);

/// Durable, append-only records in sequence order.
pub trait RecordStore: Send {
    fn append(&mut self, data: &str) -> Result<StoredRecord, StoreError>;
    fn get(&self, seq: Seq) -> Result<Option<StoredRecord>, StoreError>;
    /// Records in (after, through], in sequence order.
    fn read(&self, after: Seq, through: Seq) -> Result<Vec<StoredRecord>, StoreError>;
    fn last_seq(&self) -> Result<Seq, StoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TrajectoryError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("trajectory entry {seq} is corrupt: {reason}")]
    Corrupt { seq: Seq, reason: String },
    #[error("result references a missing evaluation: {0}")]
    DanglingResult(Seq),
}

/// The trajectory: an append-only log of validated events over a record
/// store. Every producer appends here, and consumers follow it with their own
/// cursors through [`Trajectory::wait_after`].
pub struct Trajectory {
    store: Mutex<Box<dyn RecordStore>>,
    latest: watch::Sender<Seq>,
}

impl Trajectory {
    pub fn new(store: impl RecordStore + 'static) -> Result<Self, TrajectoryError> {
        let latest = store.last_seq()?;
        Ok(Trajectory {
            store: Mutex::new(Box::new(store)),
            latest: watch::Sender::new(latest),
        })
    }

    /// Record an event. A result must reference an existing evaluation.
    pub fn append(&self, event: Event) -> Result<Entry, TrajectoryError> {
        let data = codec::encode(&event);
        let mut store = self.store();
        if let Event::Result { eval, .. } = &event {
            let evaluation = store.get(*eval)?.map(|record| codec::decode(&record.data));
            if !matches!(evaluation, Some(Ok(Event::Eval { .. }))) {
                return Err(TrajectoryError::DanglingResult(*eval));
            }
        }
        let record = store.append(&data)?;
        drop(store);
        self.latest.send_replace(record.seq);
        Ok(Entry {
            seq: record.seq,
            timestamp: record.timestamp,
            event,
        })
    }

    /// Entries in (after, through], in order.
    pub fn read(&self, after: Seq, through: Seq) -> Result<Vec<Entry>, TrajectoryError> {
        let records = self.store().read(after, through)?;
        records.into_iter().map(decode).collect()
    }

    pub fn read_all(&self) -> Result<Vec<Entry>, TrajectoryError> {
        self.read(Seq::ZERO, self.last_seq())
    }

    pub fn last_seq(&self) -> Seq {
        *self.latest.borrow()
    }

    /// The latest sequence number beyond `seq`, waiting for an append if needed.
    pub async fn wait_after(&self, seq: Seq) -> Seq {
        let mut latest = self.latest.subscribe();
        match latest.wait_for(|latest| *latest > seq).await {
            Ok(latest) => *latest,
            // Only possible if the sender is gone, and we own it.
            Err(_) => self.last_seq(),
        }
    }

    fn store(&self) -> MutexGuard<'_, Box<dyn RecordStore>> {
        // Each store operation is a single statement, so a panic elsewhere
        // can't leave the store half-updated.
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn decode(record: StoredRecord) -> Result<Entry, TrajectoryError> {
    let event = codec::decode(&record.data).map_err(|reason| TrajectoryError::Corrupt {
        seq: record.seq,
        reason,
    })?;
    Ok(Entry {
        seq: record.seq,
        timestamp: record.timestamp,
        event,
    })
}
