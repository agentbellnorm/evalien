use crate::domain::{RecordStore, Seq, StoreError, StoredRecord, Timestamp};

/// A record store that lives and dies with the process.
#[derive(Default)]
pub struct MemoryStore {
    records: Vec<StoredRecord>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RecordStore for MemoryStore {
    fn append(&mut self, data: &str) -> Result<StoredRecord, StoreError> {
        let seq = Seq::new(self.records.len() as u64 + 1);
        let record = StoredRecord {
            seq,
            timestamp: Timestamp::now(),
            data: data.to_owned(),
        };
        self.records.push(record.clone());
        Ok(record)
    }

    fn get(&self, seq: Seq) -> Result<Option<StoredRecord>, StoreError> {
        Ok(self.records.iter().find(|record| record.seq == seq).cloned())
    }

    fn read(&self, after: Seq, through: Seq) -> Result<Vec<StoredRecord>, StoreError> {
        Ok(self
            .records
            .iter()
            .filter(|r| r.seq > after && r.seq <= through)
            .cloned()
            .collect())
    }

    fn last_seq(&self) -> Result<Seq, StoreError> {
        Ok(self.records.last().map_or(Seq::ZERO, |record| record.seq))
    }
}
