//! Bearer-token auth, ported from `src/auth.ts`, as an axum middleware.
//!
//! When no API key is configured, everything passes. When one is, every request
//! except `/health` must carry `Authorization: Bearer <key>`.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::types::AppConfig;

pub fn generate_internal_bearer_token() -> anyhow::Result<String> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("generate internal MCP bearer token: {error}"))?;
    Ok(format!("codexify_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

pub async fn require_auth(
    State(config): State<Arc<AppConfig>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(key) = &config.api_key
        && request.uri().path() != "/health"
    {
        let expected = format!("Bearer {key}");
        let provided = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if provided != Some(expected.as_str()) {
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_bearer_tokens_are_high_entropy_and_url_safe() {
        let first = generate_internal_bearer_token().unwrap();
        let second = generate_internal_bearer_token().unwrap();

        assert_ne!(first, second);
        assert!(first.starts_with("codexify_"));
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
        assert!(first.len() >= 50);
    }
}
