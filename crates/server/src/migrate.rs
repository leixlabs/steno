//! Schema versioning: the DDL lives in `migrations/*.sql`, never in this crate.
//!
//! The files are the format `sqlx migrate` reads — `<VERSION>_<DESCRIPTION>.sql`,
//! applied in ascending version order, each one recorded in the
//! `_sqlx_migrations` table with a SHA-384 checksum of its bytes. This module is
//! the applier, so the record it writes is the same one the CLI reads back:
//! `sqlx migrate info --source migrations --database-url sqlite:…` against a
//! live database reports exactly what this process did.
//!
//! Why not [`sqlx::migrate`] itself: this crate talks to SQLite through
//! `rusqlite`, and `sqlx`'s SQLite driver pins `libsqlite3-sys` to a range that
//! excludes the version `rusqlite` 0.40 needs. Both crates declare
//! `links = "sqlite3"`, so cargo refuses to build them together — there is no
//! configuration in which `sqlx`'s migrator could run on our connection. Keeping
//! `rusqlite` (and the synchronous `DbWriter` thread built on it) and writing the
//! bookkeeping in sqlx's own format buys the thing that actually matters: the
//! SQL is versioned and reviewable as files, and the CLI stays a truthful window
//! into a deployed database.
//!
//! Rules that hold for every file in `migrations/`:
//!
//! - **Forward-only, one transaction each.** A released file is never edited; the
//!   checksum is what catches an edit to an already-applied migration, and a
//!   mismatch is a startup error rather than a silent re-run.
//! - **No `PRAGMA user_version` inside the file.** The applier sets it after the
//!   bookkeeping row, so the file also runs correctly when given to the CLI.
//! - **A database ahead of the binary is refused, never downgraded.** An unknown
//!   version in `_sqlx_migrations`, or a `user_version` past the newest file,
//!   stops startup with the offending version named.

use rusqlite::{params, Connection};
use steno_lib::error::{Error, Result};
use std::time::Instant;

/// Where the SQL files live, embedded at build time so a released binary does
/// not depend on the layout of the machine it runs on.
static MIGRATIONS_DIR: include_dir::Dir<'static> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../migrations");

/// sqlx's own table name. Not configurable on purpose: the CLI has to find it.
const BOOKKEEPING_TABLE: &str = "_sqlx_migrations";

/// One migration file, already validated.
#[derive(Debug, Clone)]
pub struct Migration {
    pub version: i64,
    /// File name as it sits in `migrations/`, for error messages.
    pub name: &'static str,
    /// The version's text with underscores turned into spaces, so it reads the
    /// way `sqlx migrate info` prints it.
    pub description: String,
    pub sql: &'static str,
}

impl Migration {
    /// SHA-384 over the file's bytes — the same digest sqlx stores, which is what
    /// makes the CLI agree with us about what has been applied.
    fn checksum(&self) -> [u8; 48] {
        use sha2::{Digest, Sha384};
        let mut hasher = Sha384::new();
        hasher.update(self.sql.as_bytes());
        hasher.finalize().into()
    }
}

/// Every migration file, in apply order.
///
/// A file that does not parse stops startup instead of being skipped: a
/// misnamed migration is a migration that would silently never run.
pub fn migration_files() -> Result<Vec<Migration>> {
    let mut migrations = Vec::new();
    for file in MIGRATIONS_DIR.files() {
        let name = file
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| Error::Storage(format!("migration path is not UTF-8: {:?}", file.path())))?;
        let Some(stem) = name.strip_suffix(".sql") else {
            // sqlx ignores non-SQL files in the directory; so do we.
            continue;
        };
        let Some((version, description)) = stem.split_once('_') else {
            return Err(Error::Storage(format!(
                "migration {name} is not named <VERSION>_<DESCRIPTION>.sql"
            )));
        };
        let version: i64 = version.parse().map_err(|_| {
            Error::Storage(format!("migration {name} has a version that is not an integer"))
        })?;
        if version <= 0 {
            return Err(Error::Storage(format!(
                "migration {name} has version {version}; versions start at 1"
            )));
        }
        let sql = file.contents_utf8().ok_or_else(|| {
            Error::Storage(format!("migration {name} is not valid UTF-8"))
        })?;
        migrations.push(Migration {
            version,
            name,
            description: description.replace('_', " "),
            sql,
        });
    }
    migrations.sort_by_key(|m| m.version);
    for pair in migrations.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(Error::Storage(format!(
                "two migration files share version {}",
                pair[0].version
            )));
        }
    }
    Ok(migrations)
}

/// Brings `conn` up to the newest embedded migration. Idempotent.
pub fn apply(conn: &Connection) -> Result<()> {
    let migrations = migration_files()?;
    let newest = migrations.last().map(|m| m.version).unwrap_or(0);

    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {BOOKKEEPING_TABLE} (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        );"
    ))?;

    // A database this binary does not understand is a refusal, not a guess.
    let mut user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if user_version > newest {
        return Err(Error::Storage(format!(
            "database schema version {user_version} is newer than supported {newest}"
        )));
    }

    let mut applied = applied_versions(conn)?;

    // A database written before this module existed carries its version in
    // `user_version` and has no bookkeeping rows. Everything up to that version
    // is by definition already in the file, so record it rather than re-running
    // DDL that would fail on an existing table. The checksums are the files we
    // ship, which is exactly what the rows would have held had they been written
    // at the time.
    if applied.is_empty() && user_version > 0 {
        let tx = conn.unchecked_transaction()?;
        for migration in migrations.iter().filter(|m| m.version <= user_version) {
            record(&tx, migration, 0)?;
        }
        tx.commit()?;
        applied = applied_versions(conn)?;
    }

    // Verify before touching anything: an edited migration or a row from a newer
    // binary has to fail here, with the version named.
    for (version, checksum, success) in &applied {
        if !success {
            return Err(Error::Storage(format!(
                "migration {version} was left half-applied (success = false); the database needs a look before it can start"
            )));
        }
        let Some(migration) = migrations.iter().find(|m| m.version == *version) else {
            return Err(Error::Storage(format!(
                "database has migration {version}, which this binary does not ship (newest is {}); it was written by a newer build",
                migrations.last().map(|m| m.version).unwrap_or(0)
            )));
        };
        if migration.checksum().as_slice() != checksum.as_slice() {
            return Err(Error::Storage(format!(
                "migration {} ({}) changed after it was applied; migrations are immutable once released",
                version, migration.name
            )));
        }
    }

    for migration in migrations.iter() {
        if applied.iter().any(|(version, _, _)| version == &migration.version) {
            continue;
        }
        // DDL and its bookkeeping row share one transaction, so a crash can never
        // leave the file ahead of the record or the record ahead of the file.
        let started = Instant::now();
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(migration.sql)?;
        let elapsed = started.elapsed();
        record(
            &tx,
            migration,
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX),
        )?;
        // Kept truthful for the operators and tools that still read it (the Pi
        // deploy checklist greps it); the bookkeeping table is what this code
        // trusts.
        tx.execute_batch(&format!("PRAGMA user_version = {}", migration.version))?;
        tx.commit()?;
        user_version = migration.version;
        tracing::info!(
            version = migration.version,
            elapsed_ms = elapsed.as_millis() as u64,
            "schema migration applied"
        );
    }

    // A database migrated by `sqlx migrate run` alone never had its
    // `user_version` advanced; leave it consistent with the table.
    let table_newest = applied_versions(conn)?.last().map(|(v, _, _)| *v).unwrap_or(0);
    if table_newest > user_version {
        conn.execute_batch(&format!("PRAGMA user_version = {table_newest}"))?;
    }
    Ok(())
}

/// `(version, checksum, success)` for every recorded migration, in order.
fn applied_versions(conn: &Connection) -> Result<Vec<(i64, Vec<u8>, bool)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT version, checksum, success FROM {BOOKKEEPING_TABLE} ORDER BY version"
    ))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn record(conn: &Connection, migration: &Migration, execution_time: i64) -> Result<()> {
    let checksum = migration.checksum();
    conn.execute(
        &format!(
            "INSERT INTO {BOOKKEEPING_TABLE} (version, description, success, checksum, execution_time)
             VALUES (?1, ?2, 1, ?3, ?4)"
        ),
        params![
            migration.version,
            migration.description,
            checksum.as_slice(),
            execution_time
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap()
    }

    fn recorded(conn: &Connection) -> Vec<i64> {
        applied_versions(conn).unwrap().into_iter().map(|(v, _, _)| v).collect()
    }

    #[test]
    fn files_are_versioned_and_named_the_sqlx_way() {
        let migrations = migration_files().unwrap();
        // One file on purpose: the five-table history was collapsed, so a fresh
        // install creates the three tables once and never drops anything.
        assert_eq!(migrations.len(), 1);
        assert_eq!(migrations[0].version, 1);
        // The description sqlx would print, not the file's underscores.
        assert_eq!(migrations[0].description, "init");
    }

    #[test]
    fn applies_every_file_once_and_is_idempotent() {
        let conn = fresh();
        apply(&conn).unwrap();
        let newest = migration_files().unwrap().last().unwrap().version;
        assert_eq!(recorded(&conn), (1..=newest).collect::<Vec<_>>());
        assert_eq!(user_version(&conn), newest);

        // Second startup: no DDL runs again, no row is duplicated.
        apply(&conn).unwrap();
        assert_eq!(recorded(&conn), (1..=newest).collect::<Vec<_>>());
    }

    #[test]
    fn an_unknown_recorded_version_refuses_to_start() {
        let conn = fresh();
        apply(&conn).unwrap();
        conn.execute(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES (9999, 'from the future', 1, x'00', 0)",
            [],
        )
        .unwrap();
        let error = apply(&conn).unwrap_err().to_string();
        assert!(error.contains("9999"), "{error}");
    }

    #[test]
    fn an_edited_migration_is_refused_instead_of_re_run() {
        let conn = fresh();
        apply(&conn).unwrap();
        conn.execute("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 1", [])
            .unwrap();
        let error = apply(&conn).unwrap_err().to_string();
        assert!(error.contains("changed after it was applied"), "{error}");
    }

    #[test]
    fn a_user_version_ahead_of_the_files_refuses_to_start() {
        let conn = fresh();
        apply(&conn).unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        let error = apply(&conn).unwrap_err().to_string();
        assert!(error.contains("newer than supported"), "{error}");
    }
}
