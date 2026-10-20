//! Warden: an outbound URL's userinfo credential must not reach a capsule.
//!
//! Attacker: anyone who later reads a capsule off disk (a support engineer, a
//! CI artifact, a shared bug report). App shape: an app that follows
//! `docs/guide/failure-capsules.md` — capture on, `[log] filter_parameters`
//! default — and calls a downstream service whose credential travels as URL
//! userinfo (`https://user:secret@host/..`), the same credential an
//! `Authorization: Basic` header would carry and which the capsule masks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use autumn_web::capsule::{DivergenceLog, ReplayFixtures, Verdict, execute, load_capsule};
use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

const SENTINEL_USER: &str = "warden-userinfo-user-sentinel";
const SENTINEL_PASS: &str = "warden-userinfo-pass-sentinel";

#[get("/userinfo-charge")]
async fn userinfo_charge(State(state): State<AppState>) -> Result<&'static str, AutumnError> {
    let client = autumn_web::http_client::Client::from_state(&state).named("payments");
    let _ = client
        .get(format!(
            "https://{SENTINEL_USER}:{SENTINEL_PASS}@payments.example/charge"
        ))
        .send()
        .await;
    Err(AutumnError::internal_server_error_msg("charge failed"))
}

fn capture_config(dir: &Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    config.failure_capture.enabled = true;
    config.failure_capture.dir = dir.to_string_lossy().into_owned();
    config
}

async fn await_one_capsule(dir: &Path) -> PathBuf {
    for _ in 0..200 {
        if let Ok(entries) = std::fs::read_dir(dir)
            && let Some(path) = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|ext| ext == "json"))
        {
            return path;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no capsule was written to {}", dir.display());
}

#[tokio::test]
async fn outbound_url_userinfo_credential_is_masked_in_the_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![userinfo_charge]);
    let _mock = app
        .http_mock("payments")
        .get("/charge")
        .respond_with(502, serde_json::json!({"error": "upstream down"}));
    let client = app.build();

    client
        .get("/userinfo-charge")
        .send()
        .await
        .assert_status(500);
    let path = await_one_capsule(dir.path()).await;
    let raw = std::fs::read_to_string(&path).expect("read capsule");
    let capsule = load_capsule(&path).expect("the capsule loads");
    assert!(
        !capsule.effects.http.is_empty(),
        "the outbound call must be on the tape"
    );
    assert!(
        !raw.contains(SENTINEL_PASS) && !raw.contains(SENTINEL_USER),
        "URL userinfo credential reached the capsule on disk: {:?}",
        capsule
            .effects
            .http
            .iter()
            .map(|e| &e.url)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_capsule_with_masked_userinfo_still_replays_from_the_tape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut app = TestApp::new()
        .config(capture_config(dir.path()))
        .routes(routes![userinfo_charge]);
    let _mock = app
        .http_mock("payments")
        .get("/charge")
        .respond_with(502, serde_json::json!({"error": "upstream down"}));
    let client = app.build();
    client
        .get("/userinfo-charge")
        .send()
        .await
        .assert_status(500);
    let path = await_one_capsule(dir.path()).await;
    let capsule = load_capsule(&path).expect("the capsule loads");

    let fixtures = ReplayFixtures::from_capsule(&capsule);
    let router = TestApp::new()
        .config(AutumnConfig {
            profile: Some("test".into()),
            ..AutumnConfig::default()
        })
        .routes(routes![userinfo_charge])
        .with_clock(fixtures.clock())
        .with_entropy(fixtures.entropy())
        .build()
        .into_router();
    let outcome = execute(router, &capsule, Arc::new(DivergenceLog::new()), &fixtures).await;
    assert!(
        outcome.effect_divergences.is_empty(),
        "the masked userinfo URL must match the live one on replay: {:?}",
        outcome.effect_divergences
    );
    assert_eq!(outcome.verdict, Verdict::Reproduced, "{outcome:?}");
}
