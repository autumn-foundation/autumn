//! Helpers for tests that read the text of a markdown guide.

/// The level of a markdown heading line, or `None` for another line.
fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
}

/// The text of `guide` under `heading`, up to the next heading of the same
/// or a higher level. A `#` line in a code fence is not a heading.
pub fn section(guide: &str, heading: &str) -> String {
    let level = heading_level(heading).expect("a markdown heading");
    let mut lines = guide.lines().skip_while(|line| *line != heading);
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

/// Panics with the missing term when `text` does not say each of `terms`.
/// The check ignores case. A line break counts as a space.
pub fn assert_says(name: &str, text: &str, terms: &[&str]) {
    let text = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    for term in terms {
        assert!(
            text.contains(&term.to_lowercase()),
            "`{name}` must say `{term}`"
        );
    }
}
