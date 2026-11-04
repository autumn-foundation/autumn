//! The self-test of `scripts/check-docs-guide-index.sh`, one test per
//! case. Each corpus was captured from the script's own fixtures at the
//! moment it ran: the tracked files under `docs/guide/`, and the contents
//! of every index and the README. Page contents are never read, so only
//! their paths are kept.

use super::tests::case;

/// 1. complete index passes
#[test]
fn complete_index_passes() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
            "docs/guide/tutorial/01-x.md",
            "docs/guide/tutorial/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [B](beta.md)\n",
                    "- [T](tutorial/index.md)\n"
                ),
            ),
            (
                "docs/guide/tutorial/index.md",
                concat!("# T\n", "\n", "## Chapters\n", "\n", "1. [One](01-x.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 2. unlisted page fails
#[test]
fn unlisted_page_fails() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 3. duplicate entry fails
#[test]
fn duplicate_entry_fails() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "## T\n",
                    "\n",
                    "- [A again](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 4. entry outside a section fails
#[test]
fn entry_outside_a_section_fails() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 5. link to a missing guide page fails
#[test]
fn link_to_a_missing_guide_page_fails() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [G](ghost.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 6. README without the index link fails
#[test]
fn readme_without_the_index_link_fails() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "no pointer here\n"),
        ],
    );
}

/// 7. absent index fails
#[test]
fn absent_index_fails() {
    case(
        false,
        &["docs/guide/alpha.md"],
        &[("README.md", "[Guide index](docs/guide/index.md)\n")],
    );
}

/// 8. fenced entry is not double-counted
#[test]
fn fenced_entry_is_not_double_counted() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [B](beta.md)\n",
                    "\n",
                    "```\n",
                    "- [A](alpha.md)\n",
                    "```\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 9. unlisted page in a delegated subdir fails
#[test]
fn unlisted_page_in_a_delegated_subdir_fails() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/index.md",
            "docs/guide/tutorial/01-x.md",
            "docs/guide/tutorial/02-unlisted.md",
            "docs/guide/tutorial/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [T](tutorial/index.md)\n"
                ),
            ),
            (
                "docs/guide/tutorial/index.md",
                concat!("# T\n", "\n", "## Chapters\n", "\n", "1. [One](01-x.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 10. delegated page listed in its sub-index passes
#[test]
fn delegated_page_listed_in_its_sub_index_passes() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/index.md",
            "docs/guide/tutorial/01-x.md",
            "docs/guide/tutorial/02-listed.md",
            "docs/guide/tutorial/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [T](tutorial/index.md)\n"
                ),
            ),
            (
                "docs/guide/tutorial/index.md",
                concat!(
                    "# T\n",
                    "\n",
                    "## Chapters\n",
                    "\n",
                    "1. [One](01-x.md)\n",
                    "2. [Two](02-listed.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 11. README mention without a link fails
#[test]
fn readme_mention_without_a_link_fails() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "The guide index lives at `docs/guide/index.md` somewhere.\n",
            ),
        ],
    );
}

/// 12. commented-out entry does not count
#[test]
fn commented_out_entry_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "<!-- - [B](beta.md) -->\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 13. multi-line commented entry does not count
#[test]
fn multi_line_commented_entry_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "<!--\n",
                    "parked:\n",
                    "- [B](beta.md)\n",
                    "-->\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 14. tilde-fenced entry does not count
#[test]
fn tilde_fenced_entry_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "~~~markdown\n",
                    "- [B](beta.md)\n",
                    "~~~\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 15. backtick fence inside a tilde fence stays hidden
#[test]
fn backtick_fence_inside_a_tilde_fence_stays_hidden() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "~~~markdown\n",
                    "```\n",
                    "- [B](beta.md)\n",
                    "```\n",
                    "~~~\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 16. README link only inside a fence fails
#[test]
fn readme_link_only_inside_a_fence_fails() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Docs\n",
                    "\n",
                    "```markdown\n",
                    "[Guide](docs/guide/index.md)\n",
                    "```\n"
                ),
            ),
        ],
    );
}

/// 17. inline-code entry does not count
#[test]
fn inline_code_entry_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- write it as `[B](beta.md)`\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 18. indented-code entry does not count
#[test]
fn indented_code_entry_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "For example:\n",
                    "\n",
                    "    - [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 19. nested row is not an entry
#[test]
fn nested_row_is_not_an_entry() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "    - [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 20. page listed in two indexes fails
#[test]
fn page_listed_in_two_indexes_fails() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/index.md",
            "docs/guide/tutorial/01-x.md",
            "docs/guide/tutorial/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [T](tutorial/index.md)\n",
                    "- [One](tutorial/01-x.md)\n"
                ),
            ),
            (
                "docs/guide/tutorial/index.md",
                concat!("# T\n", "\n", "## Chapters\n", "\n", "1. [One](01-x.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 21. prose cross-reference is not an entry
#[test]
fn prose_cross_reference_is_not_an_entry() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/index.md",
            "docs/guide/tutorial/01-x.md",
            "docs/guide/tutorial/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [T](tutorial/index.md)\n"
                ),
            ),
            (
                "docs/guide/tutorial/index.md",
                concat!(
                    "# T\n",
                    "\n",
                    "## Chapters\n",
                    "\n",
                    "1. [One](01-x.md)\n",
                    "\n",
                    "See the [A guide](../alpha.md) when you finish.\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 22. code block nested in a list does not count
#[test]
fn code_block_nested_in_a_list_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- Example:\n",
                    "\n",
                    "      - [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 23. nested row after a blank line is not an entry
#[test]
fn nested_row_after_a_blank_line_is_not_an_entry() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "    - [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 24. image entry does not count
#[test]
fn image_entry_does_not_count() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- ![preview](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 25. image in README is not an index link
#[test]
fn image_in_readme_is_not_an_index_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "![Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 26. tab-indented row is not an entry
#[test]
fn tab_indented_row_is_not_an_entry() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "\t- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 27. escaped bracket is not an entry
#[test]
fn escaped_bracket_is_not_an_entry() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- \\[B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 28. escaped link in README is not an index link
#[test]
fn escaped_link_in_readme_is_not_an_index_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "\\[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 29. row inside a raw HTML block is not an entry
#[test]
fn row_inside_a_raw_html_block_is_not_an_entry() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "<pre>\n",
                    "- [B](beta.md)\n",
                    "</pre>\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 30. HTML block ends at a blank line
#[test]
fn html_block_ends_at_a_blank_line() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "<details><summary>note</summary>\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 31. indented raw HTML opener still opens a block
#[test]
fn indented_raw_html_opener_still_opens_a_block() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    " <pre>\n",
                    "- [B](beta.md)\n",
                    "</pre>\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 32. row inside a CDATA block is not an entry
#[test]
fn row_inside_a_cdata_block_is_not_an_entry() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "<![CDATA[\n",
                    "- [B](beta.md)\n",
                    "]]>\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 33. one-line <pre> does not swallow later rows
#[test]
fn one_line_pre_does_not_swallow_later_rows() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "<pre>sample</pre>\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 34. inline-code link in README is not an index link
#[test]
fn inline_code_link_in_readme_is_not_an_index_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "Write it as `[Guide](docs/guide/index.md)` in your docs.\n",
            ),
        ],
    );
}

/// 35. multi-line code span hides its rows
#[test]
fn multi_line_code_span_hides_its_rows() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "write `like this:\n",
                    "- [B](beta.md)` in docs\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 36. unmatched backtick blanks nothing
#[test]
fn unmatched_backtick_blanks_nothing() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "100% of the `budget is spent\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 37. link inside image alt text is not an index link
#[test]
fn link_inside_image_alt_text_is_not_an_index_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "![alt [Guide](docs/guide/index.md)](preview.png)\n",
            ),
        ],
    );
}

/// 38. badge link wrapping an image still counts
#[test]
fn badge_link_wrapping_an_image_still_counts() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[![CI](badge.svg)](https://ci.example)\n",
                    "\n",
                    "[![docs](d.svg)](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 39. link in an inline HTML attribute is not an index link
#[test]
fn link_in_an_inline_html_attribute_is_not_an_index_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "Docs <span title=\"[Guide](docs/guide/index.md)\">here</span>.\n",
            ),
        ],
    );
}

/// 40. link beside inline HTML still counts
#[test]
fn link_beside_inline_html_still_counts() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "See <b>the</b> [Guide index](docs/guide/index.md) for everything.\n",
            ),
        ],
    );
}

/// 41. comment opener inside a code span is not a comment
#[test]
fn comment_opener_inside_a_code_span_is_not_a_comment() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "Write `<!--` literally.\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 42. backtick inside a comment does not open a span
#[test]
fn backtick_inside_a_comment_does_not_open_a_span() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "<!-- note: ` -->\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "see `x` here\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 43. autolink is not an HTML block opener
#[test]
fn autolink_is_not_an_html_block_opener() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "<https://example.com>\n",
                    "- [A](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 44. div still opens an HTML block
#[test]
fn div_still_opens_an_html_block() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "<div>\n",
                    "- [B](beta.md)\n",
                    "</div>\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 45. link inside a reference image label is not a link
#[test]
fn link_inside_a_reference_image_label_is_not_a_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "![alt [Guide](docs/guide/index.md)][preview]\n",
                    "\n",
                    "[preview]: p.png\n"
                ),
            ),
        ],
    );
}

/// 46. shortcut image label keeps its real link
#[test]
fn shortcut_image_label_keeps_its_real_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "![see [Guide](docs/guide/index.md)]\n"),
        ],
    );
}

/// 47. multi-line inline tag hides its attributes
#[test]
fn multi_line_inline_tag_hides_its_attributes() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Docs <span\n",
                    "title=\"[Guide](docs/guide/index.md)\">here</span>\n"
                ),
            ),
        ],
    );
}

/// 48. unterminated tag blanks nothing
#[test]
fn unterminated_tag_blanks_nothing() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "A <b is unfinished\n",
                    "\n",
                    "See [Guide](docs/guide/index.md).\n"
                ),
            ),
        ],
    );
}

/// 49. indented-code link in README is not an index link
#[test]
fn indented_code_link_in_readme_is_not_an_index_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("Docs:\n", "\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 50. list continuation is not indented code
#[test]
fn list_continuation_is_not_indented_code() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Docs\n",
                    "\n",
                    "- a bullet\n",
                    "    continued [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 51. escaped backtick does not open a span
#[test]
fn escaped_backtick_does_not_open_a_span() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "Escaped \\` then `[Guide](docs/guide/index.md)`\n",
            ),
        ],
    );
}

/// 52. quoted > does not end an inline tag
#[test]
fn quoted_does_not_end_an_inline_tag() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "Docs <span title=\"not a link > [Guide](docs/guide/index.md)\">t</span>\n",
            ),
        ],
    );
}

/// 53. reference link in README reaches the index
#[test]
fn reference_link_in_readme_reaches_the_index() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "See [Guide][catalog].\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 54. unreferenced definition is not a link
#[test]
fn unreferenced_definition_is_not_a_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Nothing links it.\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 55. tag with text after it is not a block opener
#[test]
fn tag_with_text_after_it_is_not_a_block_opener() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "<span>Docs:</span> [Guide](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 56. tab-indented README line is code
#[test]
fn tab_indented_readme_line_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("Docs:\n", "\n", "\t[Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 57. backslash before a span closer still closes it
#[test]
fn backslash_before_a_span_closer_still_closes_it() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "Code `[Guide](docs/guide/index.md)\\`\n"),
        ],
    );
}

/// 58. angle-bracketed destination resolves in a row
#[test]
fn angle_bracketed_destination_resolves_in_a_row() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](<alpha.md>)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 59. angle-bracketed destination resolves in README
#[test]
fn angle_bracketed_destination_resolves_in_readme() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "See [Guide](<docs/guide/index.md>)\n"),
        ],
    );
}

/// 60. reference label collapses whitespace
#[test]
fn reference_label_collapses_whitespace() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "See [Guide][guide   catalog].\n",
                    "\n",
                    "[guide catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 61. HTML block ends at a whitespace-only line
#[test]
fn html_block_ends_at_a_whitespace_only_line() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<div>\n",
                    "some html\n",
                    "   \n",
                    "[Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 62. indented code opens after a heading
#[test]
fn indented_code_opens_after_a_heading() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("# Documentation\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 63. indented code cannot interrupt a paragraph
#[test]
fn indented_code_cannot_interrupt_a_paragraph() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose text\n",
                    "    continued [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 64. unterminated link is not a link
#[test]
fn unterminated_link_is_not_a_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "See [Guide](docs/guide/index.md\n"),
        ],
    );
}

/// 65. row link with a title is still a row
#[test]
fn row_link_with_a_title_is_still_a_row() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md \"Alpha guide\")\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 66. link inside a title is not a link
#[test]
fn link_inside_a_title_is_not_a_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[Other](other.md \"title ) [Guide](docs/guide/index.md)\")\n",
            ),
        ],
    );
}

/// 67. indented code opens after a thematic break
#[test]
fn indented_code_opens_after_a_thematic_break() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("---\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 68. a tag name needs a delimiter to open a block
#[test]
fn a_tag_name_needs_a_delimiter_to_open_a_block() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("<div.class\n", "[Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 69. a real block opener still blanks its block
#[test]
fn a_real_block_opener_still_blanks_its_block() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("<div class=\"x\">\n", "[Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 70. reference inside a title is not a link
#[test]
fn reference_inside_a_title_is_not_a_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[Other](other.md \"see [Guide][catalog]\")\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 71. a reference beside an inline link still counts
#[test]
fn a_reference_beside_an_inline_link_still_counts() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[X](y.md) [Guide][catalog]\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 72. a definition glued to a multiline link defines nothing
#[test]
fn a_definition_glued_to_a_multiline_link_defines_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[X](y.md \"a\n",
                    "b\")\n",
                    "[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 73. a definition after a multiline link survives a gap
#[test]
fn a_definition_after_a_multiline_link_survives_a_gap() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[X](y.md \"a\n",
                    "b\")\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 74. balanced parens do not end a link early
#[test]
fn balanced_parens_do_not_end_a_link_early() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[Other](other(foo).md \"see [Guide][catalog]\")\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 75. a balanced-paren destination still resolves
#[test]
fn a_balanced_paren_destination_still_resolves() {
    case(
        true,
        &["docs/guide/alpha(1).md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha(1).md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 76. a reference-style row is an entry
#[test]
fn a_reference_style_row_is_an_entry() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][alpha]\n",
                    "\n",
                    "[alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 77. inline plus reference row is a duplicate
#[test]
fn inline_plus_reference_row_is_a_duplicate() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [A again][alpha]\n",
                    "\n",
                    "[alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 78. an undefined label is not an entry
#[test]
fn an_undefined_label_is_not_an_entry() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A][nosuch]\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 79. a mid-row reference is prose, not an entry
#[test]
fn a_mid_row_reference_is_prose_not_an_entry() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- see the [alpha] page\n",
                    "\n",
                    "[alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 80. a definition inside a title is not a definition
#[test]
fn a_definition_inside_a_title_is_not_a_definition() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[Other](other.md \"title\n",
                    "[a]: alpha.md\n",
                    "\")\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 81. the first definition of a label wins
#[test]
fn the_first_definition_of_a_label_wins() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]: ghost.md\n",
                    "[a]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 82. a later duplicate definition is ignored
#[test]
fn a_later_duplicate_definition_is_ignored() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]: alpha.md\n",
                    "[a]: ghost.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 83. a fragment in a definition is stripped
#[test]
fn a_fragment_in_a_definition_is_stripped() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]: alpha.md#section\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 84. an angled destination may carry a fragment
#[test]
fn an_angled_destination_may_carry_a_fragment() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](<alpha.md#top>)\n",
                    "- [B][b]\n",
                    "\n",
                    "[b]: <beta.md#top>\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 85. a bare # prefix is not a heading
#[test]
fn a_bare_prefix_is_not_a_heading() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("#not-a-heading\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 86. a real heading still ends a paragraph
#[test]
fn a_real_heading_still_ends_a_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("# Heading\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 87. a definition cannot cross a blank line
#[test]
fn a_definition_cannot_cross_a_blank_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]:\n",
                    "\n",
                    "alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 88. a definition may use one line ending
#[test]
fn a_definition_may_use_one_line_ending() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]:\n",
                    "alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 89. a type-7 tag cannot interrupt a paragraph
#[test]
fn a_type_7_tag_cannot_interrupt_a_paragraph() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("Some prose\n", "<span>\n", "[Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 90. a type-7 tag after a blank line opens a block
#[test]
fn a_type_7_tag_after_a_blank_line_opens_a_block() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "\n",
                    "<span>\n",
                    "[Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 91. a type-6 tag does interrupt a paragraph
#[test]
fn a_type_6_tag_does_interrupt_a_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("Some prose\n", "<div>\n", "[Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 92. trailing garbage is not a definition
#[test]
fn trailing_garbage_is_not_a_definition() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]: alpha.md trailing garbage\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 93. a definition may carry a title
#[test]
fn a_definition_may_carry_a_title() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]: alpha.md \"Alpha guide\"\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 94. a level-one heading ends the section
#[test]
fn a_level_one_heading_ends_the_section() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "# Appendix\n",
                    "\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 95. a level-three subheading keeps the section
#[test]
fn a_level_three_subheading_keeps_the_section() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "### Sub\n",
                    "\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 96. a type-7 tag does not end the paragraph
#[test]
fn a_type_7_tag_does_not_end_the_paragraph() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "<span>\n",
                    "    [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 97. a type-6 tag does end the paragraph
#[test]
fn a_type_6_tag_does_end_the_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "<div>\n",
                    "    [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 98. a bare # ends the section
#[test]
fn a_bare_ends_the_section() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "#\n",
                    "\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 99. a Setext level-one heading ends the section
#[test]
fn a_setext_level_one_heading_ends_the_section() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "Appendix\n",
                    "===\n",
                    "\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 100. a bare === is text, not a heading
#[test]
fn a_bare_is_text_not_a_heading() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "===\n",
                    "\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 101. two backslashes still open an image
#[test]
fn two_backslashes_still_open_an_image() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "\\\\![alt [Guide](docs/guide/index.md)](preview.png)\n",
            ),
        ],
    );
}

/// 102. one backslash escapes an image opener
#[test]
fn one_backslash_escapes_an_image_opener() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "\\![alt](preview.png) [Guide](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 103. a link cannot cross a blank line
#[test]
fn a_link_cannot_cross_a_blank_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide](docs/guide/index.md\n", "\n", ")\n"),
            ),
        ],
    );
}

/// 104. a link may span one line ending
#[test]
fn a_link_may_span_one_line_ending() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", concat!("[Guide](docs/guide/index.md\n", ")\n")),
        ],
    );
}

/// 105. two backslashes leave a live link
#[test]
fn two_backslashes_leave_a_live_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "\\\\[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 106. one backslash escapes a link opener
#[test]
fn one_backslash_escapes_a_link_opener() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "\\[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 107. a literal block owns its closing line
#[test]
fn a_literal_block_owns_its_closing_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "<pre></pre> [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 108. a link after the closing line is clickable
#[test]
fn a_link_after_the_closing_line_is_clickable() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("<pre></pre>\n", "\n", "[Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 109. a comment block owns its closing line
#[test]
fn a_comment_block_owns_its_closing_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "<!-- hidden --> [Guide](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 110. a mid-line comment keeps the link
#[test]
fn a_mid_line_comment_keeps_the_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "see <!-- x --> [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 111. a declaration block owns its closing line
#[test]
fn a_declaration_block_owns_its_closing_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "<!DOCTYPE html> [Guide](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 112. a link in a definition title is not a link
#[test]
fn a_link_in_a_definition_title_is_not_a_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[other]: other.md \"title [Guide](docs/guide/index.md)\"\n",
            ),
        ],
    );
}

/// 113. a definition used by a reference still counts
#[test]
fn a_definition_used_by_a_reference_still_counts() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide][c]\n", "\n", "[c]: docs/guide/index.md\n"),
            ),
        ],
    );
}

/// 114. a row inside a definition title is not a row
#[test]
fn a_row_inside_a_definition_title_is_not_a_row() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "[c]: o.md \"t\n",
                    "- [B](beta.md)\n",
                    "\"\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 115. a next-line title belongs to the definition
#[test]
fn a_next_line_title_belongs_to_the_definition() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "[c]: other.md\n",
                    "\"title\n",
                    "- [B](beta.md)\n",
                    "\"\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 116. a blank line ends the definition
#[test]
fn a_blank_line_ends_the_definition() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "- [B](beta.md)\n",
                    "\n",
                    "[a]: alpha.md\n",
                    "\n",
                    "\"T\"\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 117. two backslashes leave a live reference
#[test]
fn two_backslashes_leave_a_live_reference() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("\\\\[Guide][]\n", "\n", "[Guide]: docs/guide/index.md\n"),
            ),
        ],
    );
}

/// 118. one backslash escapes a reference opener
#[test]
fn one_backslash_escapes_a_reference_opener() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("\\[Guide][]\n", "\n", "[Guide]: docs/guide/index.md\n"),
            ),
        ],
    );
}

/// 119. a label in its own title is not a use
#[test]
fn a_label_in_its_own_title_is_not_a_use() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[catalog]: docs/guide/index.md \"title [catalog]\"\n",
            ),
        ],
    );
}

/// 120. a real reference use still counts
#[test]
fn a_real_reference_use_still_counts() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("see [catalog]\n", "\n", "[catalog]: docs/guide/index.md\n"),
            ),
        ],
    );
}

/// 121. a label inside an autolink is not a use
#[test]
fn a_label_inside_an_autolink_is_not_a_use() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<https://example.com/[catalog]>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 122. an autolink hides neither destination nor link
#[test]
fn an_autolink_hides_neither_destination_nor_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](<alpha.md>)\n"),
            ),
            (
                "README.md",
                "<https://example.com> [Guide](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 123. an escaped < is not an autolink
#[test]
fn an_escaped_is_not_an_autolink() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "\\<https://example.com/[catalog]>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 124. an unescaped < still opens an autolink
#[test]
fn an_unescaped_still_opens_an_autolink() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<https://example.com/[catalog]>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 125. balanced brackets in link text are a row
#[test]
fn balanced_brackets_in_link_text_are_a_row() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A [advanced]](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 126. unbalanced brackets are not link text
#[test]
fn unbalanced_brackets_are_not_link_text() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A [x](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 127. an escaped delimiter is title content
#[test]
fn an_escaped_delimiter_is_title_content() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[Other](other.md \"title \\\" ) [Guide](docs/guide/index.md)\")\n",
            ),
        ],
    );
}

/// 128. an ordinary title still closes
#[test]
fn an_ordinary_title_still_closes() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[Guide](docs/guide/index.md \"T\") and more text\n",
            ),
        ],
    );
}

/// 129. balanced text in a reference row is a row
#[test]
fn balanced_text_in_a_reference_row_is_a_row() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A [advanced]][alpha]\n",
                    "\n",
                    "[alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 130. a nested-text row with no definition is not an entry
#[test]
fn a_nested_text_row_with_no_definition_is_not_an_entry() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A [advanced]][nosuch]\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 131. a lowercase <!foo> is not a declaration
#[test]
fn a_lowercase_foo_is_not_a_declaration() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "<!foo> [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 132. brackets nest to any depth
#[test]
fn brackets_nest_to_any_depth() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A [one [two]]](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 133. a deep reference row is a row
#[test]
fn a_deep_reference_row_is_a_row() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A [one [two]]][alpha]\n",
                    "\n",
                    "[alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 134. unbalanced nesting is not link text
#[test]
fn unbalanced_nesting_is_not_link_text() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A [one [two](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 135. invalid tag syntax is literal text
#[test]
fn invalid_tag_syntax_is_literal_text() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<span = [catalog]>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 136. a label inside an attribute is not a reference
#[test]
fn a_label_inside_an_attribute_is_not_a_reference() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<span title=\"[catalog]\">\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 137. escaped, encoded and queried destinations resolve
#[test]
fn escaped_encoded_and_queried_destinations_resolve() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/gamma.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha\\.md)\n",
                    "- [B](beta%2Emd)\n",
                    "- [C](gamma.md?plain=1)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 138. parentheses nest in a destination
#[test]
fn parentheses_nest_in_a_destination() {
    case(
        true,
        &["docs/guide/a((b)).md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](a((b)).md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 139. an unbalanced paren is not a destination
#[test]
fn an_unbalanced_paren_is_not_a_destination() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](a(b.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 140. a linked-image row targets the page
#[test]
fn a_linked_image_row_targets_the_page() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/img.png",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [![badge](img.png)](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 141. an escaped bracket stays inside link text
#[test]
fn an_escaped_bracket_stays_inside_link_text() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [closing \\]](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 142. a single-quoted title on a row is a row
#[test]
fn a_single_quoted_title_on_a_row_is_a_row() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md 'why')\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 143. link text cannot cross a blank line
#[test]
fn link_text_cannot_cross_a_blank_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide\n", "\n", "text](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 144. link text may span one line ending
#[test]
fn link_text_may_span_one_line_ending() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide\n", "text](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 145. a label cannot cross a blank line
#[test]
fn a_label_cannot_cross_a_blank_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[Guide][cat\n",
                    "\n",
                    "alog]\n",
                    "\n",
                    "[cat alog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 146. a label folds with full Unicode case folding
#[test]
fn a_label_folds_with_full_unicode_case_folding() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide][ẞ]\n", "\n", "[ss]: docs/guide/index.md\n"),
            ),
        ],
    );
}

/// 147. a 1000-character label is not a label
#[test]
fn a_1000_character_label_is_not_a_label() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[Guide][aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]\n",
                    "\n",
                    "[aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 148. a definition destination must balance parens
#[test]
fn a_definition_destination_must_balance_parens() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[x]: docs/guide/index.md#(unterminated\n",
                    "\n",
                    "[Guide][x]\n"
                ),
            ),
        ],
    );
}

/// 149. a link inside link text deactivates the outer
#[test]
fn a_link_inside_link_text_deactivates_the_outer() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [[x](beta.md)](alpha.md)\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 150. an image inside link text does not deactivate it
#[test]
fn an_image_inside_link_text_does_not_deactivate_it() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/img.png",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [![badge](img.png)](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 151. a definition glued to a paragraph defines nothing
#[test]
fn a_definition_glued_to_a_paragraph_defines_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 152. a definition after a blank line still defines
#[test]
fn a_definition_after_a_blank_line_still_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 153. a resolved inner reference deactivates the outer
#[test]
fn a_resolved_inner_reference_deactivates_the_outer() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[outer [Other][o]](docs/guide/index.md)\n",
                    "\n",
                    "[o]: other.md\n"
                ),
            ),
        ],
    );
}

/// 154. an unresolved inner reference does not deactivate
#[test]
fn an_unresolved_inner_reference_does_not_deactivate() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[outer [Other][nope]](docs/guide/index.md)\n"),
        ],
    );
}

/// 155. a code span cannot cross a blank line
#[test]
fn a_code_span_cannot_cross_a_blank_line() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("`opener\n", "\n", "[Guide](docs/guide/index.md) `\n"),
            ),
        ],
    );
}

/// 156. an unresolved reference image is literal
#[test]
fn an_unresolved_reference_image_is_literal() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "![alt [Guide](docs/guide/index.md)][missing]\n",
            ),
        ],
    );
}

/// 157. a resolved reference image hides its alt text
#[test]
fn a_resolved_reference_image_hides_its_alt_text() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "![alt [Guide](docs/guide/index.md)][found]\n",
                    "\n",
                    "[found]: other.png\n"
                ),
            ),
        ],
    );
}

/// 158. only a real definition is blanked
#[test]
fn only_a_real_definition_is_blanked() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "[c]: other.md \"title [Guide](docs/guide/index.md)\"\n"
                ),
            ),
        ],
    );
}

/// 159. an invalid fence is not a block boundary
#[test]
fn an_invalid_fence_is_not_a_block_boundary() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "```md`invalid\n",
                    "[c]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][c]\n"
                ),
            ),
        ],
    );
}

/// 160. a definition chain glued to prose defines nothing
#[test]
fn a_definition_chain_glued_to_prose_defines_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "[a]: other.md\n",
                    "[c]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][c]\n"
                ),
            ),
        ],
    );
}

/// 161. a definition chain after a blank line defines
#[test]
fn a_definition_chain_after_a_blank_line_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Some prose\n",
                    "\n",
                    "[a]: other.md\n",
                    "[c]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][c]\n"
                ),
            ),
        ],
    );
}

/// 162. a malformed image is not an image
#[test]
fn a_malformed_image_is_not_an_image() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "![alt [Guide](docs/guide/index.md)](not a valid dest)\n",
            ),
        ],
    );
}

/// 163. a valid image still masks its alt text
#[test]
fn a_valid_image_still_masks_its_alt_text() {
    case(
        false,
        &[
            "docs/guide/alpha.md",
            "docs/guide/img.png",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "![alt [Guide](docs/guide/index.md)](img.png)\n",
            ),
        ],
    );
}

/// 164. a root-relative destination does not resolve
#[test]
fn a_root_relative_destination_does_not_resolve() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide](/docs/guide/index.md)\n"),
        ],
    );
}

/// 165. a ./ prefix still resolves
#[test]
fn a_prefix_still_resolves() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide](./docs/guide/index.md)\n"),
        ],
    );
}

/// 166. an escape does not swallow a blank line
#[test]
fn an_escape_does_not_swallow_a_blank_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide\\\n", "\n", "continued](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 167. an empty link label is not an entry
#[test]
fn an_empty_link_label_is_not_an_entry() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 168. an image label is rendered content
#[test]
fn an_image_label_is_rendered_content() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/img.png",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [![badge](img.png)](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 169. a fenced definition does not resolve
#[test]
fn a_fenced_definition_does_not_resolve() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "```\n",
                    "[missing]: other.md\n",
                    "```\n",
                    "\n",
                    "![alt [Guide](docs/guide/index.md)][missing]\n"
                ),
            ),
        ],
    );
}

/// 170. a malformed angle destination defines nothing
#[test]
fn a_malformed_angle_destination_defines_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide][x]\n", "\n", "[x]: <docs/guide/index.md?>>\n"),
            ),
        ],
    );
}

/// 171. a well-formed angle destination resolves
#[test]
fn a_well_formed_angle_destination_resolves() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide][x]\n", "\n", "[x]: <docs/guide/index.md>\n"),
            ),
        ],
    );
}

/// 172. an unescaped bracket is not a label
#[test]
fn an_unescaped_bracket_is_not_a_label() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [Alpha][a[b]\n",
                    "\n",
                    "[a[b]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 173. an escaped bracket is label content
#[test]
fn an_escaped_bracket_is_label_content() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [Alpha][a\\[b]\n",
                    "\n",
                    "[a\\[b]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 174. a backslash before a letter is not an escape
#[test]
fn a_backslash_before_a_letter_is_not_an_escape() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide](docs/guide/\\index.md)\n"),
        ],
    );
}

/// 175. any URI scheme is outside the guide
#[test]
fn any_uri_scheme_is_outside_the_guide() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "- [FTP](ftp://example.com/f)\n",
                    "- [X](HTTPS://example.com/x)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 176. one scanner decides what a definition is
#[test]
fn one_scanner_decides_what_a_definition_is() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[outer [Other][o]](docs/guide/index.md)\n",
                    "\n",
                    "[o]: bad(unbalanced\n"
                ),
            ),
        ],
    );
}

/// 177. an empty reference label is not an entry
#[test]
fn an_empty_reference_label_is_not_an_entry() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [][a]\n",
                    "\n",
                    "[a]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 178. an autolink does not deactivate its outer link
#[test]
fn an_autolink_does_not_deactivate_its_outer_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[outer <https://example.com>](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 179. a non-definition keeps its links
#[test]
fn a_non_definition_keeps_its_links() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[]: alpha.md \"t [Guide](docs/guide/index.md)\"\n",
            ),
        ],
    );
}

/// 180. a bare setext underline is not a boundary
#[test]
fn a_bare_setext_underline_is_not_a_boundary() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("===\n", "[x]: docs/guide/index.md\n", "\n", "[Guide][x]\n"),
            ),
        ],
    );
}

/// 181. a real setext heading is a boundary
#[test]
fn a_real_setext_heading_is_a_boundary() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Title\n",
                    "===\n",
                    "[x]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][x]\n"
                ),
            ),
        ],
    );
}

/// 182. every list marker starts a row
#[test]
fn every_list_marker_starts_a_row() {
    case(
        true,
        &[
            "docs/guide/a.md",
            "docs/guide/b.md",
            "docs/guide/c.md",
            "docs/guide/d.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "* [A](a.md)\n",
                    "+ [B](b.md)\n",
                    "1000. [C](c.md)\n",
                    "-  [D](d.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 183. five spaces after a marker is code, not a row
#[test]
fn five_spaces_after_a_marker_is_code_not_a_row() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "-     [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 184. an unmatched inline comment stays literal
#[test]
fn an_unmatched_inline_comment_stays_literal() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose <!-- unmatched\n",
                    "\n",
                    "[Guide index](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 185. a closer past a blank line closes nothing
#[test]
fn a_closer_past_a_blank_line_closes_nothing() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose <!-- a\n",
                    "\n",
                    "b --> [Guide index](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 186. a comment closed on the next line is a comment
#[test]
fn a_comment_closed_on_the_next_line_is_a_comment() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose <!-- a\n",
                    "b --> [Guide index](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 187. a link inside a closed comment is not a route
#[test]
fn a_link_inside_a_closed_comment_is_not_a_route() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "prose <!-- [Guide index](docs/guide/index.md) --> tail\n",
            ),
        ],
    );
}

/// 188. a quoted fence is a fence
#[test]
fn a_quoted_fence_is_a_fence() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> ~~~markdown\n",
                    "> [Guide index](docs/guide/index.md)\n",
                    "> ~~~\n"
                ),
            ),
        ],
    );
}

/// 189. a quoted fence outlives a short closer
#[test]
fn a_quoted_fence_outlives_a_short_closer() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> ````markdown\n",
                    "> [Guide index](docs/guide/index.md)\n",
                    "> ```\n"
                ),
            ),
        ],
    );
}

/// 190. an unclosed quoted fence ends with its quote
#[test]
fn an_unclosed_quoted_fence_ends_with_its_quote() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> ```md\n",
                    "> x\n",
                    "\n",
                    "[Guide index](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 191. a link in quoted prose is a link
#[test]
fn a_link_in_quoted_prose_is_a_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "> see [Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 192. a quoted html block is a block
#[test]
fn a_quoted_html_block_is_a_block() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("> <div>\n", "> [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 193. quoted indentation is measured inside the quote
#[test]
fn quoted_indentation_is_measured_inside_the_quote() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", ">     [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 194. a quoted html block ends with its quote
#[test]
fn a_quoted_html_block_ends_with_its_quote() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("> <div>\n", "\n", "[Guide index](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 195. an image label stops at a blank line
#[test]
fn an_image_label_stops_at_a_blank_line() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "![alt\n",
                    "\n",
                    "[Guide](docs/guide/index.md)](image.png)\n"
                ),
            ),
        ],
    );
}

/// 196. a real image still masks its alt text
#[test]
fn a_real_image_still_masks_its_alt_text() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "![alt [Guide](docs/guide/index.md)](image.png)\n",
            ),
        ],
    );
}

/// 197. masking a link invents no block boundary
#[test]
fn masking_a_link_invents_no_block_boundary() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[ordinary](other.md)\n",
                    "[x]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][x]\n"
                ),
            ),
        ],
    );
}

/// 198. a definition after a blank line still defines
#[test]
fn a_definition_after_a_blank_line_still_defines_2() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[ordinary](other.md)\n",
                    "\n",
                    "[x]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][x]\n"
                ),
            ),
        ],
    );
}

/// 199. a quoted comment ends with its quote
#[test]
fn a_quoted_comment_ends_with_its_quote() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> <!-- unclosed\n",
                    "\n",
                    "[Guide index](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 200. an unquoted comment block still runs to EOF
#[test]
fn an_unquoted_comment_block_still_runs_to_eof() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<!-- unclosed\n",
                    "\n",
                    "[Guide index](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 201. a link inside a quoted comment is not a route
#[test]
fn a_link_inside_a_quoted_comment_is_not_a_route() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "> <!-- [Guide index](docs/guide/index.md) -->\n",
            ),
        ],
    );
}

/// 202. angle destinations ignore their padding
#[test]
fn angle_destinations_ignore_their_padding() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](< alpha.md>)\n"),
            ),
            ("README.md", "[Guide](<docs/guide/index.md >)\n"),
        ],
    );
}

/// 203. a space inside a destination is part of the name
#[test]
fn a_space_inside_a_destination_is_part_of_the_name() {
    case(
        true,
        &["docs/guide/index.md", "docs/guide/my file.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](<my file.md>)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 204. a definition after an html block is real
#[test]
fn a_definition_after_an_html_block_is_real() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<pre></pre>\n",
                    "[img]: pic.png\n",
                    "\n",
                    "![alt [Guide](docs/guide/index.md)][img]\n"
                ),
            ),
        ],
    );
}

/// 205. a reference after an html block reaches
#[test]
fn a_reference_after_an_html_block_reaches() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<pre></pre>\n",
                    "[x]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][x]\n"
                ),
            ),
        ],
    );
}

/// 206. a code span is visible link text
#[test]
fn a_code_span_is_visible_link_text() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [`alpha`](alpha.md)\n"),
            ),
            ("README.md", "[`Guide`](docs/guide/index.md)\n"),
        ],
    );
}

/// 207. a blank code span is not content
#[test]
fn a_blank_code_span_is_not_content() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[` `](docs/guide/index.md)\n"),
        ],
    );
}

/// 208. an angle definition may hold a space
#[test]
fn an_angle_definition_may_hold_a_space() {
    case(
        true,
        &["docs/guide/index.md", "docs/guide/my file.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]\n",
                    "\n",
                    "[a]: <my file.md>\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 209. a whitespace-only label is not a label
#[test]
fn a_whitespace_only_label_is_not_a_label() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][ ]\n",
                    "\n",
                    "[ ]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 210. a whitespace-only label reaches nothing
#[test]
fn a_whitespace_only_label_reaches_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[Guide][ ]\n", "\n", "[ ]: docs/guide/index.md\n"),
            ),
        ],
    );
}

/// 211. a folded label still resolves
#[test]
fn a_folded_label_still_resolves() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[Guide][big  catalog]\n",
                    "\n",
                    "[BIG CATALOG]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 212. a tab is list-marker padding
#[test]
fn a_tab_is_list_marker_padding() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "-\t[Alpha](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 213. two tabs after a marker is code
#[test]
fn two_tabs_after_a_marker_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "-\t\t[Alpha](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 214. a shortcut before a colon is still a link
#[test]
fn a_shortcut_before_a_colon_is_still_a_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [Alpha]: the page\n",
                    "\n",
                    "[Alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 215. a definition row lists nothing
#[test]
fn a_definition_row_lists_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [Alpha]: alpha.md\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 216. a backslash before a space is not an escape
#[test]
fn a_backslash_before_a_space_is_not_an_escape() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide](docs/guide/index.md?\\ foo)\n"),
        ],
    );
}

/// 217. tab padding is counted in columns
#[test]
fn tab_padding_is_counted_in_columns() {
    case(
        true,
        &[
            "docs/guide/a.md",
            "docs/guide/b.md",
            "docs/guide/c.md",
            "docs/guide/d.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "-\t[A](a.md)\n",
                    "-  \t[B](b.md)\n",
                    "10.\t[C](c.md)\n",
                    "- [D](d.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 218. a tab past column four is code
#[test]
fn a_tab_past_column_four_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "-   \t[A](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 219. tab padding depends on marker width
#[test]
fn tab_padding_depends_on_marker_width() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "10. \t[A](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 220. a quoted definition defines
#[test]
fn a_quoted_definition_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> [catalog]: docs/guide/index.md\n",
                    ">\n",
                    "> [Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 221. a quoted definition reaches outside its quote
#[test]
fn a_quoted_definition_reaches_outside_its_quote() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 222. a definition glued to quoted prose defines nothing
#[test]
fn a_definition_glued_to_quoted_prose_defines_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> prose\n",
                    "> [a]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][a]\n"
                ),
            ),
        ],
    );
}

/// 223. a definition in a quoted fence defines nothing
#[test]
fn a_definition_in_a_quoted_fence_defines_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> ```\n",
                    "> [a]: docs/guide/index.md\n",
                    "> ```\n",
                    "\n",
                    "[Guide][a]\n"
                ),
            ),
        ],
    );
}

/// 224. a list-item definition defines
#[test]
fn a_list_item_definition_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "- [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 225. a shortcut row survives the list prefix
#[test]
fn a_shortcut_row_survives_the_list_prefix() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [Alpha]: the page\n",
                    "\n",
                    "[Alpha]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 226. a definition past column four is code
#[test]
fn a_definition_past_column_four_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "-\t\t[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 227. a definition one tab in still defines
#[test]
fn a_definition_one_tab_in_still_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "-\t[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 228. every level-two heading opens a section
#[test]
fn every_level_two_heading_opens_a_section() {
    case(
        true,
        &[
            "docs/guide/alpha.md",
            "docs/guide/beta.md",
            "docs/guide/index.md",
        ],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "   ## Indented\n",
                    "\n",
                    "- [A](alpha.md)\n",
                    "\n",
                    "##\tTabbed\n",
                    "\n",
                    "- [B](beta.md)\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 229. a level-three heading opens no section
#[test]
fn a_level_three_heading_opens_no_section() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "### Sub\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 230. an entity that renders blank is not content
#[test]
fn an_entity_that_renders_blank_is_not_content() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [&#32;](alpha.md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 231. a character reference decodes before the fragment
#[test]
fn a_character_reference_decodes_before_the_fragment() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha&#46;md)\n"),
            ),
            ("README.md", "[Guide](docs/guide/index&#46;md)\n"),
        ],
    );
}

/// 232. container prefixes compose in any order
#[test]
fn container_prefixes_compose_in_any_order() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "- > [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 233. an unterminated reference does not decode
#[test]
fn an_unterminated_reference_does_not_decode() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha&#46md)\n"),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 234. every nested container's padding counts
#[test]
fn every_nested_container_s_padding_counts() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "-\t\t- [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 235. a properly nested definition defines
#[test]
fn a_properly_nested_definition_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "- - [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 236. a quoted attribute stops at a blank line
#[test]
fn a_quoted_attribute_stops_at_a_blank_line() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose <span title=\"\n",
                    "\n",
                    "[Guide](docs/guide/index.md)\">\n"
                ),
            ),
        ],
    );
}

/// 237. a quoted attribute may cross one newline
#[test]
fn a_quoted_attribute_may_cross_one_newline() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose <span title=\"a\n",
                    "[Guide](docs/guide/index.md)\">\n"
                ),
            ),
        ],
    );
}

/// 238. a full reference survives a colon
#[test]
fn a_full_reference_survives_a_colon() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A][a]: the page\n",
                    "\n",
                    "[a]: alpha.md\n"
                ),
            ),
            ("README.md", "[Guide index](docs/guide/index.md)\n"),
        ],
    );
}

/// 239. a paren after a full reference finds the inline link
#[test]
fn a_paren_after_a_full_reference_finds_the_inline_link() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[x][y](docs/guide/index.md)\n"),
        ],
    );
}

/// 240. a bare setext keeps its paragraph open
#[test]
fn a_bare_setext_keeps_its_paragraph_open() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("===\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 241. a real setext heading still ends the paragraph
#[test]
fn a_real_setext_heading_still_ends_the_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "Title\n",
                    "===\n",
                    "\n",
                    "    [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 242. a quoted line four columns in is code
#[test]
fn a_quoted_line_four_columns_in_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", ">     [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 243. a quoted line three columns in is a paragraph
#[test]
fn a_quoted_line_three_columns_in_is_a_paragraph() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", ">    [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 244. a definition cannot interrupt a paragraph
#[test]
fn a_definition_cannot_interrupt_a_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[X](y.md \"a\n",
                    "b\")\n",
                    "[catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 245. a row's link may cross a line break
#[test]
fn a_row_s_link_may_cross_a_line_break() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A long title,\n",
                    "  and the rest](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 246. a row's link may not cross a blank line
#[test]
fn a_row_s_link_may_not_cross_a_blank_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A long title,\n",
                    "\n",
                    "  and the rest](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 247. a heading interrupts a row's link
#[test]
fn a_heading_interrupts_a_row_s_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A\n",
                    "# interrupted](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 248. the next list item interrupts a row's link
#[test]
fn the_next_list_item_interrupts_a_row_s_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!(
                    "# Guide\n",
                    "\n",
                    "## S\n",
                    "\n",
                    "- [A\n",
                    "- B](alpha.md)\n"
                ),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 249. an ordered list at 1 interrupts a paragraph
#[test]
fn an_ordered_list_at_1_interrupts_a_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[A title\n", "1. was the year](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 250. an ordered list at 2 does not interrupt
#[test]
fn an_ordered_list_at_2_does_not_interrupt() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("[A title\n", "2. was the year](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 251. six spaces in a list item is code
#[test]
fn six_spaces_in_a_list_item_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("- Example:\n", "\n", "      [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 252. four spaces in a list item is prose
#[test]
fn four_spaces_in_a_list_item_is_prose() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("- Example:\n", "\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 253. seven spaces under an ordered item is code
#[test]
fn seven_spaces_under_an_ordered_item_is_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "1. Example:\n",
                    "\n",
                    "       [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 254. four spaces at the margin is still code
#[test]
fn four_spaces_at_the_margin_is_still_code() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("Example:\n", "\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 255. a line below the content column leaves the item
#[test]
fn a_line_below_the_content_column_leaves_the_item() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("100. item\n", "\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 256. a definition opening a quote defines
#[test]
fn a_definition_opening_a_quote_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose\n",
                    "> [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 257. a definition opening a list item defines
#[test]
fn a_definition_opening_a_list_item_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "- item\n",
                    "- [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 258. a definition in a deeper quote defines
#[test]
fn a_definition_in_a_deeper_quote_defines() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> prose\n",
                    "> > [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 259. an ordered marker at 2 opens no block
#[test]
fn an_ordered_marker_at_2_opens_no_block() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "prose\n",
                    "2. [catalog]: docs/guide/index.md\n",
                    "\n",
                    "[Guide][catalog]\n"
                ),
            ),
        ],
    );
}

/// 260. an overpadded marker puts code on its line
#[test]
fn an_overpadded_marker_puts_code_on_its_line() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "-     [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 261. four spaces of padding is still content
#[test]
fn four_spaces_of_padding_is_still_content() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "-    [Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 262. a fence at the item content column opens
#[test]
fn a_fence_at_the_item_content_column_opens() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "100. Example:\n",
                    "\n",
                    "     ~~~md\n",
                    "     [Guide](docs/guide/index.md)\n",
                    "     ~~~\n"
                ),
            ),
        ],
    );
}

/// 263. a fence in a list item closes at that column
#[test]
fn a_fence_in_a_list_item_closes_at_that_column() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "100. Example:\n",
                    "\n",
                    "     ~~~md\n",
                    "     x\n",
                    "     ~~~\n",
                    "\n",
                    "[Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 264. a one-character scheme is not an autolink
#[test]
fn a_one_character_scheme_is_not_an_autolink() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<x:[Guide][catalog]>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 265. brackets disqualify an email autolink
#[test]
fn brackets_disqualify_an_email_autolink() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<x[Guide][catalog]@e.co>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 266. a real autolink still hides its brackets
#[test]
fn a_real_autolink_still_hides_its_brackets() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "<https://e.co/[catalog]>\n",
                    "\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 267. a short comment closes itself
#[test]
fn a_short_comment_closes_itself() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "prose <!--> [Guide](docs/guide/index.md) -->\n",
            ),
        ],
    );
}

/// 268. a processing instruction hides its contents
#[test]
fn a_processing_instruction_hides_its_contents() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "prose <?x [Guide](docs/guide/index.md)?>\n"),
        ],
    );
}

/// 269. a code span does not close an image label
#[test]
fn a_code_span_does_not_close_an_image_label() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "![alt `]` [Guide](docs/guide/index.md)](pic.png)\n",
            ),
        ],
    );
}

/// 270. paired emphasis around a space renders nothing
#[test]
fn paired_emphasis_around_a_space_renders_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[*&#32;*](docs/guide/index.md)\n"),
        ],
    );
}

/// 271. an invisible row lists nothing
#[test]
fn an_invisible_row_lists_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [*&#32;*](alpha.md)\n"),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 272. an unpaired delimiter run is visible text
#[test]
fn an_unpaired_delimiter_run_is_visible_text() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[***](docs/guide/index.md)\n"),
        ],
    );
}

/// 273. a lone delimiter is visible text
#[test]
fn a_lone_delimiter_is_visible_text() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[*](docs/guide/index.md)\n"),
        ],
    );
}

/// 274. a delimiter against whitespace stays visible
#[test]
fn a_delimiter_against_whitespace_stays_visible() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[* *](docs/guide/index.md)\n"),
        ],
    );
}

/// 275. a visible delimiter row still lists
#[test]
fn a_visible_delimiter_row_still_lists() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [* *](alpha.md)\n"),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 276. strong emphasis flanks the same way
#[test]
fn strong_emphasis_flanks_the_same_way() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[** **](docs/guide/index.md)\n"),
        ],
    );
}

/// 277. a list-contained html block opens
#[test]
fn a_list_contained_html_block_opens() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "100. item\n",
                    "\n",
                    "     <pre>\n",
                    "     [Guide](docs/guide/index.md)\n",
                    "     </pre>\n"
                ),
            ),
        ],
    );
}

/// 278. a list-contained paragraph still links
#[test]
fn a_list_contained_paragraph_still_links() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("100. item\n", "\n", "     [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 279. a tab can overpad a marker too
#[test]
fn a_tab_can_overpad_a_marker_too() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "-   \t[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 280. one tab of padding is still content
#[test]
fn one_tab_of_padding_is_still_content() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "-\t[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 281. a short comment closes itself in block position
#[test]
fn a_short_comment_closes_itself_in_block_position() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("<!-->\n", "[Guide](docs/guide/index.md)\n", "prose -->\n"),
            ),
        ],
    );
}

/// 282. a real block comment still hides its link
#[test]
fn a_real_block_comment_still_hides_its_link() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("<!-- x\n", "[Guide](docs/guide/index.md)\n", "-->\n"),
            ),
        ],
    );
}

/// 283. a dedent lands in the enclosing item
#[test]
fn a_dedent_lands_in_the_enclosing_item() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "100. outer\n",
                    "\n",
                    "     - inner\n",
                    "\n",
                    "     [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 284. a dedent past the outermost column leaves the list
#[test]
fn a_dedent_past_the_outermost_column_leaves_the_list() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("100. item\n", "\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 285. a heading at the content column ends the paragraph
#[test]
fn a_heading_at_the_content_column_ends_the_paragraph() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "100. item\n",
                    "\n",
                    "     # heading\n",
                    "\n",
                    "         [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 286. a deactivated outer leaves a live shortcut
#[test]
fn a_deactivated_outer_leaves_a_live_shortcut() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[outer [B][beta]][catalog]\n",
                    "\n",
                    "[beta]: beta.md\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 287. a second label blocks the shortcut
#[test]
fn a_second_label_blocks_the_shortcut() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[outer [B][beta]][catalog][beta]\n",
                    "\n",
                    "[beta]: beta.md\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 288. an inline tail blocks the shortcut
#[test]
fn an_inline_tail_blocks_the_shortcut() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "[outer [B][beta]][catalog](other.md)\n",
                    "\n",
                    "[beta]: beta.md\n",
                    "[catalog]: docs/guide/index.md\n"
                ),
            ),
        ],
    );
}

/// 289. a thematic break opens no list item
#[test]
fn a_thematic_break_opens_no_list_item() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!("* * *\n", "\n", "    [Guide](docs/guide/index.md)\n"),
            ),
        ],
    );
}

/// 290. a quoted item's column ends with its quote
#[test]
fn a_quoted_item_s_column_ends_with_its_quote() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> - quoted item\n",
                    "\n",
                    "    [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 291. a quoted item's column holds inside its quote
#[test]
fn a_quoted_item_s_column_holds_inside_its_quote() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                concat!(
                    "> - quoted item\n",
                    ">\n",
                    ">     [Guide](docs/guide/index.md)\n"
                ),
            ),
        ],
    );
}

/// 292. a raw image label is visible content
#[test]
fn a_raw_image_label_is_visible_content() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            (
                "README.md",
                "[<img alt=\"Guide\" src=\"icon.png\">](docs/guide/index.md)\n",
            ),
        ],
    );
}

/// 293. a tag starting with img is not an image
#[test]
fn a_tag_starting_with_img_is_not_an_image() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[<imgx a=\"1\">](docs/guide/index.md)\n"),
        ],
    );
}

/// 294. a zero-width label renders nothing
#[test]
fn a_zero_width_label_renders_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[&#8203;](docs/guide/index.md)\n"),
        ],
    );
}

/// 295. a zero-width row lists nothing
#[test]
fn a_zero_width_row_lists_nothing() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [&#8203;](alpha.md)\n"),
            ),
            ("README.md", "[Guide](docs/guide/index.md)\n"),
        ],
    );
}

/// 296. a byte-order mark renders nothing either
#[test]
fn a_byte_order_mark_renders_nothing_either() {
    case(
        false,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[&#65279;](docs/guide/index.md)\n"),
        ],
    );
}

/// 297. a zero-width beside text keeps the text
#[test]
fn a_zero_width_beside_text_keeps_the_text() {
    case(
        true,
        &["docs/guide/alpha.md", "docs/guide/index.md"],
        &[
            (
                "docs/guide/index.md",
                concat!("# Guide\n", "\n", "## S\n", "\n", "- [A](alpha.md)\n"),
            ),
            ("README.md", "[&#8203;Guide](docs/guide/index.md)\n"),
        ],
    );
}
