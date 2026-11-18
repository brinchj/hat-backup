//! Shared SQLite connection setup for hash/blob index and per-family key index.

use diesel::connection::SimpleConnection;
use diesel::result::Error as DieselResultError;
use diesel::sqlite::SqliteConnection;

/// Apply durable journaling defaults after `establish`, before migrations.
///
/// WAL is the recommended mode for local Hat state: writers append, readers
/// do not block on the same lock window as DELETE journaling, and a crash
/// replays the `-wal` into the main file on next open. In-memory DBs ignore
/// the request and stay on the memory journal.
///
/// Defensive mode blocks `writable_schema`, `journal_mode=OFF`, shadow-table
/// writes, and related foot-guns. Hygiene for local state files, not a trust
/// boundary against a hostile DB image.
pub(crate) fn configure_connection(conn: &mut SqliteConnection) -> Result<(), DieselResultError> {
    conn.set_defensive(true)?;
    conn.batch_execute("PRAGMA journal_mode = WAL;")?;
    Ok(())
}
