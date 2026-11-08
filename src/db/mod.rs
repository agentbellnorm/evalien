//! Record stores under the trajectory: SQLite in production, memory in tests.

mod memory;
mod sqlite;

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

pub use memory::MemoryStore;
pub use sqlite::SqliteStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
}

/// Open the database file. Readers and the appending runtime can work at once.
pub fn open(path: &Path, access: Access) -> rusqlite::Result<Connection> {
    let flags = match access {
        Access::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        Access::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
    };
    let connection = Connection::open_with_flags(path, flags)?;
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    if access == Access::ReadWrite {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    Ok(connection)
}
