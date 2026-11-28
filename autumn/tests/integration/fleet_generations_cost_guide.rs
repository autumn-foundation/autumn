//! The cost of `ShipMode::Generations`, as the docs state it.
//!
//! The guide and the rustdoc of `ShipMode::Generations` must give the write
//! formula and the facts that follow from it. The behaviour behind each
//! claim has a test in `db::fleet::generations_tests`.

use super::guide_text::{assert_says, section};

const GUIDE: &str = include_str!("../../../docs/guide/sqlite-fleet.md");
const ENGINE: &str = include_str!("../../src/db/fleet_engine.rs");

/// The formula, as each doc writes it.
const FORMULA: &str = "changed files × file size × 86,400 / every";

const COST: &str = "### What a generation costs";

fn cost() -> String {
    section(GUIDE, COST)
}

/// The doc comment on the `Generations` variant, as one line of text.
fn generations_rustdoc() -> String {
    let lines: Vec<&str> = ENGINE.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim() == "Generations {")
        .expect("`ShipMode` has a `Generations` variant");
    let mut doc: Vec<&str> = lines[..at]
        .iter()
        .rev()
        .map(|line| line.trim())
        .skip_while(|line| line.starts_with("#["))
        .take_while(|line| line.starts_with("///"))
        .map(|line| line.trim_start_matches("///").trim())
        .collect();
    doc.reverse();
    doc.join(" ")
}

/// Bytes written to the replica in `secs` seconds, by the formula.
const fn replica_bytes(files: u64, file_bytes: u64, every_secs: u64, secs: u64) -> u64 {
    files * file_bytes * secs / every_secs
}

/// `bytes` to the nearest `step`, in units of `unit`.
const fn about(bytes: u64, step: u64, unit: u64) -> u64 {
    (bytes + step / 2) / step * step / unit
}

const DAY: u64 = 86_400;
const GB: u64 = 1_000_000_000;
const MB: u64 = 1_000_000;

#[test]
fn the_guide_gives_the_write_formula_and_its_units() {
    assert_says(
        COST,
        &cost(),
        &[
            FORMULA,
            "`every` in seconds",
            "zero counts as 1",
            "ships each changed file whole",
        ],
    );
}

#[test]
fn the_guide_says_a_small_table_costs_a_whole_file() {
    assert_says(
        COST,
        &cost(),
        &["a small table", "whole file", "about 1 MB"],
    );
}

#[test]
fn the_guide_says_write_volume_sets_the_rpo() {
    assert_says(
        COST,
        &cost(),
        &["write volume sets the RPO", "not from the default"],
    );
}

#[test]
fn the_guide_says_prune_bounds_storage_not_traffic() {
    assert_says(
        COST,
        &cost(),
        &[
            "two newest complete generations",
            "storage stays bounded",
            "write traffic",
        ],
    );
}

#[test]
fn the_guide_names_the_measure_of_one_ship() {
    assert_says(
        COST,
        &cost(),
        &[
            "`ShipReport::bytes`",
            "`ship_now`",
            "manifest and epoch markers",
        ],
    );
}

/// The numbers of the example in the guide follow from the formula.
#[test]
fn the_guide_example_agrees_with_the_formula() {
    let (files, file_bytes) = (64, 3_300_000);
    let fast = replica_bytes(files, file_bytes, 30, DAY);
    let fast_minute = replica_bytes(files, file_bytes, 30, 60);
    let slow = replica_bytes(files, file_bytes, 300, DAY);
    assert_says(
        COST,
        &cost(),
        &[
            "64 files of about 3.3 MB",
            "`every` = 30 seconds",
            &format!("about {} GB a day", about(fast, 100 * GB, GB)),
            &format!("about {} MB a minute", about(fast_minute, 10 * MB, MB)),
            "`every` = 300 seconds",
            &format!("about {} GB a day", about(slow, 10 * GB, GB)),
            "the RPO is 5 minutes",
        ],
    );
}

#[test]
fn the_shipping_table_links_to_the_cost() {
    assert_says(
        "### Shipping a plugin engine",
        &section(GUIDE, "### Shipping a plugin engine"),
        &["[What a generation costs](#what-a-generation-costs)"],
    );
}

#[test]
fn the_rustdoc_next_to_every_gives_the_cost() {
    assert_says(
        "the rustdoc of `ShipMode::Generations`",
        &generations_rustdoc(),
        &[
            "ships each changed file whole",
            FORMULA,
            "`every` in seconds",
            "write volume sets the RPO",
            "not from the default",
            "two newest generations",
        ],
    );
}
