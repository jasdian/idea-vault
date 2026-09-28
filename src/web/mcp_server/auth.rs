//! Single-token Bearer gate for the inbound MCP route (docs/adr/0024).
//!
//! Adapted from the sibling `mcp-server` repo's `AuthLayer`/`AuthMiddleware` Tower pair, but
//! simplified: idea-vault is solo, so there is no `CredentialsStore` and no per-user identity to
//! resolve — this is a boolean "does the presented token match the configured one" check, and on
//! success the request is forwarded unchanged (nothing is injected into extensions, because
//! `handler::IdeaVaultMcpServer` never needs a caller identity).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use tower::{Layer, Service};

/// JSON-RPC error code for a rejected Bearer token (mirrors `mcp-server`'s `ERROR_AUTH`).
const ERROR_AUTH: i32 = -32001;

#[derive(Debug)]
enum AuthError {
    MissingToken,
    InvalidFormat,
    InvalidToken,
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let message = match self {
            AuthError::MissingToken => "Missing Authorization header",
            AuthError::InvalidFormat => "Authorization header must be 'Bearer <token>'",
            AuthError::InvalidToken => "Invalid MCP token",
        };
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "jsonrpc": "2.0",
                "error": { "code": ERROR_AUTH, "message": message },
            })),
        )
            .into_response()
    }
}

/// Constant-time byte comparison — a plain `==` on the presented token would let response timing
/// leak how many leading bytes matched. The length check short-circuits (lengths aren't secret),
/// but every byte of an equal-length guess is compared regardless of an earlier mismatch.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authenticate(headers: &HeaderMap, token: &str) -> Result<(), AuthError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::MissingToken)?;
    let presented = header
        .strip_prefix("Bearer ")
        .ok_or(AuthError::InvalidFormat)?;
    if constant_time_eq(presented, token) {
        Ok(())
    } else {
        Err(AuthError::InvalidToken)
    }
}

/// Tower layer wrapping a service with the Bearer check.
#[derive(Clone)]
pub struct AuthLayer {
    token: Arc<str>,
}

impl AuthLayer {
    pub fn new(token: String) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddleware {
            inner,
            token: self.token.clone(),
        }
    }
}

/// Tower middleware service performing the Bearer check.
#[derive(Clone)]
pub struct AuthMiddleware<S> {
    inner: S,
    token: Arc<str>,
}

impl<S> Service<Request> for AuthMiddleware<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        match authenticate(req.headers(), &self.token) {
            Ok(()) => {
                let future = self.inner.call(req);
                Box::pin(future)
            }
            Err(e) => Box::pin(async move { Ok(e.into_response()) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    #[test]
    fn valid_token_authenticates() {
        assert!(authenticate(&headers_with_bearer("s3cr3t"), "s3cr3t").is_ok());
    }

    #[test]
    fn missing_header_is_missing_token() {
        assert!(matches!(
            authenticate(&HeaderMap::new(), "s3cr3t"),
            Err(AuthError::MissingToken)
        ));
    }

    #[test]
    fn wrong_scheme_is_invalid_format() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert!(matches!(
            authenticate(&headers, "s3cr3t"),
            Err(AuthError::InvalidFormat)
        ));
    }

    #[test]
    fn wrong_token_is_invalid_token() {
        assert!(matches!(
            authenticate(&headers_with_bearer("nope"), "s3cr3t"),
            Err(AuthError::InvalidToken)
        ));
    }
}
