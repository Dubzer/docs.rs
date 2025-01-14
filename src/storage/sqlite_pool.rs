use anyhow::Result;
use moka::sync::Cache;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use std::{
    num::NonZeroU64,
    path::{Path, PathBuf},
    time::Duration,
};

static MAX_IDLE_TIME: Duration = Duration::from_secs(10 * 60);
static MAX_LIFE_TIME: Duration = Duration::from_secs(60 * 60);

/// SQLite connection pool.
///
/// Typical connection pools handle many connections to a single database,
/// while this one handles some connections to many databases.
///
/// The more connections we keep alive, the more open files we have,
/// so you might need to tweak this limit based on the max open files
/// on your system.
///
/// We open the databases in readonly mode.
/// We are using an additional connection pool per database to parallel requests
/// can be efficiently answered. Because of this the actual max connection count
/// might be higher than the given max_connections.
///
/// We keep at minimum of one connection per database, for one hour.  
/// Any additional connections will be dropped after 10 minutes of inactivity.
///
/// * `max_databases` is the maximum amout of databases in the pool.
/// * for each of the databases, we manage a pool of 1-10 connections
#[derive(Clone)]
pub(crate) struct SqliteConnectionPool {
    pools: Cache<PathBuf, r2d2::Pool<SqliteConnectionManager>>,
}

impl Default for SqliteConnectionPool {
    fn default() -> Self {
        Self::new(NonZeroU64::new(10).unwrap())
    }
}

impl SqliteConnectionPool {
    pub(crate) fn new(max_databases: NonZeroU64) -> Self {
        Self {
            pools: Cache::builder()
                .max_capacity(max_databases.get())
                .time_to_idle(MAX_LIFE_TIME)
                .build(),
        }
    }

    pub(crate) fn with_connection<R, P: AsRef<Path>, F: Fn(&Connection) -> Result<R>>(
        &self,
        path: P,
        f: F,
    ) -> Result<R> {
        let path = path.as_ref().to_owned();

        let pool = self
            .pools
            .entry(path.clone())
            .or_insert_with(|| {
                let manager = SqliteConnectionManager::file(path)
                    .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX);
                r2d2::Pool::builder()
                    .min_idle(Some(1))
                    .max_lifetime(Some(MAX_LIFE_TIME))
                    .idle_timeout(Some(MAX_IDLE_TIME))
                    .max_size(10)
                    .build_unchecked(manager)
            })
            .into_value();

        let conn = pool.get()?;
        f(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_connection() {
        let filename = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        rusqlite::Connection::open(&filename).unwrap();

        let pool = SqliteConnectionPool::new(NonZeroU64::new(1).unwrap());

        pool.with_connection(&filename, |conn| {
            conn.query_row("SELECT 1", [], |row| {
                assert_eq!(row.get::<_, i32>(0).unwrap(), 1);
                Ok(())
            })
            .unwrap();
            Ok(())
        })
        .unwrap();
    }
}
