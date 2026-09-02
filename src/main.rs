use anyhow::Result;
use inginx::{admin, config, db, logstore, proxy};

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

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

    let db = db::Db::open(&cfg.db_path)?;
    let log = logstore::spawn_writer(db.clone());
    logstore::spawn_pruner(db.clone());

    let state = proxy::AppState {
        db: db.clone(),
        log,
        http: reqwest::Client::new(),
    };

    if let Some(token) = admin::auth::ensure_setup_token(&db).await? {
        let host = cfg.admin_listen.unwrap_or(cfg.listen);
        println!("==> No admin account yet. Create one at: http://{host}/admin/setup?t={token}");
    }

    let public = proxy::router(state.clone());
    let admin_app = admin::router(state);

    tracing::info!(listen = %cfg.listen, db = %cfg.db_path.display(), "inginx starting");

    match cfg.admin_listen {
        Some(admin_addr) => {
            let public_listener = tokio::net::TcpListener::bind(cfg.listen).await?;
            let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
            tracing::info!(admin = %admin_addr, "admin GUI on separate listener");
            let public_srv =
                axum::serve(public_listener, public).with_graceful_shutdown(shutdown_signal());
            let admin_srv =
                axum::serve(admin_listener, admin_app).with_graceful_shutdown(shutdown_signal());
            let (a, b) = tokio::join!(public_srv, admin_srv);
            a?;
            b?;
        }
        None => {
            let app = public.merge(admin_app);
            let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }
    Ok(())
}
