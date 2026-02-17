use duckdb::Connection;

const CURRENT_VERSION: u32 = 1;

/// Initialize the schema version tracking table and run any pending migrations.
pub fn run_migrations(conn: &Connection) -> Result<(), duckdb::Error> {
    // Create version tracking table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL, applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
    )?;

    let current = get_current_version(conn)?;

    if current < 1 {
        migrate_v1(conn)?;
    }

    Ok(())
}

fn get_current_version(conn: &Connection) -> Result<u32, duckdb::Error> {
    let mut stmt = conn.prepare("SELECT COALESCE(MAX(version), 0) FROM schema_version")?;
    stmt.query_row([], |row| row.get(0))
}

fn migrate_v1(conn: &Connection) -> Result<(), duckdb::Error> {
    // V1: Initial schema — events table
    crate::storage::schema::init_schema(conn)?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?)",
        [CURRENT_VERSION],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_migrations_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let version = get_current_version(&conn).unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }

    #[test]
    fn test_run_migrations_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let version = get_current_version(&conn).unwrap();
        assert_eq!(version, CURRENT_VERSION);
    }

    #[test]
    fn test_events_table_exists_after_migration() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        let mut stmt = conn.prepare("SELECT COUNT(*) FROM events").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }
}
