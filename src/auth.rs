//! Optional websocket authentication. Compiled only with the `auth` feature.
//!
//! Validates an `Authorization: Bearer <jwt>` header on each incoming websocket
//! connection, using one of three strategies ([`crate::config::WsAuth`]): an
//! HS256 shared secret, a JWKS document URL, or OIDC issuer discovery.
//!
//! On top of any strategy, issuer/audience bindings
//! ([`crate::config::WsClaimBindings`], MC-2) and a mandatory `exp` claim on the
//! JWKS/OIDC path (MC-3) are enforced so a same-IdP token minted for some other
//! service — or a non-expiring token — cannot authenticate.

use std::sync::Arc;

use axum::http::HeaderMap;
use jwtk::jwk::RemoteJwksVerifier;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::{WsAuth, WsClaimBindings};
use crate::error::{Error, Result};

/// JWKS cache lifetime (MC-3). jwtk's own default; far shorter than the former
/// 3600s, which capped key-rotation lag at an hour. On an unknown `kid` jwtk
/// additionally refetches immediately (rate-limited by its own cooldown), so a
/// freshly rotated key is picked up without waiting for this to expire.
const JWKS_CACHE_DURATION: std::time::Duration = std::time::Duration::from_secs(300);

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
    /// Build an authenticator from a [`WsAuth`] strategy plus the configured
    /// issuer/audience [`WsClaimBindings`] (MC-2). [`WsAuth::OidcIssuer`]
    /// performs OIDC discovery (one network call) and fails if the issuer is
    /// unreachable or its `issuer` claim doesn't match; it also binds the
    /// token's `iss` to the discovered issuer automatically.
    pub async fn from(auth: WsAuth, bindings: WsClaimBindings) -> Result<Self> {
        let kind = match auth {
            WsAuth::None => Kind::None,
            WsAuth::Secret(secret) => {
                // jsonwebtoken's default Validation already *requires* `exp`.
                // Layer the MC-2 iss/aud bindings on top when configured.
                let mut validation = jsonwebtoken::Validation::default();
                if let Some(iss) = &bindings.issuer {
                    validation.set_issuer(&[iss]);
                    // `set_issuer` validates the value *when present*; also
                    // require it to be present so a token with no `iss` can't
                    // slip through (MC-2).
                    validation.required_spec_claims.insert("iss".to_string());
                }
                match &bindings.audience {
                    Some(aud) => {
                        validation.set_audience(&[aud]);
                        validation.required_spec_claims.insert("aud".to_string());
                    }
                    // jsonwebtoken validates `aud` by default; with no expected
                    // audience configured we must turn that off explicitly,
                    // otherwise every token is rejected for a missing audience.
                    None => validation.validate_aud = false,
                }
                Kind::Secret {
                    key: jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
                    validation,
                }
            }
            WsAuth::Jwks(url) => Kind::Jwks(JwksVerifier::from_jwks_url(&url, bindings)),
            WsAuth::OidcIssuer(issuer) => {
                Kind::Jwks(JwksVerifier::from_oidc_issuer(&issuer, bindings).await?)
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

/// Verifies JWTs against a JWKS endpoint, with lazy key fetching + caching, then
/// enforces a present `exp` (MC-3) and the configured issuer/audience bindings
/// (MC-2).
struct JwksVerifier {
    jwks_url: String,
    bindings: WsClaimBindings,
    verifier: Arc<RwLock<Option<Arc<RemoteJwksVerifier>>>>,
}

impl JwksVerifier {
    fn from_jwks_url(jwks_url: &str, bindings: WsClaimBindings) -> Self {
        Self {
            jwks_url: jwks_url.to_string(),
            bindings,
            verifier: Arc::new(RwLock::new(None)),
        }
    }

    async fn from_oidc_issuer(issuer_url: &str, mut bindings: WsClaimBindings) -> Result<Self> {
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
        // MC-2: an OIDC issuer always binds the token's `iss` to itself. An
        // explicit `websocket_expected_issuer` would only ever be the same
        // value, but if one was set and disagrees, fail loudly at startup.
        match &bindings.issuer {
            Some(explicit) if explicit.trim_end_matches('/') != config_issuer => {
                return Err(Error::Config(format!(
                    "auth: configured expected issuer {explicit} disagrees with OIDC issuer {config_issuer}"
                )));
            }
            _ => bindings.issuer = Some(config_issuer.to_string()),
        }
        Ok(Self::from_jwks_url(&config.jwks_uri, bindings))
    }

    async fn get_verifier(&self) -> Arc<RemoteJwksVerifier> {
        if let Some(v) = self.verifier.read().await.as_ref() {
            return Arc::clone(v);
        }
        let verifier = Arc::new(
            RemoteJwksVerifier::builder(self.jwks_url.clone())
                .with_cache_duration(JWKS_CACHE_DURATION)
                .build(),
        );
        *self.verifier.write().await = Some(Arc::clone(&verifier));
        verifier
    }

    async fn verify(&self, token: &str) -> std::result::Result<(), String> {
        // jwtk checks the signature, `alg`, and (when present) `exp`/`nbf`.
        let verified = self
            .get_verifier()
            .await
            .verify::<serde_json::Value>(token)
            .await
            .map_err(|e| e.to_string())?;
        let claims = verified.claims();
        check_claim_bindings(
            claims.exp.is_some(),
            claims.iss.as_deref(),
            claims.aud.iter().map(String::as_str),
            &self.bindings,
        )
    }
}

/// Enforce the post-signature claim policy on the JWKS/OIDC path: a present
/// `exp` (MC-3 — jwtk only checks `exp` when present) and the configured
/// issuer/audience bindings (MC-2).
///
/// Takes borrowed primitives rather than jwtk's `#[non_exhaustive]` `Claims`, so
/// it is decoupled from that type and unit-testable directly.
fn check_claim_bindings<'a>(
    has_exp: bool,
    iss: Option<&str>,
    mut auds: impl Iterator<Item = &'a str>,
    bindings: &WsClaimBindings,
) -> std::result::Result<(), String> {
    if !has_exp {
        return Err("token is missing the required `exp` claim".to_string());
    }
    if let Some(expected) = &bindings.issuer {
        match iss {
            Some(iss) if iss == expected => {}
            Some(iss) => return Err(format!("issuer mismatch: expected {expected}, got {iss}")),
            None => {
                return Err(format!(
                    "token is missing the required `iss` claim ({expected})"
                ));
            }
        }
    }
    if let Some(expected) = &bindings.audience
        && !auds.any(|a| a == expected)
    {
        return Err(format!(
            "token `aud` does not contain the required audience {expected}"
        ));
    }
    Ok(())
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

    /// Build an authenticator with no extra issuer/audience bindings.
    async fn auth(ws: WsAuth) -> Authenticator {
        Authenticator::from(ws, WsClaimBindings::default())
            .await
            .unwrap()
    }

    /// Mint an HS256 token from `claims` signed with `secret`.
    fn hs256(secret: &[u8], claims: &serde_json::Value) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn none_accepts_anything() {
        let a = auth(WsAuth::None).await;
        assert!(!a.is_enabled());
        assert!(a.check(&HeaderMap::new()).await.is_ok());
    }

    #[tokio::test]
    async fn secret_accepts_valid_and_rejects_invalid() {
        let secret = "topsecret";
        let a = auth(WsAuth::Secret(secret.into())).await;
        assert!(a.is_enabled());

        // exp in the far future so the default validation passes.
        let claims = json!({ "sub": "u1", "exp": 9_999_999_999u64 });
        let token = hs256(secret.as_bytes(), &claims);
        assert!(a.check(&bearer(&token)).await.is_ok());

        // Wrong secret → rejected.
        let bad = hs256(b"wrong", &claims);
        assert!(a.check(&bearer(&bad)).await.is_err());
    }

    #[tokio::test]
    async fn secret_rejects_missing_or_malformed_header() {
        let a = auth(WsAuth::Secret("s".into())).await;
        assert!(a.check(&HeaderMap::new()).await.is_err());
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic xyz".parse().unwrap(),
        );
        assert!(a.check(&h).await.is_err());
        assert!(a.check(&bearer("")).await.is_err());
    }

    // --- MC-2: HS256 issuer/audience binding ---

    #[tokio::test]
    async fn secret_no_binding_accepts_token_with_aud() {
        // With no expected audience configured, a token that *carries* an `aud`
        // claim must still be accepted (we must disable jsonwebtoken's
        // default aud validation, not leave it on with an empty expected set).
        let secret = b"s3cr3t";
        let a = auth(WsAuth::Secret("s3cr3t".into())).await;
        let token = hs256(
            secret,
            &json!({ "sub": "u1", "exp": 9_999_999_999u64, "aud": "someone-else" }),
        );
        assert!(a.check(&bearer(&token)).await.is_ok());
    }

    #[tokio::test]
    async fn secret_binds_issuer() {
        let secret = b"s3cr3t";
        let a = Authenticator::from(
            WsAuth::Secret("s3cr3t".into()),
            WsClaimBindings {
                issuer: Some("https://idp.example".into()),
                audience: None,
            },
        )
        .await
        .unwrap();

        let ok = hs256(
            secret,
            &json!({ "exp": 9_999_999_999u64, "iss": "https://idp.example" }),
        );
        assert!(a.check(&bearer(&ok)).await.is_ok());

        // Same IdP secret, wrong/other issuer → rejected (the core MC-2 case).
        let wrong = hs256(
            secret,
            &json!({ "exp": 9_999_999_999u64, "iss": "https://attacker.example" }),
        );
        assert!(a.check(&bearer(&wrong)).await.is_err());

        // Missing iss entirely → rejected.
        let missing = hs256(secret, &json!({ "exp": 9_999_999_999u64 }));
        assert!(a.check(&bearer(&missing)).await.is_err());
    }

    #[tokio::test]
    async fn secret_binds_audience() {
        let secret = b"s3cr3t";
        let a = Authenticator::from(
            WsAuth::Secret("s3cr3t".into()),
            WsClaimBindings {
                issuer: None,
                audience: Some("mcp-core".into()),
            },
        )
        .await
        .unwrap();

        let ok = hs256(
            secret,
            &json!({ "exp": 9_999_999_999u64, "aud": "mcp-core" }),
        );
        assert!(a.check(&bearer(&ok)).await.is_ok());

        // Token minted for a different audience → rejected.
        let wrong = hs256(
            secret,
            &json!({ "exp": 9_999_999_999u64, "aud": "other-service" }),
        );
        assert!(a.check(&bearer(&wrong)).await.is_err());

        // Missing aud entirely → rejected.
        let missing = hs256(secret, &json!({ "exp": 9_999_999_999u64 }));
        assert!(a.check(&bearer(&missing)).await.is_err());
    }

    // --- MC-2 / MC-3: JWKS-path claim policy (pure, no network) ---

    fn check(
        has_exp: bool,
        iss: Option<&str>,
        auds: &[&str],
        bindings: &WsClaimBindings,
    ) -> std::result::Result<(), String> {
        check_claim_bindings(has_exp, iss, auds.iter().copied(), bindings)
    }

    #[test]
    fn jwks_requires_exp() {
        // MC-3: a token with no `exp` is valid forever on the JWKS path unless
        // we reject it ourselves (jwtk only checks exp when present).
        let bindings = WsClaimBindings::default();
        assert!(
            check(false, None, &[], &bindings).is_err(),
            "missing exp must be rejected"
        );
        assert!(
            check(true, None, &[], &bindings).is_ok(),
            "present exp with no bindings is accepted"
        );
    }

    #[test]
    fn jwks_binds_issuer_and_audience() {
        let bindings = WsClaimBindings {
            issuer: Some("https://idp.example".into()),
            audience: Some("mcp-core".into()),
        };
        let auds = ["other", "mcp-core"];
        // Matching iss + aud-contains → ok.
        assert!(check(true, Some("https://idp.example"), &auds, &bindings).is_ok());
        // Wrong issuer → rejected (the core MC-2 case).
        assert!(check(true, Some("https://attacker.example"), &auds, &bindings).is_err());
        // Missing issuer → rejected.
        assert!(check(true, None, &auds, &bindings).is_err());
        // Audience not contained → rejected.
        assert!(
            check(
                true,
                Some("https://idp.example"),
                &["only-other"],
                &bindings
            )
            .is_err()
        );
        // Present exp is still required even with bindings.
        assert!(check(false, Some("https://idp.example"), &auds, &bindings).is_err());
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
