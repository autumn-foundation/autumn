//! The costs of a fleet failover, and the rules to move a row between fleet
//! databases, as `docs/guide/sqlite-fleet.md` states them.
//!
//! The guide must state each cost and each rule. Its numbers must agree with
//! the config defaults. The behaviour behind each claim has a test in
//! `db::fleet_ownership`.

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

/// A reader can take "replication" to mean that an acknowledged write
/// survives. The guide must say that it can be lost.
#[test]
fn the_replication_section_says_an_acknowledged_write_can_be_lost() {
    let heading = "## Replication: object storage between volumes";
    assert_says(
        &section(heading),
        heading,
        &["asynchronous", "acknowledged", "What a failover costs"],
    );
}

/// The last cell of the table row whose first cell is `label`.
fn row_default(text: &str, label: &str) -> String {
    let row = text
        .lines()
        .find(|line| line.starts_with(&format!("| {label} |")))
        .unwrap_or_else(|| panic!("the cost table has no `{label}` row"));
    let cells: Vec<&str> = row.split('|').map(str::trim).collect();
    cells[cells.len() - 2].to_owned()
}

/// The data and the time that a failover costs.
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
            "opened during the pause",
            "keeps its own file",
        ],
    );
    let lease = default_lease_secs();
    let rpo = ReplicationConfig::default().rpo_secs;
    for (label, secs) in [
        ("Data", rpo),
        ("Time, old host crashed", lease + lease / 3),
        ("Time, old lease already ended", lease / 3),
    ] {
        let default = row_default(&text, label);
        assert!(
            default.starts_with(&format!("{secs} s")),
            "the `{label}` row must give the default {secs} s, not `{default}`"
        );
    }
}

/// The ownership config states the default lease, next to the
/// cost of a long one.
#[test]
fn the_ownership_example_shows_the_default_lease() {
    let heading = "## Failover: slot ownership";
    let text = section(heading);
    let lease = default_lease_secs();
    assert!(
        text.lines().any(|line| {
            line.starts_with(&format!("lease_secs = {lease} ")) && line.contains("# default")
        }),
        "`{heading}` must show the default `lease_secs = {lease}`"
    );
    assert_says(&text, heading, &["takeover time"]);
}

/// A reader can take `restore_missing` to mean "only when the file is
/// absent". The guide must say that a fail-back restores a stale file
/// without it.
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

const MOVE: &str = "## Move rows between fleet databases";

/// The numbered rules of `text`, in order: the text after `1. `, `2. `, ...
/// A rule ends at a blank line. Code fences are not read.
fn rules(text: &str) -> Vec<String> {
    let mut rules: Vec<String> = Vec::new();
    let mut open = false;
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            open = false;
            continue;
        }
        if in_fence || line.trim().is_empty() {
            open = false;
            continue;
        }
        let numbered = line
            .split_once(". ")
            .filter(|(n, _)| n.parse::<usize>() == Ok(rules.len() + 1));
        if let Some((_, rule)) = numbered {
            rules.push(rule.to_owned());
            open = true;
        } else if let Some(last) = rules.last_mut().filter(|_| open && line.starts_with("   ")) {
            last.push(' ');
            last.push_str(line.trim());
        } else {
            open = false;
        }
    }
    rules
}

/// The five rules of a move, in order.
#[test]
fn the_guide_gives_the_five_rules_of_a_move() {
    let text = section(MOVE);
    let rules = rules(&text);
    assert_eq!(rules.len(), 5, "`{MOVE}` must give five rules: {rules:#?}");
    let expect: [&[&str]; 5] = [
        &[
            "version one higher",
            "never replace a newer version",
            "tombstone",
        ],
        &["mark the source copy moved", "keep it"],
        &[
            "delete the source copy only when",
            "coverage(",
            "replica_bound",
            "same epoch",
        ],
        &[
            "new owner",
            "new epoch",
            "deliver again",
            "mark moved again",
        ],
        &["safe to repeat", "versioned upsert", "bounded by version"],
    ];
    for (n, (rule, terms)) in rules.iter().zip(expect).enumerate() {
        assert_says(rule, &format!("{MOVE}, rule {}", n + 1), terms);
    }
}

/// The obvious rule loses data. The guide must say so, and why.
#[test]
fn the_guide_says_a_delete_on_acknowledgement_loses_a_row() {
    assert_says(
        &section(MOVE),
        MOVE,
        &[
            "delete on acknowledgement",
            "rewinds the destination",
            "the row is lost",
            "every engine that has a replica bound",
            "`rebuild` mode",
            "`fleet.replica_bound(&dst)`",
            "process that replicates",
            "`Covered`",
            "`Pending`",
            "`Stale`",
            "unix nanoseconds",
            "do not round it",
        ],
    );
}

/// A reader of the older sections must find the move rules.
#[test]
fn the_replica_and_failover_sections_point_to_the_move_rules() {
    let link = "(#move-rows-between-fleet-databases)";
    for heading in [
        "### Know when a write is in the replica",
        "### The failover procedure",
    ] {
        assert_says(&section(heading), heading, &[link]);
    }
    assert!(
        GUIDE
            .lines()
            .any(|line| line.starts_with("| `fleet.replica_bound(&key)` |")),
        "the API table must list `fleet.replica_bound(&key)`"
    );
}
