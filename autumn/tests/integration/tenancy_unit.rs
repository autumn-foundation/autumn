use autumn_web::config::AutumnConfig;
use autumn_web::tenancy::extract_tenant_from_parts;
use axum::http::Request;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    company: String,
    exp: usize,
    iss: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aud: Option<String>,
}

// Helper to generate a signed JWT
fn generate_jwt(tenant: &str, secret: &str, expired: bool, issuer: Option<&str>) -> String {
    let my_claims = Claims {
        sub: "1234567890".to_owned(),
        company: tenant.to_owned(),
        exp: if expired { 1 } else { 10_000_000_000 },
        iss: issuer.map(std::borrow::ToOwned::to_owned),
        aud: None,
    };
    let header = Header::new(Algorithm::HS256);
    encode(
        &header,
        &my_claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

// Helper to generate a JWT with an audience claim
fn generate_jwt_with_audience(tenant: &str, secret: &str, audience: Option<&str>) -> String {
    let my_claims = Claims {
        sub: "1234567890".to_owned(),
        company: tenant.to_owned(),
        exp: 10_000_000_000,
        iss: None,
        aud: audience.map(std::borrow::ToOwned::to_owned),
    };
    let header = Header::new(Algorithm::HS256);
    encode(
        &header,
        &my_claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

// Test case-insensitive Bearer check
#[tokio::test]
async fn test_case_insensitive_bearer_and_valid_jwt() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_string();
    config.tenancy.jwt_claim = "company".to_string();
    config.tenancy.jwt_secret = Some("my-secret".to_string());
    config.tenancy.jwt_issuer = Some("my-issuer".to_string());

    let token = generate_jwt("tenant123", "my-secret", false, Some("my-issuer"));

    // Case 1: Standard "Bearer "
    let req1 = Request::builder()
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (mut parts1, ()) = req1.into_parts();
    let tenant1 = extract_tenant_from_parts(&mut parts1, &config)
        .await
        .expect("Should extract tenant with valid Bearer prefix");
    assert_eq!(tenant1, "tenant123");

    // Case 2: lowercase "bearer "
    let req2 = Request::builder()
        .header("Authorization", format!("bearer {token}"))
        .body(())
        .unwrap();
    let (mut parts2, ()) = req2.into_parts();
    let tenant2 = extract_tenant_from_parts(&mut parts2, &config)
        .await
        .expect("Should extract tenant with lowercase bearer prefix");
    assert_eq!(tenant2, "tenant123");

    // Case 3: mixed case "bEaReR "
    let req3 = Request::builder()
        .header("Authorization", format!("bEaReR {token}"))
        .body(())
        .unwrap();
    let (mut parts3, ()) = req3.into_parts();
    let tenant3 = extract_tenant_from_parts(&mut parts3, &config)
        .await
        .expect("Should extract tenant with mixed-case bearer prefix");
    assert_eq!(tenant3, "tenant123");
}

// Test JWT validation and signature verification
#[tokio::test]
async fn test_jwt_verification_signature_and_expiration() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_string();
    config.tenancy.jwt_claim = "company".to_string();
    config.tenancy.jwt_secret = Some("my-secret".to_string());
    config.tenancy.jwt_issuer = Some("my-issuer".to_string());

    // 1. Untrusted signature (forged payload)
    let forged_token = generate_jwt("tenant123", "wrong-secret", false, Some("my-issuer"));
    let req1 = Request::builder()
        .header("Authorization", format!("Bearer {forged_token}"))
        .body(())
        .unwrap();
    let (mut parts1, ()) = req1.into_parts();
    let err1 = extract_tenant_from_parts(&mut parts1, &config)
        .await
        .unwrap_err();
    let err1_str = err1.to_string().to_lowercase();
    assert!(err1_str.contains("signature") || err1_str.contains("unauthorized"));

    // 2. Expired JWT
    let expired_token = generate_jwt("tenant123", "my-secret", true, Some("my-issuer"));
    let req2 = Request::builder()
        .header("Authorization", format!("Bearer {expired_token}"))
        .body(())
        .unwrap();
    let (mut parts2, ()) = req2.into_parts();
    let err2 = extract_tenant_from_parts(&mut parts2, &config)
        .await
        .unwrap_err();
    let err2_str = err2.to_string().to_lowercase();
    assert!(err2_str.contains("expired") || err2_str.contains("unauthorized"));

    // 3. Incorrect issuer
    let bad_iss_token = generate_jwt("tenant123", "my-secret", false, Some("wrong-issuer"));
    let req3 = Request::builder()
        .header("Authorization", format!("Bearer {bad_iss_token}"))
        .body(())
        .unwrap();
    let (mut parts3, ()) = req3.into_parts();
    let err3 = extract_tenant_from_parts(&mut parts3, &config)
        .await
        .unwrap_err();
    let err3_str = err3.to_string().to_lowercase();
    assert!(err3_str.contains("issuer") || err3_str.contains("unauthorized"));
}

// Test rejection of non-subdomain hosts in subdomain mode
#[tokio::test]
async fn test_subdomain_mode_apex_and_ip_rejection() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "subdomain".to_string();

    // 1. Valid subdomain
    let req1 = Request::builder()
        .header("Host", "tenant1.example.com")
        .body(())
        .unwrap();
    let (mut parts1, ()) = req1.into_parts();
    let tenant1 = extract_tenant_from_parts(&mut parts1, &config)
        .await
        .expect("Valid subdomain should succeed");
    assert_eq!(tenant1, "tenant1");

    // 2. Apex domain (e.g. example.com)
    let req2 = Request::builder()
        .header("Host", "example.com")
        .body(())
        .unwrap();
    let (mut parts2, ()) = req2.into_parts();
    let err2 = extract_tenant_from_parts(&mut parts2, &config)
        .await
        .unwrap_err();
    assert!(err2.to_string().contains("apex") || err2.to_string().contains("subdomain"));

    // 3. IP address host
    let req3 = Request::builder()
        .header("Host", "127.0.0.1:3000")
        .body(())
        .unwrap();
    let (mut parts3, ()) = req3.into_parts();
    let err3 = extract_tenant_from_parts(&mut parts3, &config)
        .await
        .unwrap_err();
    assert!(err3.to_string().contains("IP") || err3.to_string().contains("subdomain"));

    // 4. Local host (localhost:3000) - apex, rejected as it lacks subdomain
    let req4 = Request::builder()
        .header("Host", "localhost:3000")
        .body(())
        .unwrap();
    let (mut parts4, ()) = req4.into_parts();
    let err4 = extract_tenant_from_parts(&mut parts4, &config)
        .await
        .unwrap_err();
    assert!(err4.to_string().contains("local") || err4.to_string().contains("subdomain"));

    // 5. Local subdomain (tenant1.localhost) - valid
    let req5 = Request::builder()
        .header("Host", "tenant1.localhost")
        .body(())
        .unwrap();
    let (mut parts5, ()) = req5.into_parts();
    let tenant5 = extract_tenant_from_parts(&mut parts5, &config)
        .await
        .expect("subdomain of localhost is valid");
    assert_eq!(tenant5, "tenant1");
}

// Test rejection of non-subdomain hosts with custom base_domain configured
#[tokio::test]
async fn test_subdomain_mode_custom_base_domain() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "subdomain".to_string();
    config.tenancy.base_domain = Some("mycompany.co.uk".to_string());

    // 1. Valid subdomain matching base domain
    let req1 = Request::builder()
        .header("Host", "tenant1.mycompany.co.uk")
        .body(())
        .unwrap();
    let (mut parts1, ()) = req1.into_parts();
    let tenant1 = extract_tenant_from_parts(&mut parts1, &config)
        .await
        .expect("Valid matching subdomain should succeed");
    assert_eq!(tenant1, "tenant1");

    // 2. Base domain itself (apex)
    let req2 = Request::builder()
        .header("Host", "mycompany.co.uk")
        .body(())
        .unwrap();
    let (mut parts2, ()) = req2.into_parts();
    let err2 = extract_tenant_from_parts(&mut parts2, &config)
        .await
        .unwrap_err();
    assert!(err2.to_string().contains("apex") || err2.to_string().contains("subdomain"));

    // 3. Different domain entirely
    let req3 = Request::builder()
        .header("Host", "tenant1.otherdomain.com")
        .body(())
        .unwrap();
    let (mut parts3, ()) = req3.into_parts();
    let err3 = extract_tenant_from_parts(&mut parts3, &config)
        .await
        .unwrap_err();
    assert!(err3.to_string().contains("domain") || err3.to_string().contains("subdomain"));
}

// ── New TDD tests for issue #695 ──────────────────────────────────────────

/// A mixed-case host like `Tenant1.Example.COM` should match `base_domain`
/// `"example.com"` and return tenant `"tenant1"` (lowercased).
#[tokio::test]
async fn mixed_case_host_matches_base_domain() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "subdomain".to_string();
    config.tenancy.base_domain = Some("example.com".to_string());

    let req = Request::builder()
        .header("Host", "Tenant1.Example.COM")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let tenant = extract_tenant_from_parts(&mut parts, &config)
        .await
        .expect("mixed-case host should match case-insensitively");
    assert_eq!(tenant, "tenant1");
}

/// The apex domain `EXAMPLE.COM` with `base_domain` `"example.com"` should be
/// rejected (apex not allowed), not succeed.
#[tokio::test]
async fn apex_mixed_case_rejected() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "subdomain".to_string();
    config.tenancy.base_domain = Some("example.com".to_string());

    let req = Request::builder()
        .header("Host", "EXAMPLE.COM")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let err = extract_tenant_from_parts(&mut parts, &config)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("apex") || err.to_string().contains("subdomain"),
        "Expected apex rejection, got: {err}"
    );
}

/// When `jwt_audience` is configured and the JWT carries a matching `aud`
/// claim, extraction should succeed.
#[tokio::test]
async fn jwt_audience_valid_passes() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_string();
    config.tenancy.jwt_claim = "company".to_string();
    config.tenancy.jwt_secret = Some("secret".to_string());
    config.tenancy.jwt_audience = Some("my-api".to_string());

    let token = generate_jwt_with_audience("acme", "secret", Some("my-api"));
    let req = Request::builder()
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let tenant = extract_tenant_from_parts(&mut parts, &config)
        .await
        .expect("JWT with matching audience should succeed");
    assert_eq!(tenant, "acme");
}

/// When `jwt_audience` is configured and the JWT carries a *different* `aud`
/// claim, extraction must fail with a 401-style error.
#[tokio::test]
async fn jwt_audience_mismatch_fails() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_string();
    config.tenancy.jwt_claim = "company".to_string();
    config.tenancy.jwt_secret = Some("secret".to_string());
    config.tenancy.jwt_audience = Some("my-api".to_string());

    // Token carries audience "wrong-api" — should be rejected
    let token = generate_jwt_with_audience("acme", "secret", Some("wrong-api"));
    let req = Request::builder()
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let err = extract_tenant_from_parts(&mut parts, &config)
        .await
        .unwrap_err();
    let err_str = err.to_string().to_lowercase();
    assert!(
        err_str.contains("audience") || err_str.contains("unauthorized"),
        "Expected audience rejection, got: {err}"
    );
}

/// When `jwt_audience` is configured but the JWT has **no** `aud` claim at all
/// (e.g. a legacy token issuer), extraction must fail. This covers the case
/// where the token simply omits the audience field rather than providing the
/// wrong value.
#[tokio::test]
async fn jwt_audience_missing_claim_fails() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_string();
    config.tenancy.jwt_claim = "company".to_string();
    config.tenancy.jwt_secret = Some("secret".to_string());
    config.tenancy.jwt_audience = Some("my-api".to_string());

    // Token has aud: None — skip_serializing_if means the field is absent
    let token = generate_jwt_with_audience("acme", "secret", None);
    let req = Request::builder()
        .header("Authorization", format!("Bearer {token}"))
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let err = extract_tenant_from_parts(&mut parts, &config)
        .await
        .unwrap_err();
    let err_str = err.to_string().to_lowercase();
    assert!(
        err_str.contains("audience") || err_str.contains("unauthorized"),
        "Expected audience rejection for missing aud claim, got: {err}"
    );
}

/// Slicing Authorization at index 7 when index 7 is inside a multi-byte UTF-8 char
/// (like `é`) must not panic, but must return a clean unauthorized error.
#[tokio::test]
async fn jwt_malformed_bearer_boundary_handling_does_not_panic() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "jwt".to_string();
    config.tenancy.jwt_claim = "company".to_string();
    config.tenancy.jwt_secret = Some("secret".to_string());

    let req = Request::builder()
        .header("Authorization", "aaaaaaé...")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let err = extract_tenant_from_parts(&mut parts, &config)
        .await
        .unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("Invalid Authorization header format")
            || err_str.contains("Invalid UTF-8"),
        "Expected clean unauthorized error, got: {err_str}"
    );
}

/// When polling a response body wrapped in `TenantPropagatingBody`, the
/// `CURRENT_TENANT` task-local context must be re-established for the duration
/// of the poll.
#[tokio::test]
async fn tenant_propagating_body_sets_context_during_poll() {
    use autumn_web::tenancy::{CURRENT_TENANT, TenantPropagatingBody};
    use bytes::Bytes;
    use http_body::{Body as HttpBody, Frame};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // A body that checks CURRENT_TENANT inside poll_frame
    struct ContextCheckBody {
        done: bool,
    }
    impl HttpBody for ContextCheckBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if self.done {
                return Poll::Ready(None);
            }
            self.done = true;
            let tenant = CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
            assert_eq!(
                tenant.as_deref(),
                Some("acme"),
                "tenant must be visible during poll_frame"
            );
            Poll::Ready(Some(Ok(Frame::data(Bytes::from("data")))))
        }
    }

    let body = ContextCheckBody { done: false };
    let mut wrapped = TenantPropagatingBody {
        inner: body,
        tenant_id: "acme".to_string(),
    };
    // Poll it outside any CURRENT_TENANT scope (which would fail without the wrapper)
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let res = Pin::new(&mut wrapped).poll_frame(&mut cx);
    assert!(res.is_ready());
}
