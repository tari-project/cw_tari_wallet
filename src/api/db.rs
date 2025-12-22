use anyhow::{anyhow, Context, Result};
use flutter_rust_bridge::frb;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use std::sync::RwLock;

use minotari_wallet::init_db;

#[frb(ignore)]
struct DatabaseState {
    pub pool: Pool<SqliteConnectionManager>,
    pub path: String,
}

static DB_STATE: RwLock<Option<DatabaseState>> = RwLock::new(None);

#[frb]
pub fn initialize_database(path: String) -> Result<()> {
    println!("initializing database {}", path);

    let pool = init_db(&path).context("Failed to create database pool")?;

    let mut guard = DB_STATE
        .write()
        .map_err(|_| anyhow!("Failed to lock DB_STATE for writing"))?;

    *guard = Some(DatabaseState { pool, path });

    Ok(())
}

#[frb]
pub fn disconnect_database() -> Result<()> {
    let mut guard = DB_STATE
        .write()
        .map_err(|_| anyhow!("Failed to lock DB_STATE for writing"))?;

    *guard = None;

    Ok(())
}

pub(crate) fn get_db_connection() -> Result<PooledConnection<SqliteConnectionManager>> {
    let guard = DB_STATE
        .read()
        .map_err(|_| anyhow!("Failed to lock DB_STATE for reading"))?;

    let state = guard.as_ref().context("Database is not initialized")?;

    state
        .pool
        .get()
        .context("Failed to retrieve connection from pool")
}

pub(crate) fn get_db_path() -> Result<String> {
    let guard = DB_STATE
        .read()
        .map_err(|_| anyhow!("Failed to lock DB_STATE for reading"))?;

    let state = guard.as_ref().context("Database is not initialized")?;

    Ok(state.path.clone())
}
