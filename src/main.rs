//! ostara binary: wires the axum router, shared HTTP client, and TTL
//! cache together and serves the API.

use std::{net::SocketAddr, time::Duration};

use ostara::{cache::TtlCache, config::Config, routes};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // One pooled client for all upstreams: connection reuse across feeds and
    // requests, with hard timeouts so a hung upstream can never stall us.
    let client = reqwest::Client::builder()
        .user_agent("ostara/0.1 (OSINT API core)")
        .timeout(Duration::from_secs(12))
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(16)
        .build()?;

    // Every API key and knob comes from the environment (see .env.example).
    // Keyed feeds degrade gracefully when their key is unset.
    let config = Arc::new(Config::from_env());

    let state = routes::AppState {
        client,
        cache: Arc::new(TtlCache::new()),
        config: config.clone(),
    };

    let app = routes::app(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("ostara listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
