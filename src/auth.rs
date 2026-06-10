//! Optional websocket authentication. Compiled only with the `auth` feature.
//!
//! Validates an `Authorization: Bearer <jwt>` header on each incoming websocket
//! connection, using one of three strategies ([`crate::config::WsAuth`]): an
//! HS256 shared secret, a JWKS document URL, or OIDC issuer discovery.

use std::sync::Arc;

use axum::http::HeaderMap;
use jwtk::jwk::RemoteJwksVerifier;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::WsAuth;
use crate::error::{Error, Result};

/// Reason a websocket connection failed authentication; rendered as the body of
/// the `401 Unauthorized` response.
#[derive(Debug)]
pub struct AuthError(pub String);

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates the `Authorization` header on incoming websocket connections.
/// Built once at startup (resolving OIDC discovery if needed) and shared across
/// connections.
pub struct Authenticator {
    kind: Kind,
}

// Built exactly once at startup and shared by reference — there is never a
// collection of these — so the inter-variant size difference is irrelevant and
// boxing would only add indirection.
#[allow(clippy::large_enum_variant)]
enum Kind {
    None,
    Secret {
        key: jsonwebtoken::DecodingKey,
        validation: jsonwebtoken::Validation,
    },
    Jwks(JwksVerifier),
}

impl Authenticator {
    /// Build an authenticator from a [`WsAuth`]. [`WsAuth::OidcIssuer`] performs
    /// OIDC discovery (one network call) and fails if the issuer is unreachable
    /// or its `issuer` claim doesn't match.
    pub async fn from(auth: WsAuth) -> Result<Self> {
        let kind = match auth {
            WsAuth::None => Kind::None,
            WsAuth::Secret(secret) => Kind::Secret {
                key: jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
                validation: jsonwebtoken::Validation::default(),
            },
            WsAuth::Jwks(url) => Kind::Jwks(JwksVerifier::from_jwks_url(&url)),
            WsAuth::OidcIssuer(issuer) => {
                Kind::Jwks(JwksVerifier::from_oidc_issuer(&issuer).await?)
            }
        };
        Ok(Self { kind })
    }

    /// Whether this authenticator enforces anything (`false` for `WsAuth::None`).
    pub fn is_enabled(&self) -> bool {
        !matches!(self.kind, Kind::None)
    }

    /// Validate the request headers. `Ok(())` means the connection may proceed.
    pub async fn check(&self, headers: &HeaderMap) -> std::result::Result<(), AuthError> {
        match &self.kind {
            Kind::None => Ok(()),
            Kind::Secret { key, validation } => {
                let token = extract_bearer(headers)?;
                jsonwebtoken::decode::<serde_json::Value>(&token, key, validation)
                    .map(|_| ())
                    .map_err(|e| AuthError(format!("JWT validation failed: {e}")))
            }
            Kind::Jwks(verifier) => {
                let token = extract_bearer(headers)?;
                verifier
                    .verify(&token)
                    .await
                    .map_err(|e| AuthError(format!("JWT verification failed: {e}")))
            }
        }
    }
}

/// Pull the token out of an `Authorization: Bearer <token>` header.
fn extract_bearer(headers: &HeaderMap) -> std::result::Result<String, AuthError> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| AuthError("missing Authorization header".into()))?
        .to_str()
        .map_err(|_| AuthError("invalid Authorization header".into()))?;
    // MC-9: RFC 7235 auth-scheme names are case-insensitive ("Bearer",
    // "bearer", "BEARER" are all valid). Split off the first whitespace-
    // delimited token and compare the scheme case-insensitively.
    let (scheme, rest) = value
        .split_once(' ')
        .ok_or_else(|| AuthError("expected `Authorization: Bearer <token>`".into()))?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(AuthError("expected `Authorization: Bearer <token>`".into()));
    }
    let token = rest.trim();
    if token.is_empty() {
        return Err(AuthError("empty Bearer token".into()));
    }
    Ok(token.to_string())
}

// --- JWKS / OIDC verifier (ported from the gen-mcp implementation) ---

#[derive(Debug, Clone, Deserialize)]
struct OidcConfig {
    jwks_uri: String,
    issuer: String,
}

/// Verifies JWTs against a JWKS endpoint, with lazy key fetching + caching.
struct JwksVerifier {
    jwks_url: String,
    verifier: Arc<RwLock<Option<Arc<RemoteJwksVerifier>>>>,
}

impl JwksVerifier {
    fn from_jwks_url(jwks_url: &str) -> Self {
        Self {
            jwks_url: jwks_url.to_string(),
            verifier: Arc::new(RwLock::new(None)),
        }
    }

    async fn from_oidc_issuer(issuer_url: &str) -> Result<Self> {
        let issuer_url = issuer_url.trim_end_matches('/');
        let well_known = format!("{issuer_url}/.well-known/openid-configuration");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Config(format!("auth: building http client: {e}")))?;
        let config: OidcConfig = client
            .get(&well_known)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| Error::Config(format!("auth: OIDC discovery failed: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Config(format!("auth: parsing OIDC config: {e}")))?;
        let config_issuer = config.issuer.trim_end_matches('/');
        if config_issuer != issuer_url {
            return Err(Error::Config(format!(
                "auth: OIDC issuer mismatch: expected {issuer_url}, got {config_issuer}"
            )));
        }
        Ok(Self::from_jwks_url(&config.jwks_uri))
    }

    async fn get_verifier(&self) -> Arc<RemoteJwksVerifier> {
        if let Some(v) = self.verifier.read().await.as_ref() {
            return Arc::clone(v);
        }
        let verifier = Arc::new(
            RemoteJwksVerifier::builder(self.jwks_url.clone())
                .with_cache_duration(std::time::Duration::from_secs(3600))
                .build(),
        );
        *self.verifier.write().await = Some(Arc::clone(&verifier));
        verifier
    }

    async fn verify(&self, token: &str) -> std::result::Result<(), String> {
        self.get_verifier()
            .await
            .verify::<serde_json::Value>(token)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    #[tokio::test]
    async fn none_accepts_anything() {
        let a = Authenticator::from(WsAuth::None).await.unwrap();
        assert!(!a.is_enabled());
        assert!(a.check(&HeaderMap::new()).await.is_ok());
    }

    #[tokio::test]
    async fn secret_accepts_valid_and_rejects_invalid() {
        let secret = "topsecret";
        let a = Authenticator::from(WsAuth::Secret(secret.into()))
            .await
            .unwrap();
        assert!(a.is_enabled());

        // exp in the far future so the default validation passes.
        let claims = json!({ "sub": "u1", "exp": 9_999_999_999u64 });
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap();
        assert!(a.check(&bearer(&token)).await.is_ok());

        // Wrong secret → rejected.
        let bad = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(b"wrong"),
        )
        .unwrap();
        assert!(a.check(&bearer(&bad)).await.is_err());
    }

    #[tokio::test]
    async fn secret_rejects_missing_or_malformed_header() {
        let a = Authenticator::from(WsAuth::Secret("s".into()))
            .await
            .unwrap();
        assert!(a.check(&HeaderMap::new()).await.is_err());
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic xyz".parse().unwrap(),
        );
        assert!(a.check(&h).await.is_err());
        assert!(a.check(&bearer("")).await.is_err());
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        // MC-9: RFC 7235 auth-scheme names are case-insensitive.
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::header::AUTHORIZATION,
                format!("{scheme} tok123").parse().unwrap(),
            );
            assert_eq!(
                extract_bearer(&h).unwrap(),
                "tok123",
                "scheme {scheme} should be accepted"
            );
        }
    }

    #[test]
    fn bearer_optional_whitespace_and_empty_token() {
        // Extra spaces after the scheme are tolerated; an empty token is not.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "bearer    spaced".parse().unwrap(),
        );
        assert_eq!(extract_bearer(&h).unwrap(), "spaced");

        let mut empty = HeaderMap::new();
        empty.insert(
            axum::http::header::AUTHORIZATION,
            "bearer    ".parse().unwrap(),
        );
        assert!(extract_bearer(&empty).is_err());

        // A non-Bearer scheme is still rejected.
        let mut basic = HeaderMap::new();
        basic.insert(
            axum::http::header::AUTHORIZATION,
            "Basic abc".parse().unwrap(),
        );
        assert!(extract_bearer(&basic).is_err());
    }
}
