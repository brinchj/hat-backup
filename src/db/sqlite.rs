//! Shared SQLite connection setup for hash/blob index and per-family key index.

use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselResultError;
use diesel::sqlite::SqliteConnection;
use std::io;

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
///
/// `PRAGMA quick_check` runs after WAL replay on open. It is a structural
/// gate (not Hat content FSCK); prefer it over full `integrity_check` at
/// every open for cost. Fail closed if SQLite reports anything but `ok`.
pub(crate) fn configure_connection(conn: &mut SqliteConnection) -> Result<(), DieselResultError> {
    conn.set_defensive(true)?;
    conn.batch_execute("PRAGMA journal_mode = WAL;")?;
    quick_check(conn)?;
    Ok(())
}

fn quick_check(conn: &mut SqliteConnection) -> Result<(), DieselResultError> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Text)]
        quick_check: String,
    }

    let rows: Vec<Row> = diesel::sql_query("PRAGMA quick_check").load(conn)?;
    match rows.as_slice() {
        [row] if row.quick_check == "ok" => Ok(()),
        other => {
            let detail = other
                .iter()
                .map(|r| r.quick_check.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            Err(DieselResultError::QueryBuilderError(Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SQLite PRAGMA quick_check failed: {detail}"),
            ))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_accepts_fresh_memory_db() {
        let mut conn = SqliteConnection::establish(":memory:").unwrap();
        configure_connection(&mut conn).unwrap();
        assert!(conn.is_defensive().unwrap());
    }
}
