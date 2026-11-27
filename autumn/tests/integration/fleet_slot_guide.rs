//! These tests check that `docs/guide/sqlite-fleet.md` states three slot-fleet
//! facts: slot keys, held connections and the open-file budget. The behaviour
//! behind each claim has a test in `db`, `db::fleet`, `config` and `sharding`.

use super::guide_text;

const GUIDE: &str = include_str!("../../../docs/guide/sqlite-fleet.md");

fn assert_says(heading: &str, terms: &[&str]) {
    guide_text::assert_says(heading, &guide_text::section(GUIDE, heading), terms);
}

/// Slot mode takes any routing key. An app that moves from tenant
/// mode loses the id check.
#[test]
fn the_tenant_id_section_says_a_slot_fleet_takes_any_key() {
    assert_says(
        "### Tenant ids",
        &[
            "a slot fleet takes any routing key",
            "from tenant to slot mode",
            "`TenantDbId::parse`",
        ],
    );
}

/// A handler that holds `Db` across a fan-out logs one long hold,
/// not a slow query.
#[test]
fn the_handler_section_says_what_a_held_connection_logs() {
    assert_says(
        "## Handlers",
        &["database connection held", "`LazyDb`", "not one statement"],
    );
}

/// The boot warning gives the open-file count.
#[test]
fn the_budget_line_names_the_boot_warning() {
    assert_says(
        "## Opening and closing",
        &[
            "boot warning",
            "2 × connections + databases",
            "80 % of the open-file limit",
        ],
    );
}
