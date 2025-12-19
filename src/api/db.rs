use flutter_rust_bridge::for_generated::anyhow;
use minotari_wallet::init_db;

use once_cell::sync::OnceCell;
use flutter_rust_bridge::frb;

pub static DB_PATH: OnceCell<String> = OnceCell::new();

#[frb]
pub async fn initialize_database(path: String) -> anyhow::Result<()> {
    println!("initializing database {}", path);
    init_db(&path).await?;

    DB_PATH.set(path).expect("db already initialized");

    Ok(())
}
