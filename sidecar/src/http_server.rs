use axum::extract::State;
use axum::{routing::get, Json, Router};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::{TcpStream, UnixStream};
use tokio::time::timeout;
use tower_http::trace::TraceLayer;

use crate::{config::Config, AppState};

#[derive(Clone)]
pub struct ServerContext {
    pub state: Arc<AppState>,
    pub config: Arc<Config>,
}

#[derive(Serialize)]
struct HealthStatus {
    status: String,
    last_activity_timestamp: i64,
    idle_seconds: u64,
}

async fn check_upstream(config: &Config) -> bool {
    if let Some(tcp_addr) = &config.target_tcp {
        matches!(
            timeout(Duration::from_millis(200), TcpStream::connect(tcp_addr)).await,
            Ok(Ok(_))
        )
    } else if let Some(uds_path) = &config.target_uds {
        matches!(
            timeout(Duration::from_millis(200), UnixStream::connect(uds_path)).await,
            Ok(Ok(_))
        )
    } else {
        false
    }
}

/// Runs the Axum HTTP server for health checks.
pub async fn run_http_server(
    state: Arc<AppState>,
    config: Arc<Config>,
) -> Result<(), std::io::Error> {
    let ctx = ServerContext {
        state,
        config: config.clone(),
    };

    let app = Router::new()
        .route("/health", get(health_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(ctx);

    let listener = tokio::net::TcpListener::bind(&config.http_listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Responds with the current activity status.
async fn health_handler(State(ctx): State<ServerContext>) -> Json<HealthStatus> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let is_ready = check_upstream(&ctx.config).await;
    let status = if is_ready { "ok" } else { "starting" };

    let last_activity = ctx.state.get_last_activity();
    let idle_seconds = (now - last_activity).max(0) as u64;

    Json(HealthStatus {
        status: status.to_string(),
        last_activity_timestamp: last_activity,
        idle_seconds,
    })
}
