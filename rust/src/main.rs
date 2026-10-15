//! pump-detector: live last and mark prices of every USDT perpetual on six
//! futures exchanges, candles, pump/dump alerts, trailing take profit for
//! the Trigger worker, and read-only /v1 endpoints for the Trigger apps.
//! See README.md.

mod alerts;
mod app;
mod backfill;
mod candles;
mod config;
mod feeds;
mod gaps;
mod http;
mod hub;
mod marks;
mod model;
mod store;
mod token;
mod trailing;
mod v1;

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::feeds::venues::Venue;
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
    // `pump-detector mint-token <user> [seconds]`: a browser token for local
    // tests, signed with PRICES_TOKEN_SECRET.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("mint-token") {
        let Some(secret) = &cfg.prices_token_secret else {
            eprintln!("PRICES_TOKEN_SECRET is not set");
            std::process::exit(2);
        };
        let secs: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(900);
        let claims = token::Claims {
            sub: args.get(2).cloned().unwrap_or_else(|| "local".into()),
            exp: chrono::Utc::now().timestamp() + secs,
            scope: token::SCOPE.into(),
        };
        println!("{}", token::sign(secret, &claims));
        return;
    }
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

    let registry = feeds::markets::Registry::default();
    let mut feed_list: Vec<app::Feed> = Vec::new();

    let binance = Alive::default();
    let binance_marks = Alive::default();
    tokio::spawn(feeds::binance::run(price_tx.clone(), binance.clone()));
    tokio::spawn(feeds::binance::run_marks(
        feeds::binance::NAME,
        feeds::binance::MARK_URL,
        price_tx.clone(),
        binance_marks.clone(),
    ));
    tokio::spawn(refresh_markets(feeds::binance::NAME, registry.clone()));
    feed_list.push(app::Feed {
        name: feeds::binance::NAME,
        connections: vec![binance, binance_marks],
    });

    let gate = Alive::default();
    tokio::spawn(feeds::gate::run(
        price_tx.clone(),
        gate.clone(),
        registry.clone(),
    ));
    feed_list.push(app::Feed {
        name: feeds::gate::NAME,
        connections: vec![gate],
    });

    for v in Venue::ALL {
        let mut connections: Vec<Alive> = (0..v.shards()).map(|_| Alive::default()).collect();
        tokio::spawn(feeds::venues::run(
            v,
            price_tx.clone(),
            connections.clone(),
            registry.clone(),
            venue_rx[&v].clone(),
        ));
        if v == Venue::Aster {
            let marks = Alive::default();
            tokio::spawn(feeds::binance::run_marks(
                v.name(),
                feeds::binance::ASTER_MARK_URL,
                price_tx.clone(),
                marks.clone(),
            ));
            connections.push(marks);
        }
        feed_list.push(app::Feed {
            name: v.name(),
            connections,
        });
    }
    drop(price_tx);

    let port = cfg.port;
    let (app, ch) = app::App::new(cfg, hub, store.clone(), feed_list, registry);
    app.reload_configs().await;
    tokio::spawn(store.clone().candle_writer(ch.candles));
    tokio::spawn(store.trim_loop(app.cfg.candle_retention));
    tokio::spawn(app.clone().alert_sender(ch.alerts));
    tokio::spawn(app.clone().closer(ch.closes));
    tokio::spawn(app.clone().report());
    tokio::spawn(app.clone().measure());
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

/// Keeps Binance's market list fresh (other venues refresh their own).
async fn refresh_markets(exchange: &'static str, registry: feeds::markets::Registry) {
    let http = reqwest::Client::new();
    let mut every = tokio::time::interval(feeds::markets::REFRESH);
    loop {
        every.tick().await;
        match feeds::markets::fetch(&http, exchange).await {
            Ok(m) => registry.set(exchange, m),
            Err(e) => {
                tracing::warn!("{exchange}: market list failed: {e}");
                if registry.count(exchange) == 0 {
                    every.reset_after(std::time::Duration::from_secs(30));
                }
            }
        }
    }
}
