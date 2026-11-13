//! The ADR that keeps wire contracts in Rust types and makes an IDL an
//! export. These tests check that the ADR has the shape of the other ADRs,
//! that its number is its own, and that the readers of the wire-contracts
//! guide can find it.

use std::fs;
use std::path::{Path, PathBuf};

const ADR_SLUG: &str = "-wire-contracts-idl-at-the-edge.md";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("autumn-cli is under the workspace root")
        .to_path_buf()
}

/// Read a repo file with LF line ends, so the checks also hold on Windows.
fn read_repo_file(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// The lines outside fenced code blocks. Text in a code block does not count.
fn prose(text: &str) -> Vec<&str> {
    let mut in_fence = false;
    text.lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                return false;
            }
            !in_fence
        })
        .collect()
}

/// The prose as one line, with each run of whitespace made one space. A
/// reflow of the text does not change it.
fn flat(text: &str) -> String {
    prose(text)
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn adr_names() -> Vec<String> {
    let dir = workspace_root().join("docs/adr");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("read a docs/adr entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// The file name of the ADR. There must be exactly one.
fn adr_name() -> String {
    let found: Vec<String> = adr_names()
        .into_iter()
        .filter(|name| name.ends_with(ADR_SLUG))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "docs/adr needs exactly one `NNNN{ADR_SLUG}`, found {found:?}"
    );
    found.into_iter().next().expect("one ADR")
}

/// The four-digit number at the start of an ADR file name.
fn number_of(name: &str) -> Option<&str> {
    let number = name.get(..4)?;
    (number.bytes().all(|b| b.is_ascii_digit()) && name.as_bytes().get(4) == Some(&b'-'))
        .then_some(number)
}

#[test]
fn adr_number_is_not_used_by_another_adr() {
    let name = adr_name();
    let number = number_of(&name).expect("the ADR file name starts with `NNNN-`");
    let others: Vec<String> = adr_names()
        .into_iter()
        .filter(|other| number_of(other) == Some(number))
        .filter(|other| *other != name)
        .collect();
    assert!(
        others.is_empty(),
        "ADR {number} is also used by {others:?}; take the next free number"
    );
}

#[test]
fn adr_has_the_house_header_and_sections() {
    let name = adr_name();
    let number = number_of(&name).expect("the ADR file name starts with `NNNN-`");
    let adr = read_repo_file(&format!("docs/adr/{name}"));
    let lines = prose(&adr);

    let title = lines.first().copied().unwrap_or_default();
    assert!(
        title.starts_with(&format!("# ADR {number}: ")),
        "the first line must be `# ADR {number}: <title>`, got {title:?}"
    );
    for field in ["- Status: Accepted", "- Date: ", "- Deciders: ", "- Tags: "] {
        assert!(
            lines.iter().any(|line| line.starts_with(field)),
            "the ADR header needs a `{field}` line"
        );
    }

    let mut rest = lines.iter();
    for heading in [
        "## Context",
        "## Options",
        "## Decision",
        "## Consequences",
        "## When to revisit",
    ] {
        assert!(
            rest.any(|line| *line == heading),
            "the ADR needs `{heading}` outside code, in this order"
        );
    }
}

#[test]
fn adr_records_the_decision_and_its_reasons() {
    let adr = flat(&read_repo_file(&format!("docs/adr/{}", adr_name())));
    for (claim, needle) in [
        ("the epic", "issues/3548"),
        ("the protobuf export issue", "issues/3665"),
        ("the Avro export issue", "issues/3666"),
        ("the shape derive", "WireShape"),
        ("the endpoint macro", "#[endpoint]"),
        ("the event macro", "#[event]"),
        ("the cross-release check", "`autumn contracts check`"),
        ("the protobuf export", "`autumn contracts export --proto`"),
        ("the Avro export", "`autumn contracts export --avro`"),
        (
            "that the Rust types stay the source of truth",
            "The Rust types stay the source of truth.",
        ),
        (
            "that IDL files are not imported",
            "Autumn does not import IDL files.",
        ),
        ("the silent proto3 default", "gets a default value"),
        ("the shared database", "the shared database"),
        ("why Thrift is not offered", "Thrift is not offered"),
    ] {
        assert!(
            adr.contains(needle),
            "the ADR must state {claim} (looked for {needle:?})"
        );
    }
}

#[test]
fn wire_contracts_guide_links_the_adr() {
    let name = adr_name();
    let number = number_of(&name).expect("the ADR file name starts with `NNNN-`");
    let link = format!("[ADR {number}](../adr/{name})");
    let guide = read_repo_file("docs/guide/wire-contracts.md");

    // The intro says there is no IDL. It must also say where the IDL goes.
    let intro = guide.split("\n## ").next().unwrap_or_default();
    assert!(
        flat(intro).contains(&link),
        "the guide intro says there is no IDL; it must link `{link}`"
    );

    // The ADR names this section as the trigger to revisit the decision.
    let limits = guide
        .split("\n## Limits of the first slice\n")
        .nth(1)
        .expect("the guide has `## Limits of the first slice`");
    let limits = limits.split("\n## ").next().unwrap_or(limits);
    assert!(
        flat(limits).contains(&link),
        "`Limits of the first slice` must link `{link}`, which says when to revisit"
    );
}
