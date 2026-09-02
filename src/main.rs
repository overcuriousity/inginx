use anyhow::Result;
use inginx::config;

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

    let db = inginx::db::Db::open(&cfg.db_path)?;
    let log = inginx::logstore::spawn_writer(db.clone());
    inginx::logstore::spawn_pruner(db.clone());

    let state = inginx::proxy::AppState {
        db,
        log,
        http: reqwest::Client::new(),
    };
    let app = inginx::proxy::router(state);
    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
