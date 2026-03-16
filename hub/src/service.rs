use askama::Template;
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Path, Query},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use pingora::server::Fds;
use reqwest::StatusCode;
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    signal,
    sync::{Mutex, watch::Receiver},
};
use tower_cookies::CookieManagerLayer;
use tracing::{debug, error, info, trace, warn};

use crate::{auth, config::Workshop, orchestrator};

// Template structures
#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    workshops: Vec<Workshop>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate;

#[derive(Template)]
#[template(path = "pending.html")]
struct PendingTemplate {
    workshop_name: String,
}

#[derive(Template)]
#[template(path = "at_capacity.html")]
struct AtCapacityTemplate {
    workshop_name: String,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate {
    workshop_name: String,
    error_message: String,
}

pub struct AxumService {
    pub router: Router,
    pub addr: SocketAddr,
}

#[derive(Debug, serde::Deserialize)]
struct ErrorParams {
    message: Option<String>,
}

impl AxumService {
    #[tracing::instrument(level = "info")]
    pub fn new() -> Self {
        info!("Initializing Axum service");

        let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
        debug!(?addr, "Service will bind to address");

        let router = Router::new()
            .route("/health", get(health_handler))
            .route("/index", get(index_handler))
            .route("/workshop-login", get(login_handler).post(auth::handle_login))
            .route("/workshop-pending/{name}", get(pending_handler))
            .route("/workshop-at-capacity/{name}", get(capacity_handler))
            .route("/workshop-error/{name}", get(error_handler))
            .layer(CookieManagerLayer::new());

        info!("Axum service router configured with routes");

        Self { router, addr }
    }
}

async fn health_handler() -> impl IntoResponse {
    StatusCode::OK
}

/// Workshop index page - shows available workshops
#[tracing::instrument(level = "debug")]
async fn index_handler() -> Result<Response, StatusCode> {
    info!("Loading workshop index page");

    let orchestrator = match orchestrator().await {
        orch => {
            debug!("Orchestrator instance retrieved");
            orch
        }
    };

    let workshops = orchestrator.config.workshops.clone();
    info!(
        workshop_count = workshops.len(),
        "Retrieved workshop configurations"
    );

    for workshop in &workshops {
        trace!(
            workshop_name = %workshop.name,
            workshop_image = %workshop.image,
            "Workshop available"
        );
    }

    let template = IndexTemplate { workshops };

    match template.render() {
        Ok(html) => {
            debug!("Successfully rendered index template");
            Ok(Html(html).into_response())
        }
        Err(e) => {
            error!(error = ?e, "Failed to render index template");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Add the handler function
/// Login page handler
#[tracing::instrument(level = "debug")]
async fn login_handler() -> Result<Response, StatusCode> {
    info!("Serving login page");
    let template = LoginTemplate;

    match template.render() {
        Ok(html) => {
            debug!("Successfully rendered login template");
            Ok(Html(html).into_response())
        }
        Err(e) => {
            error!(error = ?e, "Failed to render login template");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Workshop pending page - shown while pod is being created
#[tracing::instrument(level = "debug", fields(workshop = %workshop_name))]
async fn pending_handler(Path(workshop_name): Path<String>) -> Result<Response, StatusCode> {
    info!(workshop_name = %workshop_name, "Serving workshop pending page");

    let template = PendingTemplate {
        workshop_name: workshop_name.clone(),
    };

    match template.render() {
        Ok(html) => {
            debug!(workshop_name = %workshop_name, "Successfully rendered pending template");
            Ok(Html(html).into_response())
        }
        Err(e) => {
            error!(
                workshop_name = %workshop_name,
                error = ?e,
                "Failed to render pending template"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Workshop at capacity page - shown when pod limit reached
#[tracing::instrument(level = "debug", fields(workshop = %workshop_name))]
async fn capacity_handler(Path(workshop_name): Path<String>) -> Result<Response, StatusCode> {
    warn!(
        workshop_name = %workshop_name,
        "Workshop at capacity - serving capacity page"
    );

    let template = AtCapacityTemplate {
        workshop_name: workshop_name.clone(),
    };

    match template.render() {
        Ok(html) => {
            debug!(workshop_name = %workshop_name, "Successfully rendered capacity template");
            Ok(Html(html).into_response())
        }
        Err(e) => {
            error!(
                workshop_name = %workshop_name,
                error = ?e,
                "Failed to render capacity template"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Workshop error page - shown when pod creation fails
#[tracing::instrument(level = "debug", fields(workshop = %workshop_name))]
async fn error_handler(
    Path(workshop_name): Path<String>,
    Query(params): Query<ErrorParams>, // Extract query params
) -> Result<Response, StatusCode> {
    error!(
        workshop_name = %workshop_name,
        "Workshop error - serving error page"
    );

    // Use the message from query params or a default fallback
    let error_message = params.message.unwrap_or_else(|| 
        "An error occurred while setting up your workshop environment. Please contact a staff member for assistance.".to_string()
    );

    let template = ErrorTemplate {
        workshop_name: workshop_name.clone(),
        error_message, 
    };

    match template.render() {
        Ok(html) => {
            debug!(workshop_name = %workshop_name, "Successfully rendered error template");
            Ok(Html(html).into_response())
        }
        Err(e) => {
            error!(
                workshop_name = %workshop_name,
                error = ?e,
                "Failed to render error template"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Implement the Pingora Service trait
#[async_trait]
impl pingora::services::Service for AxumService {
    #[tracing::instrument(level = "info", skip(self, _fds, shutdown))]
    async fn start_service(
        &mut self,
        _fds: Option<Arc<Mutex<Fds>>>,
        mut shutdown: Receiver<bool>,
        _listeners_per_fd: usize,
    ) {
        info!(addr = %self.addr, "Starting Axum service");

        let listener = match tokio::net::TcpListener::bind(self.addr).await {
            Ok(listener) => {
                info!(addr = %self.addr, "Successfully bound Axum service listener");
                listener
            }
            Err(e) => {
                error!(
                    addr = %self.addr,
                    error = ?e,
                    "Failed to bind Axum service port"
                );
                panic!("Failed to bind Axum port: {}", e);
            }
        };

        let shutdown_fn = || async move {
            debug!("Setting up shutdown handlers");

            let ctrl_c = async {
                signal::ctrl_c()
                    .await
                    .expect("failed to install Ctrl+C handler");
                info!("Received Ctrl+C signal");
            };

            #[cfg(unix)]
            let terminate = async {
                signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("failed to install signal handler")
                    .recv()
                    .await;
                info!("Received SIGTERM signal");
            };

            let pingora_signal = async {
                shutdown.changed().await.ok();
                info!("Received Pingora shutdown signal");
            };

            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();

            tokio::select! {
                _ = ctrl_c => {
                    info!("Shutting down Axum service via Ctrl+C");
                },
                _ = terminate => {
                    info!("Shutting down Axum service via SIGTERM");
                },
                _ = pingora_signal => {
                    info!("Shutting down Axum service via Pingora");
                },
            }
        };

        info!("Starting Axum server with graceful shutdown");

        if let Err(e) = axum::serve(listener, self.router.clone())
            .with_graceful_shutdown(shutdown_fn())
            .await
        {
            error!(error = ?e, "Axum server error");
        }

        info!("Axum service stopped");
    }

    fn name(&self) -> &str {
        "Axum Internal UI"
    }
}
