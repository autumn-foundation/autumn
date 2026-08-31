//! Comment-aware scanning of user-owned Rust source.
//!
//! Commands that edit an application's `src/main.rs` — `autumn generate pwa`,
//! `autumn plugin add` — must never splice code into a comment. Autumn's own
//! documentation ships quick-start snippets containing the exact lines those
//! commands anchor on (`autumn_web::app()`, `autumn_web::push::router()`), so a
//! `main.rs` that pasted one into a doc comment would otherwise be edited
//! *inside* the comment: a file that does not compile, and a mount that never
//! happened.
//!
//! One scanner, shared, so an anchor scan and its "is it already there?"
//! companion can never disagree about what a comment is.

/// Walk `source`'s lines that are **code**, skipping comments, and return the
/// first non-`None` result of `f`.
///
/// `f` receives the raw line and its byte offset in `source`. The offset is
/// tracked as the walk proceeds rather than recovered afterwards with
/// `source.find(line)`, which would re-locate an identical earlier line and
/// splice at the wrong place.
///
/// Every caller that edits a user-owned `main.rs` shares this one scanner so
/// they can never disagree about what counts as a comment — a disagreement
/// there is exactly what lets a doc-comment mention both suppress an edit and
/// suppress the warning about it. Callers today: `autumn generate pwa`'s push-
/// router injection and `autumn plugin add`'s builder-chain scan.
pub fn for_each_code_line<T>(
    source: &str,
    mut f: impl FnMut(&str, usize) -> Option<T>,
) -> Option<T> {
    let mut offset = 0_usize;
    let mut in_block_comment = false;

    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let line_start = offset;
        offset += line.len();

        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        // `//`, `///`, `//!`, and continuation lines of a block comment.
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }

        if let Some(found) = f(line, line_start) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::for_each_code_line;

    /// The offset handed to the callback must point at the line the callback
    /// actually saw — recovering it afterwards with `find` would land on an
    /// identical earlier line and splice at the wrong place.
    #[test]
    fn the_offset_locates_the_matching_line() {
        let source = "let x = 1;\nlet x = 1;\nlet y = 2;\n";
        let offset = for_each_code_line(source, |line, offset| {
            line.contains("y = 2").then_some(offset)
        })
        .expect("found");
        assert_eq!(&source[offset..offset + 9], "let y = 2");
    }

    #[test]
    fn line_comments_are_skipped() {
        for source in [
            "// let x = 1;\n",
            "/// let x = 1;\n",
            "//! let x = 1;\n",
            "   // let x = 1;\n",
        ] {
            assert!(
                for_each_code_line(source, |line, _| line.contains("let x").then_some(()))
                    .is_none(),
                "{source:?}"
            );
        }
    }

    #[test]
    fn block_comments_are_skipped_across_lines() {
        let source = "/*\nlet x = 1;\n*/\nlet y = 2;\n";
        assert!(
            for_each_code_line(source, |line, _| line.contains("let x").then_some(())).is_none()
        );
        assert!(
            for_each_code_line(source, |line, _| line.contains("let y").then_some(())).is_some()
        );
    }

    #[test]
    fn a_one_line_block_comment_does_not_open_a_block() {
        let source = "/* noise */\nlet y = 2;\n";
        assert!(
            for_each_code_line(source, |line, _| line.contains("let y").then_some(())).is_some()
        );
    }

    #[test]
    fn the_first_match_wins() {
        let source = "let a = 1;\nlet a = 2;\n";
        let offset = for_each_code_line(source, |line, offset| {
            line.contains("let a").then_some(offset)
        })
        .expect("found");
        assert_eq!(offset, 0);
    }
}
