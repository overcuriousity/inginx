use anyhow::Result;
use inginx::{admin, config, db, logstore, proxy, ratelimit};

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

    // A connect timeout so a dead or black-holed backend cannot pin a request
    // task waiting for a TCP handshake that never completes. No total timeout:
    // a streamed completion is meant to stay open for minutes.
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?;

    let state = proxy::AppState {
        db: db.clone(),
        log,
        http,
        login_throttle: std::sync::Arc::new(ratelimit::LoginThrottle::new(
            cfg.login_max_failures,
            std::time::Duration::from_secs(cfg.login_lockout_secs),
        )),
        max_body_bytes: cfg.max_body_bytes,
        trust_proxy_headers: cfg.trust_proxy_headers,
        // 0 means "no limit" in the config; the proxy speaks `Option`.
        upstream_timeout: (cfg.upstream_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(cfg.upstream_timeout_secs)),
        upstream_stream_idle: (cfg.upstream_stream_idle_secs > 0)
            .then(|| std::time::Duration::from_secs(cfg.upstream_stream_idle_secs)),
        upstream_retry_on_5xx: cfg.upstream_retry_on_5xx,
        api_throttle: std::sync::Arc::new(ratelimit::LoginThrottle::new(
            cfg.api_auth_max_failures,
            std::time::Duration::from_secs(cfg.api_auth_lockout_secs),
        )),
    };

    if let Some(token) = admin::auth::ensure_setup_token(&db).await? {
        let host = cfg.admin_listen.unwrap_or(cfg.listen);
        println!("==> No admin account yet. Create one at: http://{host}/admin/setup?t={token}");
    }

    // The admin GUI is the keys to the whole proxy. When it shares the public
    // listener and that listener faces a network, it is reachable by anyone
    // who can reach the API — so say so, loudly, once. The fix is either
    // `admin_listen = "127.0.0.1:<port>"` or a reverse proxy that fronts it.
    // `trust_proxy_headers` is the operator asserting that proxy exists, so the
    // shared listener is then deliberate and the warning would be noise.
    if cfg.admin_listen.is_none() && !cfg.listen.ip().is_loopback() && !cfg.trust_proxy_headers {
        tracing::warn!(
            listen = %cfg.listen,
            "admin GUI is served on the public listener and bound to a non-loopback \
             address — it is reachable from the network. Set `admin_listen` to a \
             loopback address or front it with a reverse proxy."
        );
    }

    let public = proxy::router(state.clone());
    let admin_app = admin::router(state);

    tracing::info!(listen = %cfg.listen, db = %cfg.db_path.display(), "inginx starting");

    match cfg.admin_listen {
        Some(admin_addr) => {
            let public_listener = tokio::net::TcpListener::bind(cfg.listen).await?;
            let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
            tracing::info!(admin = %admin_addr, "admin GUI on separate listener");
            // `into_make_service_with_connect_info` is what puts the peer
            // address in each request's extensions, which is how the login
            // throttle knows who to lock out.
            let public_srv = axum::serve(
                public_listener,
                public.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal());
            let admin_srv = axum::serve(
                admin_listener,
                admin_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal());
            let (a, b) = tokio::join!(public_srv, admin_srv);
            a?;
            b?;
        }
        None => {
            let app = public.merge(admin_app);
            let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown_signal())
            .await?;
        }
    }
    Ok(())
}
