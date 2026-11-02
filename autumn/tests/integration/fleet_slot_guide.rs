//! These tests check that `docs/guide/sqlite-fleet.md` states the Pigeonhole
//! findings. Pigeonhole is a slot-fleet lab. The behaviour behind each claim
//! has a test in `db`, `db::fleet`, `config` and `sharding`.

const GUIDE: &str = include_str!("../../../docs/guide/sqlite-fleet.md");

/// The text under `heading`, up to the next heading of the same or a higher
/// level. A `#` line in a code fence is not a heading.
fn section(heading: &str) -> String {
    let level = heading_level(heading).expect("a markdown heading");
    let mut lines = GUIDE.lines().skip_while(|line| *line != heading);
    assert!(
        lines.next().is_some(),
        "the guide has no `{heading}` section"
    );
    let mut in_fence = false;
    let mut text = Vec::new();
    for line in lines {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence && heading_level(line).is_some_and(|l| l <= level) {
            break;
        }
        text.push(line);
    }
    text.join("\n")
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
}

fn assert_says(heading: &str, terms: &[&str]) {
    let text = section(heading).to_lowercase();
    for term in terms {
        assert!(
            text.contains(&term.to_lowercase()),
            "`{heading}` must say `{term}`"
        );
    }
}

/// Finding 3: slot mode takes any routing key. An app that moves from tenant
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

/// Finding 1: a handler that holds `Db` across a fan-out logs one long hold,
/// not a slow query.
#[test]
fn the_handler_section_says_what_a_held_connection_logs() {
    assert_says(
        "## Handlers",
        &["database connection held", "`LazyDb`", "not one statement"],
    );
}

/// Finding 2: the boot warning gives the open-file count.
#[test]
fn the_budget_line_names_the_boot_warning() {
    assert_says(
        "## Opening and closing",
        &["boot warning", "2 × connections + databases"],
    );
}
