//! Issue #1973: a `#[state_machine(lifecycle = <Enum>)]` binding can declare
//! per-edge effects at the binding site via an `effects(...)` clause — a sync
//! `on = "handler"` in-transaction effect and/or an `on_commit = <Job>`
//! after-commit enqueue — while the transition TABLE still comes from the
//! `#[lifecycle]` enum. Declaring `effects(...)` makes the model gain the
//! connection-taking `transition_{field}_to_on_conn` method, identical to the
//! inline path. This is the lifecycle companion to `model_state_machine_on.rs`.

use autumn_web::model;
use autumn_web::prelude::*;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::lifecycle;
use serde::{Deserialize, Serialize};

// The typed source of truth for the transition table.
#[lifecycle(
    initial = Draft,
    terminal(Archived),
    transitions(
        Draft -> Published,
        Published -> Archived,
    )
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Draft,
    Published,
    Archived,
}

// A `#[job]` whose args are the framework-provided transition context.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EffectArgs {
    idempotency_key: String,
}

#[job(name = "announce_publish")]
async fn announce_publish(_state: AppState, _args: EffectArgs) -> AutumnResult<()> {
    Ok(())
}

diesel::table! {
    articles (id) {
        id -> BigInt,
        status -> Text,
    }
}

#[model(table = "articles")]
pub struct Article {
    #[id]
    pub id: i64,
    // Transition table from `OrderState`; effects declared at the binding site.
    #[state_machine(lifecycle = OrderState, effects(
        Draft -> Published: on_commit = AnnouncePublishJob,
        Published -> Archived: on = "record_archive",
    ))]
    pub status: String,
}

impl Article {
    // The sync in-transaction effect runs inside the transition's transaction;
    // a returned `Err` rolls the transition back.
    async fn record_archive(&self, _conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        Ok(())
    }
}

// The pure validator is unchanged and still generated (table from the enum).
fn _assert_pure_validator_compiles(article: &Article) {
    let _ = article.can_transition_status_to("Published");
    let _ = article.transition_status_to("Published");
}

// The additive connection-taking method is generated because the binding
// declares one or more `effects(...)` edges. It takes an explicit connection so
// the sync `on` handler and the `on_commit` enqueue both run inside the caller's
// transaction.
async fn _assert_on_conn_compiles(
    article: &Article,
    conn: &mut AsyncPgConnection,
) -> AutumnResult<String> {
    article.transition_status_to_on_conn(conn, "Published").await
}

fn main() {
    let _ = _assert_pure_validator_compiles;
    let _ = _assert_on_conn_compiles;
    let _ = announce_publish;
}
