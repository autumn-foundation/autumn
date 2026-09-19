//! Per-file preview rendering for `autumn upgrade` (issue #1629).
//!
//! `autumn upgrade` writes nothing by default, so the diff *is* the product of
//! the default invocation. It stays deliberately small: the rewrites this
//! command ships are identifier renames, which never add or remove a line, so a
//! line-for-line comparison is exact and no LCS machinery is needed. The
//! degenerate case where the line counts do differ is still rendered (as one
//! hunk from the first divergence) rather than trusted away.

/// Render a unified-ish diff of `original` → `updated`.
///
/// Returns an empty string when the two are identical.
pub fn render(original: &str, updated: &str) -> String {
    use std::fmt::Write as _;

    if original == updated {
        return String::new();
    }
    let before: Vec<&str> = original.lines().collect();
    let after: Vec<&str> = updated.lines().collect();

    if before.len() != after.len() {
        // Not reachable from an identifier rename, which cannot add or remove a
        // line. Rendered rather than asserted away so a future rewrite kind
        // that *does* change the shape still previews something honest.
        let first = (0..before.len().min(after.len()))
            .find(|&index| before[index] != after[index])
            .unwrap_or_else(|| before.len().min(after.len()));
        let span = (before.len() - first).max(after.len() - first);
        let mut out = header(first + 1, first + span);
        for line in &before[first..] {
            let _ = writeln!(out, "-{line}");
        }
        for line in &after[first..] {
            let _ = writeln!(out, "+{line}");
        }
        return out;
    }

    let changed: Vec<usize> = (0..before.len())
        .filter(|&index| before[index] != after[index])
        .collect();

    let mut out = String::new();
    let mut cursor = 0;
    while cursor < changed.len() {
        // One hunk per run of consecutive changed lines.
        let mut end = cursor;
        while end + 1 < changed.len() && changed[end + 1] == changed[end] + 1 {
            end += 1;
        }
        out.push_str(&header(changed[cursor] + 1, changed[end] + 1));
        for &index in &changed[cursor..=end] {
            let _ = writeln!(out, "-{}", before[index]);
        }
        for &index in &changed[cursor..=end] {
            let _ = writeln!(out, "+{}", after[index]);
        }
        cursor = end + 1;
    }
    out
}

/// `@@ line 12 @@` for one line, `@@ lines 12-14 @@` for a run.
fn header(first: usize, last: usize) -> String {
    if first >= last {
        format!("@@ line {first} @@\n")
    } else {
        format!("@@ lines {first}-{last} @@\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_input_renders_nothing() {
        assert_eq!(render("a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn renders_a_single_changed_line_with_its_number() {
        let out = render(
            "fn f() {\n    let r = Repo::with_pool(p);\n}\n",
            "fn f() {\n    let r = Repo::with_pool_untracked(p);\n}\n",
        );
        assert_eq!(
            out,
            "@@ line 2 @@\n-    let r = Repo::with_pool(p);\n+    let r = Repo::with_pool_untracked(p);\n"
        );
    }

    #[test]
    fn groups_adjacent_changed_lines_into_one_hunk() {
        let out = render("a\nb\nc\n", "a\nB\nC\n");
        assert_eq!(out, "@@ lines 2-3 @@\n-b\n-c\n+B\n+C\n");
    }

    #[test]
    fn separates_non_adjacent_changes_into_their_own_hunks() {
        let out = render("a\nb\nc\nd\n", "A\nb\nc\nD\n");
        assert_eq!(out, "@@ line 1 @@\n-a\n+A\n@@ line 4 @@\n-d\n+D\n");
    }

    #[test]
    fn renders_a_line_count_change_as_one_hunk_from_the_first_divergence() {
        let out = render("a\nb\n", "a\nb\nc\n");
        assert!(
            out.starts_with("@@ line 3 @@"),
            "unexpected rendering: {out}"
        );
        assert!(out.contains("+c"), "unexpected rendering: {out}");
    }

    #[test]
    fn strips_carriage_returns_from_displayed_lines() {
        // The file keeps its CRLF; showing a stray `\r` in the terminal does not
        // help anyone read the diff.
        let out = render("a\r\nb\r\n", "a\r\nB\r\n");
        assert_eq!(out, "@@ line 2 @@\n-b\n+B\n");
    }
}
