use duckdb::Connection;

/// Latest schema version this build knows how to produce.
///
/// Bump this and add a matching `migrate_vN` when the schema changes.
pub const CURRENT_VERSION: u32 = 1;

/// Initialize schema-version tracking and run any pending migrations.
///
/// # Errors
///
/// Returns an error if a migration statement fails.
pub fn run_migrations(conn: &Connection) -> Result<(), duckdb::Error> {
    // `applied_at` has no DEFAULT: `CURRENT_TIMESTAMP` is a `TIMESTAMP WITH
    // TIME ZONE` whose implicit cast into a naive `TIMESTAMP` follows the
    // session time zone, which follows the host's locale. Every other timestamp
    // in this database is naive UTC, so the value is bound explicitly instead.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (\
             version INTEGER NOT NULL, \
             applied_at TIMESTAMP)",
    )?;

    let current = get_current_version(conn)?;

    // A database written by a newer build may contain columns, tables or
    // semantics this one does not know about. Silently opening it and running
    // the old code against it is how a rollback turns into data loss, so refuse
    // instead and say what the operator is looking at.
    if current > CURRENT_VERSION {
        return Err(duckdb::Error::InvalidParameterName(format!(
            "the database is at schema version {current} but this build understands \
             at most {CURRENT_VERSION}. It was written by a newer version of \
             Mallard Metrics; upgrade the binary, or restore a backup taken \
             before the upgrade."
        )));
    }

    if current < 1 {
        migrate_v1(conn)?;
    }

    Ok(())
}

/// Highest applied schema version, or 0 for a fresh database.
///
/// # Errors
///
/// Returns an error if the version table cannot be read.
pub fn get_current_version(conn: &Connection) -> Result<u32, duckdb::Error> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
}

/// Record that a migration has been applied.
fn record_version(conn: &Connection, version: u32) -> Result<(), duckdb::Error> {
    conn.execute(
        "INSERT INTO schema_version (version, applied_at) VALUES (?, ?)",
        duckdb::params![version, chrono::Utc::now().naive_utc()],
    )?;
    Ok(())
}

/// V1: initial schema — the `events` table.
fn migrate_v1(conn: &Connection) -> Result<(), duckdb::Error> {
    crate::storage::schema::init_schema(conn)?;
    // Records 1, not CURRENT_VERSION: writing CURRENT_VERSION here would make
    // a future v2 migration claim v1 had already produced the v2 schema, and
    // v2 would then be skipped forever on databases created by this build.
    record_version(conn, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_migrations_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        assert_eq!(get_current_version(&conn).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn test_run_migrations_refuses_a_newer_database() {
        // Opening a database written by a newer build with older code is how a
        // rollback silently corrupts data; it must be an error, not a warning.
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        record_version(&conn, CURRENT_VERSION + 1).unwrap();

        let err = run_migrations(&conn).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("newer version"),
            "the error should name the cause: {message}"
        );
    }

    #[test]
    fn test_run_migrations_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();
        assert_eq!(get_current_version(&conn).unwrap(), CURRENT_VERSION);

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1, "a re-run must not record the migration twice");
    }

    #[test]
    fn test_events_table_exists_after_migration() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_applied_at_is_recorded_in_utc() {
        // The old DEFAULT CURRENT_TIMESTAMP recorded host-local time into a
        // naive column, disagreeing with every event timestamp in the database.
        let conn = Connection::open_in_memory().unwrap();
        let before = chrono::Utc::now().naive_utc();
        run_migrations(&conn).unwrap();
        let after = chrono::Utc::now().naive_utc();

        let applied: chrono::NaiveDateTime = conn
            .query_row("SELECT applied_at FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            applied >= before && applied <= after,
            "applied_at {applied} is outside [{before}, {after}] — not UTC"
        );
    }

    #[test]
    fn test_v1_records_version_one_not_current_version() {
        // Regression: migrate_v1 used to insert CURRENT_VERSION, so once a v2
        // migration was added every v1 database would claim to already be at v2
        // and would skip it permanently.
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let recorded: u32 = conn
            .query_row(
                "SELECT version FROM schema_version ORDER BY version LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1, "the v1 migration must record version 1");
    }
}
