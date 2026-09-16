use crate::{HubError, auth};
use async_trait::async_trait;
use axum::http::Uri;
use pingora::prelude::*;
use std::str::FromStr;

pub struct WorkshopProxy {
    pub base_domain: String,
}

impl WorkshopProxy {
    pub fn new(base_domain: String) -> Self {
        Self { base_domain }
    }
}

/// Extract the subdomain from a Host header value given a base domain.
///
/// Examples:
///   "llm-embeddings.workshop.aivillage.org"      -> Some("llm-embeddings")
///   "llm-embeddings.workshop.aivillage.org:8080" -> Some("llm-embeddings")
///   "workshop.aivillage.org"                     -> None
///   "workshop.aivillage.org:8080"                -> None
///   "other.domain.com"                           -> None
fn extract_subdomain<'a>(host: &'a str, base_domain: &str) -> Option<&'a str> {
    if base_domain.is_empty() {
        return None;
    }

    let hostname = match host.split_once(':') {
        Some((h, _)) => h,
        None => host,
    };

    if hostname.len() <= base_domain.len() + 1 {
        return None;
    }

    let split_pos = hostname.len() - base_domain.len();
    if !hostname.is_char_boundary(split_pos) {
        return None;
    }

    let (prefix_with_dot, suffix) = hostname.split_at(split_pos);
    if !suffix.eq_ignore_ascii_case(base_domain) {
        return None;
    }

    let subdomain = prefix_with_dot.strip_suffix('.')?;
    if !subdomain.is_empty() && !subdomain.contains('.') {
        Some(subdomain)
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

        if let Some(workshop_name) = extract_subdomain(host, &self.base_domain) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_subdomain() {
        let base = "workshop.aivillage.org";
        assert_eq!(
            extract_subdomain("llm-embeddings.workshop.aivillage.org", base),
            Some("llm-embeddings")
        );
        assert_eq!(
            extract_subdomain("llm-embeddings.workshop.aivillage.org:8080", base),
            Some("llm-embeddings")
        );
        assert_eq!(extract_subdomain("workshop.aivillage.org", base), None);
        assert_eq!(extract_subdomain("workshop.aivillage.org:8080", base), None);
        assert_eq!(extract_subdomain("other.domain.com", base), None);
        assert_eq!(extract_subdomain(".workshop.aivillage.org", base), None);
        assert_eq!(
            extract_subdomain("nested.sub.workshop.aivillage.org", base),
            None
        );
    }

    #[test]
    fn test_extract_subdomain_case_insensitivity() {
        let base = "workshop.aivillage.org";
        assert_eq!(
            extract_subdomain("llm-embeddings.WORKSHOP.AIVILLAGE.ORG", base),
            Some("llm-embeddings")
        );
        assert_eq!(
            extract_subdomain("LLM-Embeddings.WorkShop.AiVillage.Org:8080", base),
            Some("LLM-Embeddings")
        );
        assert_eq!(extract_subdomain("WORKSHOP.AIVILLAGE.ORG", base), None);
        assert_eq!(extract_subdomain("WORKSHOP.AIVILLAGE.ORG:8080", base), None);

        let upper_base = "WORKSHOP.AIVILLAGE.ORG";
        assert_eq!(
            extract_subdomain("llm-embeddings.workshop.aivillage.org", upper_base),
            Some("llm-embeddings")
        );
        assert_eq!(
            extract_subdomain("workshop.aivillage.org", upper_base),
            None
        );
    }

    #[test]
    fn test_extract_subdomain_zero_allocation() {
        let base = "workshop.aivillage.org";
        let host = "llm-embeddings.workshop.aivillage.org:8080";
        let sub = extract_subdomain(host, base).expect("subdomain should be extracted");

        // The returned &str must be a direct slice of the original host string,
        // proving zero allocation.
        assert_eq!(sub, "llm-embeddings");
        assert_eq!(sub.as_ptr(), host.as_ptr());
    }
}
