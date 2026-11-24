//! Shared SQLite connection setup for hash/blob index and per-family key index.

use crate::errors::DieselError;
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::result::Error as DieselResultError;
use diesel::sqlite::SqliteConnection;
use std::io;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::Path;

fn is_file_backed(path: &str) -> bool {
    !path.is_empty() && path != ":memory:" && !path.starts_with("file::memory:")
}

/// Ensure a file has no group/other permission bits on Unix (tightening to `0600`).
/// Missing files are ignored so optional WAL/SHM sidecars can be tightened safely.
#[cfg(unix)]
fn tighten_file_mode_0600(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(meta) => {
            if meta.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Pre-create a file-backed SQLite database with mode `0600` before SQLite opens it,
/// avoiding any world-readable window and causing SQLite to inherit `0600` for `-wal`/`-shm`.
#[cfg(unix)]
fn prepare_sqlite_files_0600(path: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let db_path = Path::new(path);
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(db_path)?;
    tighten_file_mode_0600(db_path)?;
    tighten_sqlite_sidecars_0600(path)
}

#[cfg(unix)]
fn tighten_sqlite_sidecars_0600(path: &str) -> io::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = format!("{path}{suffix}");
        tighten_file_mode_0600(Path::new(&sidecar))?;
    }
    Ok(())
}

/// Open a SQLite connection with restrictive (`0600`) file permissions on Unix and
/// apply Hat's defensive/WAL/`quick_check` settings.
pub(crate) fn open_connection(path: &str) -> Result<SqliteConnection, DieselError> {
    #[cfg(unix)]
    if is_file_backed(path) {
        prepare_sqlite_files_0600(path)?;
    }

    let mut conn = SqliteConnection::establish(path)?;
    configure_connection(&mut conn)?;

    #[cfg(unix)]
    if is_file_backed(path) {
        tighten_sqlite_sidecars_0600(path)?;
    }

    Ok(conn)
}

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
    conn.batch_execute("PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL;")?;
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
        let conn = open_connection(":memory:").unwrap();
        assert!(conn.is_defensive().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn open_connection_sets_and_tightens_0600_for_db_and_sidecars() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_index.sqlite3");
        let db_str = db_path.to_str().unwrap();

        // Fresh DB + WAL/SHM created via open_connection must be 0600.
        {
            let mut conn = open_connection(db_str).unwrap();
            conn.batch_execute("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();

            assert_eq!(
                fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            for suffix in ["-wal", "-shm"] {
                let p = dir.path().join(format!("test_index.sqlite3{suffix}"));
                if p.exists() {
                    assert_eq!(
                        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                        0o600,
                        "sidecar {suffix} must be 0600"
                    );
                }
            }
        }

        // Simulate legacy loose permissions on existing DB and sidecars; reopen must tighten them.
        fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).unwrap();
        let wal_path = dir.path().join("test_index.sqlite3-wal");
        if wal_path.exists() {
            fs::set_permissions(&wal_path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let mut conn = open_connection(db_str).unwrap();
        conn.batch_execute("INSERT INTO t VALUES (2);").unwrap();
        assert_eq!(
            fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for suffix in ["-wal", "-shm"] {
            let p = dir.path().join(format!("test_index.sqlite3{suffix}"));
            if p.exists() {
                assert_eq!(
                    fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "reopened sidecar {suffix} must be 0600"
                );
            }
        }
    }
}
