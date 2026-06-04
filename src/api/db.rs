//! Database state management.
//!
//! The wallet's SQLite connection pool is process-global mutable state: it is
//! created by `initialize_database` and torn down by `disconnect_database`, and
//! every read path (balance, transactions, address, send) needs it in between.
//!
//! That global is encapsulated behind a single typed `Database` holder. The
//! `static` lives here and **nowhere else reaches into it directly** — all access
//! goes through one of the `pub(crate)` accessors below, each of which is a thin
//! wrapper over `Database::with_current`. This keeps the "is it initialized?"
//! check ([`WalletError::NotInitialized`]) in exactly one place and means a future
//! refactor (e.g. injecting the pool as a parameter, per the domain seam)
//! only has to touch this module.
//!
//! ## Concurrency / lifecycle assumptions
//! - Single process, single live wallet at a time. `initialize_database` /
//!   `disconnect_database` are expected to be called from the Dart side around a
//!   session; concurrent init/teardown is not a supported pattern.
//! - The lock is a synchronous [`RwLock`]; **no guard is ever held across an
//!   `.await`** here (every accessor is synchronous and returns owned/cloned data
//!   or a pooled connection before the caller does any async work).
//! - The underlying `r2d2` pool is itself `Send + Sync` and internally
//!   synchronized, so cloning it out from under a short read lock is safe.

use crate::api::error::WalletError;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use std::{path::PathBuf, sync::RwLock};

use minotari_wallet::init_db;

/// The typed, encapsulated database state: the connection pool plus the on-disk
/// path it was opened from. Owns the process-global singleton via [`DB_STATE`].
#[frb(ignore)]
pub(crate) struct Database {
    pool: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

/// The one and only process-global database slot. `None` until
/// `initialize_database` runs; back to `None` after `disconnect_database`. Kept
/// private to this module: every access flows through a [`Database`] method, so
/// this `static` has exactly one owner.
static DB_STATE: RwLock<Option<Database>> = RwLock::new(None);

impl Database {
    /// Open the pool at `path` and install it as the current database, replacing
    /// any previously installed one.
    fn init(path: PathBuf) -> Result<()> {
        let pool = init_db(path.clone()).context("Failed to create database pool")?;

        let mut guard = DB_STATE
            .write()
            .map_err(|_| WalletError::internal("Failed to lock DB_STATE for writing"))?;
        *guard = Some(Database { pool, path });
        Ok(())
    }

    /// Clear the current database, dropping the pool (closing its connections).
    fn disconnect() -> Result<()> {
        let mut guard = DB_STATE
            .write()
            .map_err(|_| WalletError::internal("Failed to lock DB_STATE for writing"))?;
        *guard = None;
        Ok(())
    }

    /// Run `f` against the currently-installed database under a short read lock,
    /// returning [`WalletError::NotInitialized`] if none is installed.
    ///
    /// `f` must not block on async work — the read guard is alive for its duration.
    /// All accessors below only clone cheap handles / pull a pooled connection.
    fn with_current<T>(f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let guard = DB_STATE
            .read()
            .map_err(|_| WalletError::internal("Failed to lock DB_STATE for reading"))?;
        let state = guard.as_ref().ok_or(WalletError::NotInitialized)?;
        f(state)
    }
}

/// Open (or create) the wallet SQLite database at `path` and install it as the
/// process-global connection pool.
///
/// Must be called once before any DB-backed function (balance, transactions,
/// address, send, scan). Replaces any previously installed database. `path` is
/// the on-disk SQLite file path. Synchronous; errors if the pool cannot be
/// created.
#[frb]
pub fn initialize_database(path: String) -> Result<()> {
    log::info!("Initializing database at {}", path);
    Database::init(PathBuf::from(path))
}

/// Tear down the current wallet database, closing its connection pool.
///
/// Cooperatively cancels any in-flight scan first (so the scan stops issuing DB
/// queries before the pool is dropped), then clears the global slot. Idempotent:
/// calling it when no database is installed is a no-op. Synchronous.
#[frb]
pub fn disconnect_database() -> Result<()> {
    // Graceful shutdown: cooperate with any in-flight scan before dropping the pool.
    // Cancelling the scan first stops it from issuing further DB queries, so we don't
    // yank the connection pool out from under a live scan mid-operation. The scan's
    // own task observes the cancellation and clears its state on exit. We do not
    // block-await it here (this is a synchronous bridge fn; blocking on the async scan
    // task could deadlock the FRB runtime) — cancellation is the cooperative signal.
    crate::api::scanner::cancel_active_scan()?;
    Database::disconnect()
}

pub(crate) fn get_db_connection() -> Result<PooledConnection<SqliteConnectionManager>> {
    Database::with_current(|db| {
        db.pool
            .get()
            .context("Failed to retrieve connection from pool")
    })
}

pub(crate) fn get_db_path() -> Result<PathBuf> {
    Database::with_current(|db| Ok(db.path.clone()))
}

pub(crate) fn get_db_pool() -> Result<Pool<SqliteConnectionManager>> {
    Database::with_current(|db| Ok(db.pool.clone()))
}

/// Test-only serialization lock for the **two** process-global slots
/// (`db::DB_STATE` and `scanner::SCAN_STATE`). Since `disconnect_database` now
/// (transitively) takes `SCAN_STATE` via `scanner::cancel_active_scan` (graceful
/// shutdown), the DB tests and the scanner-state lifecycle test mutate overlapping
/// globals and would race if run in parallel. Every test that touches either global
/// awaits this lock for its duration so they run mutually exclusively (mirrors Step
/// 08's "consolidate global-state tests so they don't race" discipline, but across
/// the two modules that now share state).
///
/// It is a `tokio::sync::Mutex` (not `std`) on purpose: its guard is held across the
/// `.await`s in the async scanner lifecycle test, and a `std` guard there would (a)
/// trip clippy's `await_holding_lock` and (b) be a genuine footgun if the test ever
/// moved to a multi-thread runtime. The DB tests are `#[tokio::test]` so they can
/// `.await` it too. Lives in non-test code gated by `#[cfg(test)]` so both modules'
/// `#[cfg(test)] mod tests` can reach it.
#[cfg(test)]
pub(crate) static GLOBAL_STATE_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    //! Lifecycle tests for the DB state holder. These exercise the
    //! "not initialized" guard without any real wallet DB — they only drive the
    //! accessors before/after `disconnect_database`, so they stay hermetic.
    //!
    //! NOTE: these mutate the single process-global slot, so they must not race a
    //! test that *initializes* a real DB. There is no such test in this module, and
    //! the `disconnect` calls leave the slot in a known (`None`) state. They also
    //! hold [`GLOBAL_STATE_TEST_LOCK`] because `disconnect_database` now transitively
    //! takes `scanner::SCAN_STATE`, which the scanner-state lifecycle test mutates —
    //! the shared lock serializes the two so they cannot race across modules.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[tokio::test]
    async fn accessors_report_not_initialized_before_init() {
        // Serialize against the scanner-state test: `disconnect_database` now also
        // takes `SCAN_STATE`. (`tokio::sync::Mutex` does not poison, so no recovery
        // dance is needed.)
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        // Ensure a clean slate regardless of any prior test's effect.
        disconnect_database().unwrap();

        let conn_err = get_db_connection().unwrap_err();
        assert_eq!(conn_err.to_string(), "Database is not initialized");

        let path_err = get_db_path().unwrap_err();
        assert_eq!(path_err.to_string(), "Database is not initialized");

        let pool_err = get_db_pool().unwrap_err();
        assert_eq!(pool_err.to_string(), "Database is not initialized");
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_and_leaves_uninitialized() {
        let _guard = GLOBAL_STATE_TEST_LOCK.lock().await;
        disconnect_database().unwrap();
        // A second disconnect must not error and must keep the slot empty.
        disconnect_database().unwrap();
        assert_eq!(
            get_db_path().unwrap_err().to_string(),
            "Database is not initialized"
        );
    }
}
