use trader_api::{config::Config, routes, spawn_background, state};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info,sqlx=warn".into()))
        .json()
        .init();
    let cfg = Config::from_env()?;
    tracing::info!(config = ?cfg, "starting trader-api");
    if cfg.mode == trader_domain::Network::Mainnet && !cfg.mainnet_trading_enabled {
        tracing::warn!("MAINNET mode with trading DISABLED (read-only). Set MAINNET_TRADING_ENABLED only after the go-live checklist.");
    }
    let bind = cfg.bind.clone();
    let built = state::build(cfg).await?;
    let st = spawn_background(built).await;
    if let (Some(tg), Some(url), Some(secret)) = (&st.tg, &st.cfg.public_url, &st.cfg.webhook_secret) {
        if std::env::var("TELEGRAM_SET_WEBHOOK").as_deref() == Ok("true") {
            match tg.set_webhook(&format!("{}/telegram/webhook", url.trim_end_matches('/')), secret).await {
                Ok(()) => tracing::info!("telegram webhook registered"),
                Err(e) => tracing::error!(error = %e, "setWebhook failed"),
            }
        }
    }
    let app = routes::router(st);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal: draining (in-flight orders are reconciled on next start)");
    })
    .await?;
    Ok(())
}
