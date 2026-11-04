//! Named roles (`[roles.<name>]`, issue #3543): handlers, `on_startup` hooks
//! and plugins read the role name through `state.role()`.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use autumn_web::app::AppBuilder;
use autumn_web::config::{AutumnConfig, ProcessRole};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

const ROLES: &str = r#"
role = "marketing"

[roles.marketing]
base = "combined"
scheduler = false
"#;

fn marketing_config() -> AutumnConfig {
    toml::from_str(ROLES).expect("roles config parses")
}

#[get("/role")]
async fn role_name(State(state): State<AppState>) -> String {
    let role = state.role();
    format!(
        "{} base={} scheduler={}",
        role.name(),
        role.base().as_str(),
        state.capabilities().scheduler
    )
}

/// Records the role name an `on_startup` hook sees.
struct RoleProbe(Arc<Mutex<Option<String>>>);

impl Plugin for RoleProbe {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("named-role-probe")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let seen = self.0;
        app.on_startup(move |state| {
            let seen = Arc::clone(&seen);
            async move {
                *seen.lock().expect("lock") = Some(state.role().name().to_owned());
                Ok(())
            }
        })
    }
}

#[tokio::test]
async fn handler_reads_the_named_role() {
    let client = TestApp::new()
        .config(marketing_config())
        .routes(routes![role_name])
        .build();
    client
        .get("/role")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("marketing base=combined scheduler=false");
}

#[tokio::test]
async fn plugin_on_startup_reads_the_named_role() {
    let seen = Arc::new(Mutex::new(None));
    let _client = TestApp::new()
        .config(marketing_config())
        .plugin(RoleProbe(Arc::clone(&seen)))
        .routes(routes![role_name])
        .build();
    assert_eq!(seen.lock().expect("lock").as_deref(), Some("marketing"));
}

#[tokio::test]
async fn state_initializer_reads_the_named_role() {
    let seen = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);
    let _client = TestApp::new()
        .config(marketing_config())
        .state_initializer(move |state| {
            *sink.lock().expect("lock") =
                Some((state.role().name().to_owned(), state.role().base()));
        })
        .routes(routes![role_name])
        .build();
    assert_eq!(
        seen.lock().expect("lock").clone(),
        Some(("marketing".to_owned(), ProcessRole::Combined))
    );
}

/// A built-in role keeps its name.
#[tokio::test]
async fn builtin_role_name_is_its_base() {
    let client = TestApp::new().routes(routes![role_name]).build();
    client
        .get("/role")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("combined base=combined scheduler=true");
}
