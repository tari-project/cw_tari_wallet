use flutter_rust_bridge::for_generated::anyhow;
use minotari_wallet::init_db;

use flutter_rust_bridge::frb;
use once_cell::sync::OnceCell;

pub static DB_PATH: OnceCell<String> = OnceCell::new();

#[frb]
pub async fn initialize_database(path: String) -> anyhow::Result<()> {
    println!("initializing database {}", path);
    init_db(&path)?;

    DB_PATH.set(path).expect("db already initialized");

    Ok(())
}
