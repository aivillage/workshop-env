//! This is just to make sure we don't let users accidentally
//! log in as each other. If they intentionally do that, it's fine.
//! 
//! We can correct this later, if needed. Workshops at security cons
//! are usually fun benign afairs. 

use std::env;

use axum::{Json, response::IntoResponse};
use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use tower_cookies::{Cookie, Cookies};

// Thank you for finding this security vul! We're not gonna fix it.
pub const JWT_SECRET: &[u8] =
    b"this-is-just-meant-to-reliably-segment-you-from-other-friendlies-known-not-secure";
pub const COOKIE_NAME: &str = "workshop_token";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user_id
    pub username: String,
    pub exp: i64,
    pub iat: i64,
}

/// User identity extracted from JWT
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIdentity {
    pub user_id: String,
    pub username: String,
}

// Sanitize username to create valid Kubernetes labels
fn sanitize_username(username: &str) -> String {
    username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .to_lowercase()
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    username: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    success: bool,
    message: String,
    redirect: Option<String>,
}

// Handle login POST request
pub async fn handle_login(
    cookies: Cookies,
    Json(login_req): Json<LoginRequest>,
) -> impl IntoResponse {
    tracing::info!(
        "🔐 Login attempt for username: '{}' from IP: [extract from request if available]",
        login_req.username
    );

    // Check if there's already a cookie
    if let Some(_) = cookies.get(COOKIE_NAME) {
        tracing::debug!("Found existing cookie during login, will be replaced");
        cookies.remove(Cookie::from(COOKIE_NAME));
    }

    let sanitized = sanitize_username(&login_req.username);

    // Check if the sanitized username is at least 4 characters
    if sanitized.len() < 4 {
        tracing::warn!("Login failed: Username '{}' is too short after sanitization", login_req.username);
        return Json(LoginResponse {
            success: false,
            message: "Username must be at least 4 characters (alphanumeric only).".to_string(),
            redirect: None,
        });
    }
    let user_id = format!("user-{}", sanitized);

    let iat = Utc::now();
    let expiration = Utc::now() + Duration::hours(24);
    let claims = Claims {
        sub: user_id.clone(),
        username: login_req.username.clone(),
        exp: expiration.timestamp(),
        iat: iat.timestamp(),
    };

    tracing::debug!(
        "Creating JWT for user_id: {}, expires at: {}",
        user_id,
        expiration
    );

    let token = match encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(JWT_SECRET),
    ) {
        Ok(t) => {
            tracing::trace!("JWT created successfully (token length: {})", t.len());
            t
        }
        Err(e) => {
            tracing::error!("❌ Failed to create JWT: {}", e);
            return Json(LoginResponse {
                success: false,
                message: "Authentication error".to_string(),
                redirect: None,
            });
        }
    };

    let mut cookie = Cookie::new(COOKIE_NAME, token);
    cookie.set_http_only(true);
    cookie.set_same_site(tower_cookies::cookie::SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(tower_cookies::cookie::time::Duration::hours(24));
    
    if let Ok(domain) = env::var("COOKIE_DOMAIN") {
        if !domain.is_empty() {
            tracing::debug!("Setting cookie domain to: {}", domain);
            cookie.set_domain(domain);
        }
    }

    tracing::debug!("Setting cookie with max_age: 24 hours");
    cookies.add(cookie);

    tracing::info!(
        "✅ Login successful - user_id: {}, username: {}",
        user_id,
        login_req.username
    );

    Json(LoginResponse {
        success: true,
        message: "Login successful".to_string(),
        redirect: Some("/".to_string()),
    })
}

pub fn validate_cookie(cookie_header: &str) -> Option<UserIdentity> {
    // 1. Simple manual parsing of the Cookie header string
    Cookie::split_parse(cookie_header)
        .filter_map(|c| c.ok())
        .find(|c| c.name() == COOKIE_NAME)
        .and_then(|c| {
            let token = c.value();
            match decode::<Claims>(
                token,
                &DecodingKey::from_secret(JWT_SECRET),
                &Validation::default(),
            ) {
                Ok(token_data) => Some(UserIdentity {
                    user_id: token_data.claims.sub,
                    username: token_data.claims.username,
                }),
                Err(e) => {
                    tracing::debug!("JWT validation failed: {:?}", e);
                    None
                }
            }
        })
}
