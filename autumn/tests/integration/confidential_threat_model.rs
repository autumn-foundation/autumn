//! The documented threat model, asserted in CI (#1771 AC5).
//!
//! `docs/guide/confidential-fields.md` states what the operator can and cannot
//! see. A doc drifts the moment the code moves, so this test pins the two
//! together: every sink the code claims blindness for has a row in the guide,
//! every row in the guide is a sink the code claims, and the mechanism behind
//! each claim is exercised here rather than asserted in prose.
//!
//! The four sinks the acceptance criteria name are swept for a seeded plaintext
//! marker by `confidential_red_team`. This test asserts the *set*.

#![cfg(feature = "db")]

use autumn_web::confidential::{self, BlindIndex, OPERATOR_BLIND_SINKS, OPERATOR_VISIBLE, Sealed};

const GUIDE: &str = include_str!("../../../docs/guide/confidential-fields.md");

diesel::table! {
    threat_model_notes (id) {
        id -> Integer,
        secret_note -> Text,
        secret_note_bidx -> Text,
    }
}

#[autumn_web::model(table = "threat_model_notes")]
pub struct ThreatModelNote {
    pub id: i32,
    #[confidential(blind_index)]
    pub secret_note: Sealed,
    pub secret_note_bidx: BlindIndex,
}

/// The rows of the guide's "Cannot see" table, by sink id.
///
/// Scoped to that one section: the page carries other tables, and a sink id is
/// only a claim when it sits under the heading that makes the claim.
fn documented_sinks() -> Vec<String> {
    let section = GUIDE
        .split_once("### Cannot see")
        .expect("the guide must carry a \"Cannot see\" section")
        .1
        .split_once("### Can see")
        .expect("the guide must carry a \"Can see\" section")
        .0;
    section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("| `"))
        .filter_map(|rest| rest.split('`').next())
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_guide_and_the_code_agree_on_the_cannot_see_set() {
    let documented = documented_sinks();
    for sink in OPERATOR_BLIND_SINKS {
        assert!(
            documented.iter().any(|d| d == sink.id),
            "`{}` is claimed in code but has no row in the guide's \"Cannot see\" \
             table: {documented:?}",
            sink.id
        );
        // The `why` text is the half that actually drifts, so pin its opening
        // clause to the guide row as well as the sink id.
        let head: String = sink
            .why
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            GUIDE.to_lowercase().contains(&head),
            "the guide row for `{}` must state why: `{head}` is missing",
            sink.id
        );
    }
    for sink in &documented {
        assert!(
            OPERATOR_BLIND_SINKS.iter().any(|s| s.id == *sink),
            "the guide documents `{sink}` but no sink in \
             `OPERATOR_BLIND_SINKS` claims it"
        );
    }
    assert_eq!(
        documented.len(),
        OPERATOR_BLIND_SINKS.len(),
        "the guide table and the code list must be the same size"
    );
}

#[test]
fn the_cannot_see_set_names_every_sink_the_acceptance_criteria_do() {
    // The four sinks #1771 requires, plus the three this slice also covers.
    for required in [
        "database",
        "access_log",
        "db_backup",
        "replay_capsule",
        "version_history",
        "admin_ui",
        "admin_csv_export",
    ] {
        assert!(
            OPERATOR_BLIND_SINKS.iter().any(|s| s.id == required),
            "`{required}` must be in the operator-blind set"
        );
    }
}

#[test]
fn what_the_operator_can_still_see_is_documented_too() {
    assert!(
        !OPERATOR_VISIBLE.is_empty(),
        "an overclaimed guarantee is worse than a narrow one"
    );
    for claim in OPERATOR_VISIBLE {
        // The guide wraps its prose, so match on a distinctive opening phrase.
        let head: String = claim
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            GUIDE.to_lowercase().contains(&head),
            "the guide must state what stays visible: `{head}` is missing"
        );
    }
}

// ── The mechanism behind each claim ─────────────────────────────────────────

#[test]
fn the_log_filter_is_fed_the_confidential_column_names() {
    // `access_log`: `router`, `job` and `telemetry` all extend their parameter
    // filter with this list, so the name is masked wherever request data is
    // written down.
    let names = confidential::registered_confidential_column_names();
    assert!(names.contains(&"secret_note".to_owned()), "{names:?}");
    assert!(names.contains(&"secret_note_bidx".to_owned()), "{names:?}");

    let filter = autumn_web::log::filter::ParameterFilter::new(&names, &[]);
    assert!(filter.matches_key("secret_note"));
    assert!(filter.matches_key("secret_note_bidx"));
    assert!(!filter.matches_key("id"));
}

#[test]
fn version_history_holds_a_marker_rather_than_the_envelope() {
    let mut cols: Vec<&'static str> = Vec::new();
    confidential::merge_confidential_columns_for_table("threat_model_notes", &mut cols);
    // The token is sensitive too: a history of tokens is a history of which
    // values repeated, and it outlives the row that held them.
    assert_eq!(cols, vec!["secret_note", "secret_note_bidx"]);
}

#[test]
fn the_admin_redacts_the_column_and_its_token_by_name() {
    assert!(confidential::is_confidential_column_name("secret_note"));
    assert!(confidential::is_confidential_column_name(
        "secret_note_bidx"
    ));
    assert!(!confidential::is_confidential_column_name("id"));
}

#[test]
fn the_database_holds_the_envelope_because_that_is_the_column_type() {
    // `database`, `db_backup` and `replay_capsule` all follow from one fact: the
    // only value the server can bind is the envelope. `Sealed` has no
    // constructor, accessor or conversion that yields plaintext, so this is a
    // property of the type rather than of any call site.
    let sealed = Sealed::default();
    assert_eq!(format!("{sealed:?}"), "Sealed(<sealed>)");
    assert!(Sealed::from_envelope("plaintext leak".to_owned()).is_err());
}
