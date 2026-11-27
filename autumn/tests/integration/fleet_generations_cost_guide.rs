//! The cost of `ShipMode::Generations`, as the docs state it.
//!
//! The guide and the rustdoc of `ShipMode::Generations` must give the write
//! formula and the facts that follow from it. The behaviour behind each
//! claim has a test in `db::fleet::generations_tests`.

const GUIDE: &str = include_str!("../../../docs/guide/sqlite-fleet.md");
const ENGINE: &str = include_str!("../../src/db/fleet_engine.rs");

/// The formula, as each doc writes it.
const FORMULA: &str = "changed files × file size × 86,400 / every";

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
        .take_while(|line| line.starts_with("///"))
        .map(|line| line.trim_start_matches("///").trim())
        .collect();
    doc.reverse();
    doc.join(" ")
}

/// Panics with the missing term when `text` does not say each of `terms`.
/// Line breaks count as spaces.
fn assert_says(name: &str, text: &str, terms: &[&str]) {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.to_lowercase();
    for term in terms {
        assert!(
            text.contains(&term.to_lowercase()),
            "{name} must say `{term}`"
        );
    }
}

const COST: &str = "### What a generation costs";

#[test]
fn the_guide_gives_the_write_formula_and_its_units() {
    assert_says(
        COST,
        &section(COST),
        &[
            FORMULA,
            "`every` in seconds",
            "ships each changed file whole",
        ],
    );
}

#[test]
fn the_guide_says_a_small_table_costs_a_whole_file() {
    assert_says(
        COST,
        &section(COST),
        &["a small table", "whole file", "about 1 MB"],
    );
}

#[test]
fn the_guide_says_write_volume_sets_the_rpo() {
    assert_says(
        COST,
        &section(COST),
        &["write volume sets the RPO", "not from the default"],
    );
}

#[test]
fn the_guide_says_prune_bounds_storage_not_traffic() {
    assert_says(
        COST,
        &section(COST),
        &[
            "two newest generations",
            "storage stays bounded",
            "write traffic",
        ],
    );
}

#[test]
fn the_guide_names_the_measure_of_one_ship() {
    assert_says(COST, &section(COST), &["`ShipReport::bytes`", "`ship_now`"]);
}

/// The example of the guide follows from the formula.
#[test]
fn the_guide_example_agrees_with_the_formula() {
    let files: u64 = 64;
    let file_bytes: u64 = 3_300_000;
    let every_secs: u64 = 30;
    let per_day = files * file_bytes * 86_400 / every_secs;
    let gb = per_day / 1_000_000_000;
    let mb_a_minute = files * file_bytes * 60 / every_secs / 1_000_000;
    assert_eq!((gb, mb_a_minute), (608, 422));
    assert_says(
        COST,
        &section(COST),
        &[
            "64 files of about 3.3 MB",
            "`every` = 30",
            "about 600 GB a day",
            "about 420 MB a minute",
        ],
    );
}

/// The section sits in the engine-plugin part of the guide, and the
/// shipping table points to it.
#[test]
fn the_shipping_section_points_to_the_cost() {
    assert_says(
        "### Shipping a plugin engine",
        &section("### Shipping a plugin engine"),
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
            "two newest generations",
        ],
    );
}
