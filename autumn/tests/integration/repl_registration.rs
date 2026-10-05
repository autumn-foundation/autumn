//! `#[model]` and `#[repository]` register with the console REPL (issue #2148).
//!
//! Real macros, no hand-written glue. The Docker test reads a live table.

#![cfg(feature = "repl")]

use std::time::Duration;

use autumn_web::config::DatabaseConfig;
use autumn_web::repl::{self, Bridge, Outcome, Repl, ReplRow};
use autumn_web::{model, repository};

diesel::table! {
    repl_posts (id) {
        id -> Int8,
        title -> Text,
        secret_note -> Text,
    }
}

#[model(table = "repl_posts")]
pub struct ReplPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[private]
    pub secret_note: String,
}

#[repository(ReplPost, table = "repl_posts")]
pub trait ReplPostRepository {}

diesel::table! {
    repl_people (id) {
        id -> Int8,
        name -> Text,
        email -> Text,
    }
}

#[model(table = "repl_people")]
pub struct ReplPerson {
    #[id]
    pub id: i64,
    pub name: String,
    #[classified]
    pub email: String,
}

#[repository(ReplPerson, table = "repl_people")]
pub trait ReplPersonRepository {}

fn model(name: &str) -> &'static repl::ReplModel {
    repl::registered_models()
        .into_iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("model {name} is not registered"))
}

#[test]
fn model_registers_its_name_table_and_visible_fields() {
    let post = model("ReplPost");
    assert_eq!(post.table, "repl_posts");
    assert_eq!(post.fields, &["id", "title"], "#[private] is not shown");
}

#[test]
fn repository_registers_with_its_model() {
    let repository = repl::registered_repositories()
        .into_iter()
        .find(|r| r.name == "ReplPostRepository")
        .expect("ReplPostRepository is registered");
    assert_eq!(repository.model, "ReplPost");
}

#[test]
fn a_row_shows_its_json_projection() {
    let post = ReplPost {
        id: 1,
        title: "Hello".into(),
        secret_note: "hidden".into(),
    };
    assert_eq!(
        post.to_repl_value(),
        Ok(serde_json::json!({ "id": 1, "title": "Hello" }))
    );
}

#[test]
fn a_classified_column_never_reaches_the_prompt() {
    let person = ReplPerson {
        id: 7,
        name: "Ada".into(),
        email: "ada@example.com".to_string().into(),
    };
    let value = person.to_repl_value().expect("projection");
    assert_eq!(value, serde_json::json!({ "id": 7, "name": "Ada" }));
    assert!(!value.to_string().contains("ada@example.com"));
    assert_eq!(model("ReplPerson").fields, &["id", "name"]);
}

/// A pool to a closed port: every call fails fast.
fn unreachable_pool() -> repl::ReplPool {
    autumn_web::db::create_pool(&DatabaseConfig {
        primary_url: Some("postgres://autumn:autumn@127.0.0.1:1/none".into()),
        connect_timeout_secs: 2,
        ..DatabaseConfig::default()
    })
    .expect("pool builds")
    .expect("url present => Some(pool)")
}

#[test]
fn an_unreachable_database_is_a_script_error() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let bridge =
        Bridge::new(rt.handle().clone(), unreachable_pool()).with_timeout(Duration::from_secs(10));
    let mut repl = Repl::new(bridge);
    for line in [
        "ReplPostRepository::count()",
        "ReplPostRepository::find_all()",
        "ReplPostRepository::find_by_id(1)",
    ] {
        assert!(
            matches!(repl.eval_line(line), Outcome::Error(_)),
            "{line} must be a script error"
        );
    }
}

#[cfg(feature = "test-support")]
mod docker {
    use super::*;
    use autumn_web::reexports::diesel_async::RunQueryDsl;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    const SETUP: &str = "
        CREATE TABLE repl_posts (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, secret_note TEXT NOT NULL);
        INSERT INTO repl_posts (title, secret_note) VALUES ('First', 's1'), ('Second', 's2');
    ";

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires Docker (testcontainers)"]
    async fn repl_reads_a_live_table() {
        let container = Postgres::default().start().await.expect("start Postgres");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let pool = autumn_web::db::create_pool(&DatabaseConfig {
            primary_url: Some(format!(
                "postgres://postgres:postgres@{host}:{port}/postgres"
            )),
            ..DatabaseConfig::default()
        })
        .expect("pool builds")
        .expect("pool");
        {
            let mut conn = pool.get().await.expect("connection");
            for statement in SETUP.split(';').filter(|s| !s.trim().is_empty()) {
                diesel::sql_query(statement)
                    .execute(&mut *conn)
                    .await
                    .expect("setup");
            }
        }

        let handle = tokio::runtime::Handle::current();
        let lines = tokio::task::spawn_blocking(move || {
            let mut repl = Repl::new(Bridge::new(handle, pool));
            [
                "ReplPostRepository::count()",
                "ReplPostRepository::find_by_id(2).title",
                "ReplPostRepository::find_by_id(99)",
                "ReplPostRepository::find_all().len()",
                "ReplPostRepository::find_all()",
            ]
            .map(|line| repl.eval_line(line))
        })
        .await
        .expect("prompt thread");

        assert_eq!(lines[0], Outcome::Value("2".into()));
        assert_eq!(lines[1], Outcome::Value("Second".into()));
        assert_eq!(lines[2], Outcome::Value("()".into()));
        assert_eq!(lines[3], Outcome::Value("2".into()));
        let Outcome::Value(all) = &lines[4] else {
            panic!("find_all: {:?}", lines[4]);
        };
        assert!(all.contains("\"First\"") && !all.contains("s1"), "{all}");
    }
}
