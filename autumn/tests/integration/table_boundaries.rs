//! Table-ownership boundaries on a graph from real macros (#3549).
//!
//! A fixture app with two boundaries. `bnd_core` owns `bnd_contacts`.
//! `bnd_marketing` owns `bnd_campaigns`. Marketing routes reach the core
//! table through the Diesel DSL and through a raw-SQL `JOIN`. The macros make
//! the edges, so this test proves the whole path: expansion, graph, check.

#![cfg(feature = "db")]

use autumn_web::graph::boundaries::{
    Baseline, BoundaryConfig, TableViolation, Violation, check, ratchet,
};
use autumn_web::graph::manifest::audit;
use autumn_web::graph::{Access, Provenance};

pub mod bnd_core {
    diesel::table! {
        bnd_contacts (id) {
            id -> Int8,
            name -> Text,
        }
    }

    #[autumn_web::model(table = "bnd_contacts")]
    pub struct BndContact {
        #[id]
        pub id: i64,
        pub name: String,
    }

    /// Core reads its own table. This is no violation.
    #[autumn_web::get("/bnd/contacts")]
    pub async fn bnd_list_contacts() -> &'static str {
        let _ = bnd_contacts::table;
        "ok"
    }
}

pub mod bnd_marketing {
    diesel::table! {
        bnd_campaigns (id) {
            id -> Int8,
            contact_id -> Int8,
        }
    }

    #[autumn_web::model(table = "bnd_campaigns")]
    pub struct BndCampaign {
        #[id]
        pub id: i64,
        pub contact_id: i64,
    }

    /// Reads the core table through the Diesel DSL.
    #[autumn_web::get("/bnd/report")]
    pub async fn bnd_report() -> &'static str {
        let _ = crate::integration::table_boundaries::bnd_core::bnd_contacts::table;
        "ok"
    }

    /// Reads the core table through a raw-SQL `JOIN` only.
    #[autumn_web::get("/bnd/joined")]
    pub async fn bnd_joined() -> &'static str {
        let _query = diesel::sql_query(
            "SELECT c.id FROM bnd_campaigns c JOIN bnd_contacts ct ON ct.id = c.contact_id",
        );
        "ok"
    }
}

const CONFIG: &str = r#"
[boundaries.core]
owns = ["bnd_contacts"]
modules = ["*::table_boundaries::bnd_core"]

[boundaries.marketing]
owns = ["bnd_campaigns"]
modules = ["*::table_boundaries::bnd_marketing"]
"#;

fn config(extra: &str) -> BoundaryConfig {
    BoundaryConfig::parse(&format!("{CONFIG}{extra}")).expect("valid config")
}

fn violations(config: &BoundaryConfig) -> Vec<TableViolation> {
    let graph = audit(&[], 0);
    check(config, Some(&graph), None)
        .expect("check runs")
        .violations
        .into_iter()
        .filter_map(|v| match v {
            Violation::Table(t) => Some(t),
            Violation::Crate(_) => None,
        })
        .collect()
}

fn find<'a>(found: &'a [TableViolation], handler: &str) -> Option<&'a TableViolation> {
    found
        .iter()
        .find(|v| v.node.ends_with(&format!("::{handler}")))
}

#[test]
fn a_route_that_reads_the_other_boundarys_table_fails_and_names_both() {
    let found = violations(&config(""));
    let v = find(&found, "bnd_report").expect("the DSL read is a violation");
    assert_eq!(
        (v.boundary.as_str(), v.owner.as_str()),
        ("marketing", "core")
    );
    assert_eq!(v.table, "bnd_contacts");
    assert_eq!(v.access, Access::Read);
    assert_eq!(v.provenance, Provenance::Body);
    let text = Violation::Table(v.clone()).render();
    assert!(text.starts_with("marketing -> core:"), "{text}");
    assert!(text.contains("GET /bnd/report"), "{text}");
    assert!(text.contains("bnd_contacts"), "{text}");
}

#[test]
fn a_raw_sql_join_is_caught() {
    let found = violations(&config(""));
    let v = find(&found, "bnd_joined").expect("the JOIN is a violation");
    assert_eq!(
        (v.owner.as_str(), v.table.as_str()),
        ("core", "bnd_contacts")
    );
    assert_eq!(v.symbol, "bnd_contacts");
}

#[test]
fn the_own_table_is_no_violation() {
    let found = violations(&config(""));
    assert!(find(&found, "bnd_list_contacts").is_none(), "{found:?}");
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn the_check_passes_once_the_table_is_in_may_read() {
    let found = violations(&config("may_read = [\"bnd_contacts\"]\n"));
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn the_baseline_accepts_todays_violations_and_shrinks() {
    let graph = audit(&[], 0);
    let report = check(&config(""), Some(&graph), None).expect("check runs");
    let baseline = Baseline::from_violations(&report.violations);
    assert!(ratchet(report, Some(&baseline)).passes());

    // Fix the violations: the baseline is stale until it is updated.
    let fixed = check(
        &config("may_read = [\"bnd_contacts\"]\n"),
        Some(&graph),
        None,
    )
    .expect("check runs");
    let result = ratchet(fixed.clone(), Some(&baseline));
    assert!(!result.passes());
    assert_eq!(result.fixed.len(), 2);
    let shrunk = Baseline::from_violations(&fixed.violations);
    assert!(shrunk.violations.is_empty());
    assert!(ratchet(fixed, Some(&shrunk)).passes());
}

#[test]
fn the_fixture_routes_mount() {
    let routes = autumn_web::routes![
        bnd_core::bnd_list_contacts,
        bnd_marketing::bnd_report,
        bnd_marketing::bnd_joined,
    ];
    assert_eq!(routes.len(), 3);
}
