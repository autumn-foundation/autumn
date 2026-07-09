use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::time::TickingClock;
use chrono::{TimeZone, Utc};
use std::time::Duration;

#[get("/api/info", api_version = "v1")]
async fn api_info_v1() -> &'static str {
    "v1 info"
}

#[get("/api/info-opt", api_version = "v1", sunset_opt_out = true)]
async fn api_info_v1_opt() -> &'static str {
    "v1 info opt"
}

#[get(
    "/api/sunset-only-opt",
    api_version = "v_sunset_only",
    sunset_opt_out = true
)]
async fn api_sunset_only_opt() -> &'static str {
    "sunset only opt"
}

#[get("/api/plain")]
async fn api_plain() -> &'static str {
    "plain info"
}

#[tokio::test]
async fn test_api_versioning_integration() {
    let start_time = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    let deprecated_at = Utc.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap();
    let sunset_at = Utc.with_ymd_and_hms(2026, 6, 3, 0, 0, 0).unwrap();

    let clock = TickingClock::starting_at(start_time);

    let client = TestApp::new()
        .routes(routes![
            api_info_v1,
            api_info_v1_opt,
            api_plain,
            api_sunset_only_opt
        ])
        .api_version(autumn_web::app::ApiVersion {
            version: "v1".to_string(),
            deprecated_at: Some(deprecated_at),
            sunset_at: Some(sunset_at),
        })
        .api_version(autumn_web::app::ApiVersion {
            version: "v_sunset_only".to_string(),
            deprecated_at: None,
            sunset_at: Some(sunset_at),
        })
        .with_clock(clock.clone())
        .build();

    // 1. Untagged route behaves as normal
    let resp = client.get("/api/plain").send().await;
    resp.assert_ok();
    assert!(resp.header("Deprecation").is_none());
    assert!(resp.header("Sunset").is_none());

    // 2. Tagged route behaves normally before deprecation
    let resp = client.get("/api/info").send().await;
    resp.assert_ok();
    assert!(resp.header("Deprecation").is_none());
    assert!(resp.header("Sunset").is_none());

    // 3. Move past deprecation: Deprecation: structured date, Sunset: date headers are emitted
    client.advance_clock(Duration::from_secs(24 * 3600 + 10)); // past deprecation (June 2)
    let resp = client.get("/api/info").send().await;
    resp.assert_ok();
    let expected_deprecation_header = format!("@{}", deprecated_at.timestamp());
    assert_eq!(
        resp.header("Deprecation").unwrap(),
        expected_deprecation_header
    );
    let expected_sunset_header = sunset_at.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    assert_eq!(resp.header("Sunset").unwrap(), expected_sunset_header);

    // 4. Move past sunset: should return 410 Gone with Sunset header
    client.advance_clock(Duration::from_secs(24 * 3600)); // past sunset (June 3)
    let resp = client.get("/api/info").send().await;
    resp.assert_status(410);
    assert_eq!(resp.header("Sunset").unwrap(), expected_sunset_header);
    assert_eq!(
        resp.header("Deprecation").unwrap(),
        expected_deprecation_header
    );

    let body = resp.text();
    assert!(body.contains("autumn.gone"));

    // 5. Opt-out route past sunset bypasses 410 Gone but still emits headers
    let resp = client.get("/api/info-opt").send().await;
    resp.assert_ok();
    assert_eq!(
        resp.header("Deprecation").unwrap(),
        expected_deprecation_header
    );
    assert_eq!(resp.header("Sunset").unwrap(), expected_sunset_header);

    // 6. Opt-out route past sunset (with no deprecation_at configured) still emits Deprecation header
    let resp = client.get("/api/sunset-only-opt").send().await;
    resp.assert_ok();
    let expected_sunset_deprecation_header = format!("@{}", sunset_at.timestamp());
    assert_eq!(
        resp.header("Deprecation").unwrap(),
        expected_sunset_deprecation_header
    );
    assert_eq!(resp.header("Sunset").unwrap(), expected_sunset_header);
}

#[get("/api/secured-sunset", api_version = "v1")]
#[secured]
async fn api_secured_sunset() -> &'static str {
    "secured sunset info"
}

#[get("/api/secured-role-sunset", api_version = "v1")]
#[secured("admin")]
async fn api_secured_role_sunset() -> &'static str {
    "secured role sunset info"
}

#[derive(Debug, Clone, PartialEq)]
struct SunsetNote {
    id: i64,
}

#[derive(Default, Clone)]
struct SunsetNotePolicy;

impl ::autumn_web::authorization::Policy<SunsetNote> for SunsetNotePolicy {
    fn can_show<'a>(
        &'a self,
        ctx: &'a ::autumn_web::authorization::PolicyContext,
        _note: &'a SunsetNote,
    ) -> ::autumn_web::authorization::BoxFuture<'a, bool> {
        Box::pin(async move { ctx.is_authenticated() })
    }
}

struct LoadedSunsetNote(SunsetNote);

impl<S> ::autumn_web::reexports::axum::extract::FromRequestParts<S> for LoadedSunsetNote
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        _parts: &mut ::autumn_web::reexports::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(SunsetNote { id: 1 }))
    }
}

#[get("/api/authorize-sunset", api_version = "v1")]
#[authorize("show", resource = SunsetNote)]
async fn api_authorize_sunset(LoadedSunsetNote(sunset_note): LoadedSunsetNote) -> &'static str {
    let _ = sunset_note;
    "authorized sunset info"
}

#[derive(Debug, Clone, PartialEq)]
struct SunsetPolicyDenialNote {
    id: i64,
}

#[derive(Default, Clone)]
struct SunsetPolicyDenialNotePolicy;

impl ::autumn_web::authorization::Policy<SunsetPolicyDenialNote> for SunsetPolicyDenialNotePolicy {
    fn can_show<'a>(
        &'a self,
        _ctx: &'a ::autumn_web::authorization::PolicyContext,
        record: &'a SunsetPolicyDenialNote,
    ) -> ::autumn_web::authorization::BoxFuture<'a, bool> {
        let is_ok = record.id == 42;
        Box::pin(async move { is_ok })
    }
}

struct LoadedSunsetPolicyDenialNote(SunsetPolicyDenialNote);

impl<S> ::autumn_web::reexports::axum::extract::FromRequestParts<S> for LoadedSunsetPolicyDenialNote
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut ::autumn_web::reexports::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let id = parts
            .headers
            .get("X-Note-Id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(1);
        Ok(Self(SunsetPolicyDenialNote { id }))
    }
}

#[get("/api/authorize-policy-denial-sunset", api_version = "v1")]
#[authorize("show", resource = SunsetPolicyDenialNote)]
async fn api_authorize_policy_denial_sunset(
    LoadedSunsetPolicyDenialNote(sunset_policy_denial_note): LoadedSunsetPolicyDenialNote,
) -> &'static str {
    let _ = sunset_policy_denial_note;
    "authorized policy denial sunset info"
}

#[authorize("show", resource = SunsetPolicyDenialNote)]
#[get("/api/authorize-policy-denial-reverse-sunset", api_version = "v1")]
async fn api_authorize_policy_denial_reverse_sunset(
    LoadedSunsetPolicyDenialNote(sunset_policy_denial_note): LoadedSunsetPolicyDenialNote,
) -> &'static str {
    let _ = sunset_policy_denial_note;
    "authorized policy denial reverse sunset info"
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_api_versioning_auth_preservation() {
    use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer, SessionStore};

    let start_time = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
    let deprecated_at = Utc.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap();
    let sunset_at = Utc.with_ymd_and_hms(2026, 6, 3, 0, 0, 0).unwrap();

    let clock = TickingClock::starting_at(start_time);
    let store = MemoryStore::new();

    // Seed session data
    let mut admin_data = std::collections::HashMap::new();
    admin_data.insert("user_id".to_owned(), "1".to_owned());
    admin_data.insert("role".to_owned(), "admin".to_owned());
    store.save("sess-admin", admin_data).await.unwrap();

    let mut user_data = std::collections::HashMap::new();
    user_data.insert("user_id".to_owned(), "2".to_owned());
    user_data.insert("role".to_owned(), "user".to_owned());
    store.save("sess-user", user_data).await.unwrap();

    let client = TestApp::new()
        .routes(routes![
            api_secured_sunset,
            api_secured_role_sunset,
            api_authorize_sunset,
            api_authorize_policy_denial_sunset,
            api_authorize_policy_denial_reverse_sunset
        ])
        .policy::<SunsetNote, _>(SunsetNotePolicy)
        .policy::<SunsetPolicyDenialNote, _>(SunsetPolicyDenialNotePolicy)
        .api_version(autumn_web::app::ApiVersion {
            version: "v1".to_string(),
            deprecated_at: Some(deprecated_at),
            sunset_at: Some(sunset_at),
        })
        .with_clock(clock.clone())
        .layer(SessionLayer::new(store, SessionConfig::default()))
        .build();

    // 1. Before sunset:
    // Unauthenticated -> 401
    let resp = client.get("/api/secured-sunset").send().await;
    resp.assert_status(401);

    // Authenticated user -> 200
    let resp = client
        .get("/api/secured-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .send()
        .await;
    resp.assert_ok();

    // Unauthenticated authorize -> 404 (due to default ForbiddenResponse / unauthenticated)
    // Wait, let's see. For an unauthenticated request to an #[authorize]-guarded handler,
    // authorization::authorize returns NoKeyFound/unauthorized error which becomes 404 by default.
    let resp = client.get("/api/authorize-sunset").send().await;
    assert!(resp.status == 404 || resp.status == 401);

    // Authenticated authorize -> 200
    let resp = client
        .get("/api/authorize-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .send()
        .await;
    resp.assert_ok();

    // Before sunset policy denial check
    let resp = client
        .get("/api/authorize-policy-denial-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .header("X-Note-Id", "1")
        .send()
        .await;
    resp.assert_status(404);

    let resp = client
        .get("/api/authorize-policy-denial-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .header("X-Note-Id", "42")
        .send()
        .await;
    resp.assert_ok();

    // 2. Advance clock past sunset
    client.advance_clock(Duration::from_secs(48 * 3600 + 10)); // past sunset

    // Unauthenticated request -> 401 Unauthorized (not 410 Gone!)
    let resp = client.get("/api/secured-sunset").send().await;
    resp.assert_status(401);

    // Authenticated request -> 410 Gone
    let resp = client
        .get("/api/secured-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .send()
        .await;
    resp.assert_status(410);

    // Unauthenticated role-secured request -> 401 Unauthorized (not 410 Gone!)
    let resp = client.get("/api/secured-role-sunset").send().await;
    resp.assert_status(401);

    // Authenticated request with wrong role -> 403 Forbidden (not 410 Gone!)
    let resp = client
        .get("/api/secured-role-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .send()
        .await;
    resp.assert_status(403);

    // Authenticated request with correct role -> 410 Gone
    let resp = client
        .get("/api/secured-role-sunset")
        .header("Cookie", "autumn.sid=sess-admin")
        .send()
        .await;
    resp.assert_status(410);

    // Unauthenticated authorize request -> 404/401 Unauthorized (not 410 Gone!)
    let resp = client.get("/api/authorize-sunset").send().await;
    assert!(resp.status == 404 || resp.status == 401);

    // Authenticated authorize request -> 410 Gone
    let resp = client
        .get("/api/authorize-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .send()
        .await;
    resp.assert_status(410);

    // After sunset policy denial check
    let resp = client
        .get("/api/authorize-policy-denial-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .header("X-Note-Id", "1")
        .send()
        .await;
    resp.assert_status(404); // Policy denial is preserved!

    let resp = client
        .get("/api/authorize-policy-denial-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .header("X-Note-Id", "42")
        .send()
        .await;
    resp.assert_status(410); // Authorized sunset request is 410 Gone!

    // After sunset policy denial check (reverse attribute macro order)
    let resp = client
        .get("/api/authorize-policy-denial-reverse-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .header("X-Note-Id", "1")
        .send()
        .await;
    resp.assert_status(404); // Policy denial is preserved!

    let resp = client
        .get("/api/authorize-policy-denial-reverse-sunset")
        .header("Cookie", "autumn.sid=sess-user")
        .header("X-Note-Id", "42")
        .send()
        .await;
    resp.assert_status(410); // Authorized sunset request is 410 Gone!
}
