use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::domain::{RecordStore, Seq, StoreError, StoredRecord, Timestamp};

/// Append-only records in one SQLite table. Contents are opaque strings.
pub struct SqliteStore {
    connection: Connection,
    table: String,
}

impl SqliteStore {
    pub fn new(connection: Connection, table: &str) -> Result<Self, StoreError> {
        let valid = table
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(StoreError(format!("invalid table name: {table}")));
        }
        let store = SqliteStore {
            connection,
            table: table.to_owned(),
        };
        // A read-only connection can't create tables, and doesn't need to.
        if !store.connection.is_readonly(rusqlite::MAIN_DB).map_err(failed)? {
            store
                .connection
                .execute_batch(&format!(
                    "CREATE TABLE IF NOT EXISTS {table} (
                        seq INTEGER PRIMARY KEY AUTOINCREMENT,
                        timestamp TEXT NOT NULL,
                        data TEXT NOT NULL
                    )"
                ))
                .map_err(failed)?;
        }
        Ok(store)
    }
}

impl RecordStore for SqliteStore {
    fn append(&mut self, data: &str) -> Result<StoredRecord, StoreError> {
        let timestamp = Timestamp::now();
        let sql = format!("INSERT INTO {} (timestamp, data) VALUES (?1, ?2)", self.table);
        self.connection
            .prepare_cached(&sql)
            .and_then(|mut s| s.execute(params![timestamp.to_string(), data]))
            .map_err(failed)?;
        let seq = to_seq(self.connection.last_insert_rowid())?;
        Ok(StoredRecord {
            seq,
            timestamp,
            data: data.to_owned(),
        })
    }

    fn get(&self, seq: Seq) -> Result<Option<StoredRecord>, StoreError> {
        let sql = format!("SELECT seq, timestamp, data FROM {} WHERE seq = ?1", self.table);
        let mut statement = self.connection.prepare_cached(&sql).map_err(failed)?;
        let row = statement.query_row([to_row_id(seq)?], raw).optional().map_err(failed)?;
        row.map(record).transpose()
    }

    fn read(&self, after: Seq, through: Seq) -> Result<Vec<StoredRecord>, StoreError> {
        let sql = format!(
            "SELECT seq, timestamp, data FROM {} WHERE seq > ?1 AND seq <= ?2 ORDER BY seq",
            self.table
        );
        let mut statement = self.connection.prepare_cached(&sql).map_err(failed)?;
        let rows = statement
            .query_map([to_row_id(after)?, to_row_id(through)?], raw)
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map_err(failed)?;
        rows.into_iter().map(record).collect()
    }

    fn last_seq(&self) -> Result<Seq, StoreError> {
        let sql = format!("SELECT COALESCE(MAX(seq), 0) FROM {}", self.table);
        let seq: i64 = self.connection.query_row(&sql, [], |row| row.get(0)).map_err(failed)?;
        to_seq(seq)
    }
}

/// A row as stored, before its timestamp is parsed.
fn raw(row: &Row<'_>) -> rusqlite::Result<(i64, String, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}

fn record((seq, timestamp, data): (i64, String, String)) -> Result<StoredRecord, StoreError> {
    let timestamp = timestamp
        .parse()
        .map_err(|e| StoreError(format!("entry {seq} has a bad timestamp: {e}")))?;
    Ok(StoredRecord {
        seq: to_seq(seq)?,
        timestamp,
        data,
    })
}

/// SQLite row IDs are signed; sequence numbers never are.
fn to_seq(row_id: i64) -> Result<Seq, StoreError> {
    u64::try_from(row_id)
        .map(Seq::new)
        .map_err(|_| StoreError(format!("negative sequence number {row_id}")))
}

fn to_row_id(seq: Seq) -> Result<i64, StoreError> {
    i64::try_from(seq.get()).map_err(|_| StoreError(format!("sequence number {seq} is out of range")))
}

fn failed(error: rusqlite::Error) -> StoreError {
    StoreError(error.to_string())
}
