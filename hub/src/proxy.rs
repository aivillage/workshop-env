use crate::{HubError, auth};
use async_trait::async_trait;
use axum::http::Uri;
use pingora::prelude::*;
use std::str::FromStr;

pub struct WorkshopProxy;

/// Extract the subdomain from a Host header value.
///
/// Examples:
///   "llm-embeddings.aiv.local"      -> Some("llm-embeddings")
///   "llm-embeddings.aiv.local:8080" -> Some("llm-embeddings")
///   "aiv.local"                     -> None
///   "aiv.local:8080"                -> None
///   "localhost"                     -> None
fn extract_subdomain(host: &str) -> Option<&str> {
    // Strip optional port  ("host:port" -> "host")
    let hostname = host.split(':').next().unwrap_or(host);

    // Split into at most 2 parts: subdomain and the rest
    // "llm-embeddings.aiv.local" -> ["llm-embeddings", "aiv.local"]
    // "aiv.local"                -> ["aiv", "local"]  (rest has no dot -> bare domain)
    let (first, rest) = hostname.split_once('.')?;

    // A subdomain exists only if the remainder itself contains a dot
    // (i.e. the full hostname has at least 3 segments).
    if !first.is_empty() && rest.contains('.') {
        Some(first)
    } else {
        None
    }
}

/// Helper to create a peer pointing at the local Axum UI service.
fn local_peer() -> Box<HttpPeer> {
    Box::new(HttpPeer::new("127.0.0.1:3000", false, String::new()))
}

#[async_trait]
impl ProxyHttp for WorkshopProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {
        ()
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path().to_string();
        let query = session
            .req_header()
            .uri
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default();

        // ── Auth check ───────────────────────────────────────────────
        let cookie_header = session
            .req_header()
            .headers
            .get("Cookie")
            .map(|v| v.to_str().unwrap_or_default())
            .unwrap_or_default();

        let user = if let Some(user) = auth::validate_cookie(cookie_header) {
            user
        } else {
            // Unauthenticated: allow login page and static assets through,
            // redirect everything else to login.
            if path == "/workshop-login"
                || path.starts_with("/public")
                || path.starts_with("/assets")
                || path == "/health"
            {
                return Ok(local_peer());
            } else {
                session
                    .req_header_mut()
                    .set_uri(Uri::from_static("/workshop-login"));
                return Ok(local_peer());
            }
        };

        // ── Subdomain routing ────────────────────────────────────────
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        if let Some(workshop_name) = extract_subdomain(host) {
            let orchestrator = crate::orchestrator().await;

            let local_error_path = match orchestrator
                .get_or_create_pod(&user.user_id, workshop_name)
                .await
            {
                Ok(upstream_url) => {
                    // Forward the request to the workshop pod with the
                    // original path + query intact (no rewriting needed).
                    tracing::info!(
                        upstream_url,
                        path,
                        user.user_id,
                        workshop_name,
                        "Routing subdomain to workshop pod"
                    );
                    return Ok(Box::new(HttpPeer::new(
                        upstream_url,
                        false,
                        String::new(),
                    )));
                }
                Err(HubError::PodLimitReached) => {
                    Some(format!("/workshop-at-capacity/{}", workshop_name))
                }
                Err(HubError::PodNotReady) => {
                    Some(format!("/workshop-pending/{}", workshop_name))
                }
                Err(HubError::Error(error)) => {
                    let encoded_error = serde_urlencoded::to_string([("message", error)])
                        .unwrap_or_default();
                    Some(format!(
                        "/workshop-error/{}?{}",
                        workshop_name, encoded_error
                    ))
                }
                Err(HubError::WorkshopNotFound) => Some("/error-404".to_string()),
            };

            // Error state — redirect to the local Axum error page
            if let Some(error_path) = local_error_path {
                let uri = match Uri::try_from(error_path) {
                    Ok(uri) => uri,
                    Err(_) => Uri::from_static("/workshop-error"),
                };
                session.req_header_mut().set_uri(uri);
                return Ok(local_peer());
            }
        }

        // ── Hub UI (bare domain) ─────────────────────────────────────
        // No subdomain → serve the hub UI (index, login, static assets, etc.)
        if path == "/" {
            let index_uri = Uri::from_str(&format!("/index{}", query))
                .unwrap_or_else(|_| Uri::from_static("/index"));
            session.req_header_mut().set_uri(index_uri);
        }

        Ok(local_peer())
    }
}

