use crate::{HubError, auth};
use async_trait::async_trait;
use axum::http::Uri;
use pingora::prelude::*;
use std::str::FromStr;

pub struct WorkshopProxy;

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
        // Capture path and query for analysis
        let path = session.req_header().uri.path().to_string();
        let query = session
            .req_header()
            .uri
            .query()
            .map(|q| format!("?{}", q))
            .unwrap_or_default();


        // Validate Cookie
        let cookie_header = session
            .req_header()
            .headers
            .get("Cookie")
            .map(|v| v.to_str().unwrap_or_default())
            .unwrap_or_default();
        let user = if let Some(user) = auth::validate_cookie(cookie_header) {
            user
        } else {
            // If not logged in, allow the request ONLY if it is explicitly for the login page
            // or common static asset paths. Otherwise, redirect flow to login.
            if path == "/workshop-login"
                || path.starts_with("/public")
                || path.starts_with("/assets")
                || path == "/health"
            {
                // Pass through to local UI handler as-is
                return Ok(Box::new(HttpPeer::new(
                    "127.0.0.1:3000",
                    false,
                    String::new(),
                )));
            } else {
                // Rewrite all other unauthed traffic to the login page
                session
                    .req_header_mut()
                    .set_uri(Uri::from_static("/workshop-login"));
                return Ok(Box::new(HttpPeer::new(
                    "127.0.0.1:3000",
                    false,
                    String::new(),
                )));
            }
        };

        // 1. ROOT PATH HANDLER -> Send to Index
        if path == "/" {
            let index_uri = Uri::from_str(&format!("/index{}", query))
                .unwrap_or_else(|_| Uri::from_static("/index"));

            session.req_header_mut().set_uri(index_uri);
            return Ok(Box::new(HttpPeer::new(
                "127.0.0.1:3000",
                false,
                String::new(),
            )));
        }

        // 2. WORKSHOP HANDLER -> /workshop/<NAME>/...
        if let Some(stripped_prefix) = path.strip_prefix("/workshop/") {
            // Split path: "workshop_name/remainder"
            // Example: "python-lab/notebooks/foo" -> name: "python-lab", target: "/notebooks/foo"
            let (workshop_name, target_path) = match stripped_prefix.split_once('/') {
                Some((name, rest)) => (name, format!("/{}", rest)),
                None => (stripped_prefix, "/".to_string()),
            };

            let orchestrator = crate::orchestrator().await;

            // Determine if we get a Pod URL or a Local Error Path
            // Pass the extracted workshop_name to the orchestrator
            let local_error_path = match orchestrator
                .get_or_create_pod(&user.user_id, workshop_name)
                .await
            {
                Ok(upstream_url) => {
                    // SUCCESS:
                    // 1. Rewrite the URI to strip the prefix (e.g. /notebooks/foo?q=1)
                    let new_uri_string = format!("{}{}", target_path, query);
                    if let Ok(new_uri) = Uri::from_str(&new_uri_string) {
                        session.req_header_mut().set_uri(new_uri);
                    } else {
                        tracing::error!(new_uri_string, "Couldn't parse new uri");
                    }
                    tracing::info!(upstream_url, new_uri_string, user.user_id, "Sending people to workshop");

                    // 2. Return the peer for the workshop pod
                    let peer = Box::new(HttpPeer::new(upstream_url, false, String::new()));
                    return Ok(peer);
                }
                Err(HubError::PodLimitReached) => {
                    Some(format!("/workshop-at-capacity/{}", workshop_name))
                }
                Err(HubError::PodNotReady) => Some(format!("/workshop-pending/{}", workshop_name)),
                Err(HubError::Error(error)) => {
                    let encoded_error = serde_urlencoded::to_string([("message", error)])
                        .unwrap_or_default();
                    Some(format!("/workshop-error/{}?{}", workshop_name, encoded_error))
                }
                Err(HubError::WorkshopNotFound) => Some("/error-404".to_string()),
            };

            // If we are here, we have a local error path to handle
            if let Some(error_path) = local_error_path {
                let uri = match Uri::try_from(error_path) {
                    Ok(uri) => uri,
                    Err(_) => Uri::from_static("/workshop-error"),
                };
                session.req_header_mut().set_uri(uri);
                return Ok(Box::new(HttpPeer::new(
                    "127.0.0.1:3000",
                    false,
                    String::new(),
                )));
            }
        }

        // 3. FALLBACK -> Send anything else to local provider as-is
        // This handles CSS, JS, or other local routes not under /workshop
        Ok(Box::new(HttpPeer::new(
            "127.0.0.1:3000",
            false,
            String::new(),
        )))
    }
}
