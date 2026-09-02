mod config;
mod db;
mod keys;
mod metering;
mod registry;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::var("INGINX_CONFIG").unwrap_or_else(|_| "inginx.toml".into());
    let cfg = config::Config::load(std::path::Path::new(&config_path))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| cfg.log_level.clone().into()),
        )
        .init();

    tracing::info!(listen = %cfg.listen, db = %cfg.db_path.display(), "inginx starting");
    Ok(())
}
