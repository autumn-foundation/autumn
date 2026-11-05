//! `autumn db grants` and the doctor role check on a real database (#3550).
//!
//! **Requires Docker.** Postgres gets three tables in two schemas and two
//! boundary roles. The test prints and applies the grants, connects as each
//! role, and runs `autumn doctor --strict`. A stray grant makes doctor fail.
//! Applying the grants again removes it.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const BOUNDARIES: &str = r#"
[boundaries.core]
owns = ["contacts", "deals"]
role = "core"

[boundaries.marketing]
owns = ["campaigns"]
schema = "marketing"
role = "marketing"
may_read = ["contacts"]
"#;

/// `marketing` connects with `user` on the primary URL. Its password comes
/// from the env, as it would in production.
const AUTUMN_TOML: &str = r#"
[database.roles.core]
user = "core"
password = "core-pw"

[database.roles.marketing]
user = "marketing"
"#;

fn autumn(dir: &Path, args: &[&str], url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(dir)
        .env("AUTUMN_DATABASE__URL", url)
        .env(
            "AUTUMN_DATABASE__ROLES__MARKETING__PASSWORD",
            "marketing-pw",
        )
        .env_remove("AUTUMN_ENV")
        .env_remove("AUTUMN_PROFILE")
        .output()
        .expect("run autumn")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"crm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .expect("Cargo.toml");
    fs::create_dir_all(root.join("src")).expect("src");
    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("main.rs");
    fs::write(root.join("boundaries.toml"), BOUNDARIES).expect("boundaries.toml");
    fs::write(root.join("autumn.toml"), AUTUMN_TOML).expect("autumn.toml");
    dir
}

async fn start_postgres() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    String,
) {
    use testcontainers::runners::AsyncRunner as _;
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("start Postgres — is Docker running?");
    let host = container.get_host().await.expect("host").to_string();
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        container,
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        format!("{host}:{port}"),
    )
}

async fn connect(url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .expect("connect");
    tokio::spawn(connection);
    client
}

/// The SQLSTATE of a read, or `None` when it ran.
async fn read(client: &tokio_postgres::Client, table: &str) -> Option<String> {
    client
        .batch_execute(&format!("SELECT 1 FROM {table} LIMIT 0"))
        .await
        .err()
        .map(|e| {
            e.code()
                .map_or_else(|| e.to_string(), |c| c.code().to_owned())
        })
}

/// The `database_roles` entry of `autumn doctor --strict --json`.
fn doctor_roles(dir: &Path, url: &str) -> serde_json::Value {
    let output = autumn(dir, &["doctor", "--strict", "--json"], url);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "doctor JSON: {e}\nstdout: {}\nstderr: {}",
            text(&output.stdout),
            text(&output.stderr)
        )
    });
    json["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "database_roles")
        .cloned()
        .unwrap_or_else(|| panic!("no database_roles check: {json}"))
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn grants_are_generated_applied_and_checked_by_doctor() {
    let (_container, url, addr) = start_postgres().await;
    let owner = connect(&url).await;
    owner
        .batch_execute(
            "CREATE ROLE core LOGIN PASSWORD 'core-pw';
             CREATE ROLE marketing LOGIN PASSWORD 'marketing-pw';
             CREATE TABLE contacts (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL);
             CREATE TABLE deals (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);
             CREATE SCHEMA marketing;
             CREATE TABLE marketing.campaigns (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);",
        )
        .await
        .expect("schema");
    let dir = project();
    let root = dir.path();

    // Print: the SQL names each table, and changes nothing.
    let output = autumn(root, &["db", "grants"], &url);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sql = text(&output.stdout);
    for line in [
        r#"GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "marketing"."campaigns" TO "marketing";"#,
        r#"GRANT SELECT ON TABLE "public"."contacts" TO "marketing";"#,
        r#"GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "public"."contacts", "public"."deals" TO "core";"#,
        r#"ALTER ROLE "marketing" SET search_path = "marketing", "public";"#,
    ] {
        assert!(sql.contains(line), "missing {line}\n{sql}");
    }
    let marketing_url = format!("postgres://marketing:marketing-pw@{addr}/postgres");
    let before = connect(&marketing_url).await;
    assert_eq!(
        read(&before, "marketing.campaigns").await.as_deref(),
        Some("42501")
    );
    drop(before);

    // Apply.
    let output = autumn(root, &["db", "grants", "--apply"], &url);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stderr).contains("Applied the grants of 2 role(s)"));

    // Each role reads its grant and is denied the rest.
    let marketing = connect(&marketing_url).await;
    assert_eq!(read(&marketing, "campaigns").await, None, "search_path");
    assert_eq!(read(&marketing, "public.contacts").await, None, "may_read");
    assert_eq!(
        read(&marketing, "public.deals").await.as_deref(),
        Some("42501")
    );
    marketing
        .batch_execute("INSERT INTO marketing.campaigns (name) VALUES ('fall')")
        .await
        .expect("marketing writes its own table and sequence");
    let core = connect(&format!("postgres://core:core-pw@{addr}/postgres")).await;
    assert_eq!(read(&core, "public.deals").await, None);
    assert_eq!(
        read(&core, "marketing.campaigns").await.as_deref(),
        Some("42501")
    );

    // Doctor: the negative check passes for each role.
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "pass", "{check}");
    let detail = check["detail"].as_str().expect("detail");
    assert!(
        detail
            .contains("marketing: 2 of 2 granted table(s) readable; 1 of 1 other table(s) denied"),
        "{detail}"
    );
    assert!(
        detail.contains("core: 2 of 2 granted table(s) readable; 1 of 1 other table(s) denied"),
        "{detail}"
    );

    // A grant outside the config: doctor fails and names it.
    owner
        .batch_execute("GRANT SELECT ON deals TO marketing")
        .await
        .expect("stray grant");
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        check["detail"]
            .as_str()
            .expect("detail")
            .contains("marketing: reads 1 table(s) outside its grant: public.deals"),
        "{check}"
    );

    // Apply again: the REVOKE removes it.
    let output = autumn(root, &["db", "grants", "--apply"], &url);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        read(&marketing, "public.deals").await.as_deref(),
        Some("42501")
    );
    assert_eq!(doctor_roles(root, &url)["status"], "pass");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn apply_refuses_a_missing_role() {
    let (_container, url, _) = start_postgres().await;
    connect(&url)
        .await
        .batch_execute("CREATE TABLE contacts (id BIGSERIAL PRIMARY KEY)")
        .await
        .expect("schema");
    let dir = project();
    let output = autumn(dir.path(), &["db", "grants", "--apply"], &url);
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("has no role core, marketing"), "{stderr}");
    assert!(stderr.contains(r#"CREATE ROLE "core" LOGIN"#), "{stderr}");
}

#[test]
fn no_role_in_the_config_needs_no_database() {
    let dir = project();
    fs::write(
        dir.path().join("boundaries.toml"),
        "[boundaries.core]\nowns = [\"contacts\"]\n",
    )
    .expect("boundaries.toml");
    let output = autumn(
        dir.path(),
        &["db", "grants"],
        "postgres://nobody@127.0.0.1:1/none",
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).contains("declares a `role`"));
}

#[test]
fn an_unreachable_database_fails_with_a_message() {
    let dir = project();
    let output = autumn(
        dir.path(),
        &["db", "grants"],
        "postgres://nobody@127.0.0.1:1/none",
    );
    assert!(!output.status.success());
    assert!(
        text(&output.stderr).contains("autumn db grants:"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn a_missing_config_fails_with_a_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = autumn(
        dir.path(),
        &["db", "grants"],
        "postgres://nobody@127.0.0.1:1/none",
    );
    assert!(!output.status.success());
    assert!(text(&output.stderr).contains("boundaries.toml"));
}
