//! pump-detector: live prices from six futures exchanges, candles and
//! pump/dump alerts for Binance and Gate, and trailing take profit for the
//! Trigger worker. See README.md.

mod alerts;
mod app;
mod candles;
mod config;
mod feeds;
mod http;
mod hub;
mod model;
mod store;
mod trailing;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::feeds::on_demand::Venue;
use crate::feeds::Alive;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .or_else(|_| {
                    tracing_subscriber::EnvFilter::try_new(
                        std::env::var("LOG_LEVEL")
                            .unwrap_or_else(|_| "info".into())
                            .to_lowercase(),
                    )
                })
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = config::Config::from_env();
    tracing::info!(
        "Starting Crypto Pump Detector {}",
        env!("CARGO_PKG_VERSION")
    );
    if cfg.trailing_close_url.is_none() {
        tracing::warn!("TRAILING_CLOSE_URL is not set: trailing closes are only logged (dry run)");
    }
    if cfg.webhook_url.is_none() {
        tracing::warn!("WEBHOOK_URL is not set: alerts are stored only");
    }

    let store = match store::Store::connect(&cfg.mongodb_url, &cfg.mongodb_db_name).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to connect to MongoDB: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("Successfully connected to MongoDB");

    let (hub, venue_rx) = hub::Hub::new();
    let (price_tx, price_rx) = mpsc::channel(4096);

    let mut feeds: Vec<(&'static str, Alive)> = Vec::new();
    let binance = Alive::default();
    feeds.push((feeds::binance::NAME, binance.clone()));
    tokio::spawn(feeds::binance::run(price_tx.clone(), binance));
    let gate = Alive::default();
    feeds.push((feeds::gate::NAME, gate.clone()));
    tokio::spawn(feeds::gate::run(price_tx.clone(), gate));
    for v in Venue::ALL {
        let alive = Alive::default();
        feeds.push((v.name(), alive.clone()));
        tokio::spawn(feeds::on_demand::run(
            v,
            price_tx.clone(),
            alive,
            venue_rx[&v].clone(),
        ));
    }
    drop(price_tx);

    let port = cfg.port;
    let (app, ch) = app::App::new(cfg, hub, store.clone(), feeds);
    app.reload_configs().await;
    tokio::spawn(store.clone().candle_writer(ch.candles));
    tokio::spawn(store.trim_loop());
    tokio::spawn(app.clone().alert_sender(ch.alerts));
    tokio::spawn(app.clone().closer(ch.closes));
    tokio::spawn(app.clone().report());
    tokio::spawn(app.clone().expire_trailing());
    tokio::spawn(Arc::clone(&app).process(price_rx));

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Cannot listen on port {port}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("Application started successfully on port {port}");
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        tracing::info!("Shutting down");
    };
    if let Err(e) = axum::serve(listener, http::router(app))
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!("server error: {e}");
    }
}
