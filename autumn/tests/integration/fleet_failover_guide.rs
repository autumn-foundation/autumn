//! The costs of a fleet failover, as `docs/guide/sqlite-fleet.md` states them.
//!
//! Baton (a failover lab) measured these costs on 0.9. The guide must state
//! each one, and its numbers must agree with the config defaults. The
//! behaviour behind each claim has a test in `db::fleet_ownership`.

use autumn_web::config::{FleetOwnershipConfig, ReplicationConfig};

const GUIDE: &str = include_str!("../../../docs/guide/sqlite-fleet.md");

/// The level of a markdown heading line, or `None` for another line.
fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
}

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

fn assert_says(section_text: &str, heading: &str, terms: &[&str]) {
    let lower = section_text.to_lowercase();
    for term in terms {
        assert!(
            lower.contains(&term.to_lowercase()),
            "`{heading}` must say `{term}`"
        );
    }
}

fn default_lease_secs() -> u64 {
    let config: FleetOwnershipConfig =
        toml::from_str("owner = \"host-a\"").expect("a minimal ownership section parses");
    config.lease_secs
}

/// Finding 1: "replication" reads as "an acknowledged write survives".
#[test]
fn the_replication_section_says_an_acknowledged_write_can_be_lost() {
    let heading = "## Replication: object storage between volumes";
    assert_says(
        &section(heading),
        heading,
        &["asynchronous", "acknowledged", "What a failover costs"],
    );
}

/// Findings 1, 2 and 6: the data and the time that a failover costs.
#[test]
fn the_guide_gives_the_cost_of_a_failover() {
    let heading = "### What a failover costs";
    let text = section(heading);
    assert_says(
        &text,
        heading,
        &[
            "asynchronous",
            "`rpo_secs`",
            "`worst_lag_seconds`",
            "`lease_secs + lease_secs / 3`",
            "outage",
            "SIGSTOP",
        ],
    );
    let lease = default_lease_secs();
    let takeover = lease + lease / 3;
    let rpo = ReplicationConfig::default().rpo_secs;
    assert_says(
        &text,
        heading,
        &[&format!("{takeover} s"), &format!("{rpo} s")],
    );
}

/// Finding 2: the ownership config states the default lease, next to the
/// cost of a long one.
#[test]
fn the_ownership_example_shows_the_default_lease() {
    let heading = "## Failover: slot ownership";
    let text = section(heading);
    let lease = default_lease_secs();
    assert!(
        text.contains(&format!("lease_secs = {lease}      # default")),
        "`{heading}` must show the default `lease_secs = {lease}`"
    );
    assert_says(&text, heading, &["takeover time"]);
}

/// Finding 4: `restore_missing` reads as "only when the file is absent".
/// A fail back restores a stale file without it.
#[test]
fn the_guide_says_a_stale_file_is_restored_without_restore_missing() {
    let heading = "## Replication: object storage between volumes";
    assert_says(
        &section(heading),
        heading,
        &["stale", "without `restore_missing`"],
    );
    let heading = "### The failover procedure";
    assert_says(&section(heading), heading, &["without `restore_missing`"]);
}
