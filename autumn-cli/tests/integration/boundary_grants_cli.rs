//! `autumn db grants` and the doctor role check on a real database.
//!
//! **Requires Docker.** Postgres 16 gets three tables in two schemas and two
//! boundary roles. The test prints and applies the grants, connects as each
//! role, and runs `autumn doctor --strict`. A stray grant makes doctor fail.
//! Applying the grants again removes it. A grant to `PUBLIC` stays, and
//! doctor still finds it.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// The marketing role is `mkt`, not `marketing`. The default `search_path`
/// of a role starts with `"$user"`, so a role named like its schema finds a
/// bare table name with no grant help.
const BOUNDARIES: &str = r#"
[boundaries.core]
owns = ["contacts", "deals"]
role = "core"

[boundaries.marketing]
owns = ["campaigns"]
schema = "marketing"
role = "mkt"
may_read = ["contacts"]
"#;

/// `marketing` connects with `user` on the primary URL. Its password comes
/// from the env, as it would in production.
const AUTUMN_TOML: &str = r#"
[database.roles.core]
user = "core"
password = "core-pw"

[database.roles.marketing]
user = "mkt"
"#;

fn autumn(dir: &Path, args: &[&str], url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_autumn"))
        .args(args)
        .current_dir(dir)
        .env("AUTUMN_DATABASE__URL", url)
        .env("AUTUMN_DATABASE__ROLES__MARKETING__PASSWORD", "mkt-pw")
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
    use testcontainers::ImageExt as _;
    use testcontainers::runners::AsyncRunner as _;
    // Postgres 15 changed the privileges of the `public` schema.
    let container = testcontainers_modules::postgres::Postgres::default()
        .with_tag("16-alpine")
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

/// The doctor line for a right on `contacts` that the grant does not give.
const EXCEEDS_CONTACTS: &str = "has a right that the grant does not give on 1 \
                                `may_read`/`may_write` table(s): public.contacts";

fn detail(check: &serde_json::Value) -> &str {
    check["detail"].as_str().expect("detail")
}

fn apply(root: &Path, url: &str) {
    let output = autumn(root, &["db", "grants", "--apply"], url);
    assert!(output.status.success(), "{}", text(&output.stderr));
}

/// Two roles, three tables in two schemas, and a project that declares them.
async fn boundary_database() -> (
    testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
    String,
    String,
    tokio_postgres::Client,
    tempfile::TempDir,
) {
    let (container, url, addr) = start_postgres().await;
    let owner = connect(&url).await;
    owner
        .batch_execute(
            "CREATE ROLE core LOGIN PASSWORD 'core-pw';
             CREATE ROLE mkt LOGIN PASSWORD 'mkt-pw';
             CREATE TABLE contacts (id BIGSERIAL PRIMARY KEY, email TEXT NOT NULL);
             CREATE TABLE deals (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL);
             CREATE SCHEMA marketing;
             CREATE TABLE marketing.campaigns (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);",
        )
        .await
        .expect("schema");
    (container, url, addr, owner, project())
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn grants_are_generated_applied_and_checked_by_doctor() {
    let (_container, url, addr, _owner, dir) = boundary_database().await;
    let root = dir.path();

    // Print: the SQL names each table, and changes nothing.
    let output = autumn(root, &["db", "grants"], &url);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let sql = text(&output.stdout);
    for line in [
        r#"GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "marketing"."campaigns" TO "mkt";"#,
        r#"GRANT SELECT ON TABLE "public"."contacts" TO "mkt";"#,
        r#"GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "public"."contacts", "public"."deals" TO "core";"#,
        r#"ALTER ROLE "mkt" SET search_path = "marketing", "public";"#,
    ] {
        assert!(sql.contains(line), "missing {line}\n{sql}");
    }
    let mkt_url = format!("postgres://mkt:mkt-pw@{addr}/postgres");
    let before = connect(&mkt_url).await;
    assert_eq!(
        read(&before, "marketing.campaigns").await.as_deref(),
        Some("42501")
    );
    drop(before);

    // Apply.
    let output = autumn(root, &["db", "grants", "--apply"], &url);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stderr).contains("Applied the grants of 2 role(s)"));

    // Each role reads its grant and is denied the rest. The bare name
    // resolves only through the `search_path` of the grants.
    let mkt = connect(&mkt_url).await;
    assert_eq!(read(&mkt, "campaigns").await, None, "search_path");
    assert_eq!(read(&mkt, "public.contacts").await, None, "may_read");
    assert_eq!(read(&mkt, "public.deals").await.as_deref(), Some("42501"));
    mkt.batch_execute("INSERT INTO campaigns (name) VALUES ('fall')")
        .await
        .expect("mkt writes its own table and sequence");
    let core = connect(&format!("postgres://core:core-pw@{addr}/postgres")).await;
    assert_eq!(read(&core, "public.deals").await, None);
    assert_eq!(
        read(&core, "marketing.campaigns").await.as_deref(),
        Some("42501")
    );

    // Doctor: the negative check passes for each role.
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "pass", "{check}");
    for line in [
        "marketing: 2 of 2 granted table(s) readable; 1 of 1 other table(s) denied",
        "core: 2 of 2 granted table(s) readable; 1 of 1 other table(s) denied",
    ] {
        assert!(detail(&check).contains(line), "{check}");
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn doctor_finds_grants_outside_the_config() {
    let (_container, url, addr, owner, dir) = boundary_database().await;
    let root = dir.path();
    apply(root, &url);
    assert_eq!(doctor_roles(root, &url)["status"], "pass");
    let mkt = connect(&format!("postgres://mkt:mkt-pw@{addr}/postgres")).await;

    // A grant outside the config: doctor fails and names it. An apply
    // removes it.
    owner
        .batch_execute("GRANT SELECT ON deals TO mkt")
        .await
        .expect("stray grant");
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        detail(&check)
            .contains("marketing: reads or changes 1 table(s) outside its grant: public.deals"),
        "{check}"
    );
    apply(root, &url);
    assert_eq!(read(&mkt, "public.deals").await.as_deref(), Some("42501"));
    assert_eq!(doctor_roles(root, &url)["status"], "pass");

    // A write privilege only, and a write on a `may_read` table.
    owner
        .batch_execute("GRANT INSERT ON deals TO mkt; GRANT UPDATE ON contacts TO mkt")
        .await
        .expect("stray write grants");
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        detail(&check).contains("reads or changes 1 table(s) outside its grant: public.deals"),
        "{check}"
    );
    assert!(detail(&check).contains(EXCEEDS_CONTACTS), "{check}");
    apply(root, &url);
    assert_eq!(doctor_roles(root, &url)["status"], "pass");

    // A column grant: `has_table_privilege` does not show it. An apply
    // removes it.
    owner
        .batch_execute("GRANT UPDATE (email) ON contacts TO mkt")
        .await
        .expect("column grant");
    let check = doctor_roles(root, &url);
    assert!(detail(&check).contains(EXCEEDS_CONTACTS), "{check}");
    apply(root, &url);
    assert_eq!(doctor_roles(root, &url)["status"], "pass");

    // `TRIGGER` on a `may_read` table: a trigger can change the writes of
    // the owner boundary. Doctor finds it through `PUBLIC` too.
    owner
        .batch_execute("GRANT TRIGGER ON contacts TO PUBLIC")
        .await
        .expect("TRIGGER grant");
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(detail(&check).contains(EXCEEDS_CONTACTS), "{check}");
    owner
        .batch_execute("REVOKE TRIGGER ON contacts FROM PUBLIC")
        .await
        .expect("revoke TRIGGER");
    assert_eq!(doctor_roles(root, &url)["status"], "pass");

    // A grant to PUBLIC: the REVOKE of a role cannot remove it. Doctor still
    // finds it after an apply.
    owner
        .batch_execute("GRANT SELECT ON deals TO PUBLIC")
        .await
        .expect("PUBLIC grant");
    apply(root, &url);
    let check = doctor_roles(root, &url);
    assert_eq!(check["status"], "fail", "{check}");
    assert!(
        detail(&check)
            .contains("marketing: reads or changes 1 table(s) outside its grant: public.deals"),
        "{check}"
    );
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
    assert!(stderr.contains("has no role core, mkt"), "{stderr}");
    assert!(stderr.contains(r#"CREATE ROLE "core" LOGIN"#), "{stderr}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers); run with -- --ignored"]
async fn apply_refuses_a_role_that_owns_tables() {
    let (_container, url, _) = start_postgres().await;
    connect(&url)
        .await
        .batch_execute(
            "CREATE ROLE core LOGIN; CREATE ROLE mkt LOGIN;
             CREATE TABLE contacts (id BIGSERIAL PRIMARY KEY);
             ALTER TABLE contacts OWNER TO core;",
        )
        .await
        .expect("schema");
    let dir = project();
    let output = autumn(dir.path(), &["db", "grants", "--apply"], &url);
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("`core` is the owner of tables"), "{stderr}");
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
