//! Markdown link gate: every relative link and heading anchor in the tracked
//! markdown corpus must resolve.
//!
//! `scripts/check-docs.sh` gates rustdoc intra-doc links and
//! `scripts/check-plugin-freshness.sh` gates the `docs/guide/*.md` paths named
//! from `skills/` and `agents/`. Nothing else checked the markdown itself, so
//! a guide could link to a page that was renamed, never written, or lives one
//! directory up. The first run of this gate found 19 such links across 11
//! pages, five of them rustdoc paths pasted into hand-written markdown. A 404
//! is a defect a reader cannot route around, so this is a gate.
//!
//! What it checks:
//!
//! 1. Relative file links resolve to a path that exists, inside the checkout.
//! 2. Heading anchors resolve, in-page (`#section`) and cross-page
//!    (`other.md#section`), against GitHub's slugs.
//! 3. A rustdoc path (`crate::mod::Item`) used as a link target is reported
//!    on its own, because the fix is a docs.rs link, not a path repair.
//!
//! What it does not check: external links (network-dependent, and not fixable
//! here), link-shaped text inside code (samples, not links), and
//! `examples/<app>/content/` (app routes, not file paths).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

use super::corpus;
use super::markdown;
use super::re::unquote;

/// What the gate found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// How many tracked markdown files were read.
    pub corpus: usize,
    /// One line per broken link, sorted.
    pub defects: Vec<String>,
}

impl Report {
    /// True when nothing is broken.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.defects.is_empty()
    }
}

/// Check the tracked markdown under `root`.
///
/// # Errors
///
/// Returns an error when the corpus cannot be listed.
/// Broken links are not errors; they are the [`Report`]'s defects.
pub fn check(root: &Path) -> Result<Report> {
    // Destinations are normalised before they are compared with the root, so
    // the root must be too: `.` or `/repo/xtask/..` would otherwise match no
    // destination, and every link would look like it escapes the checkout.
    let root: &Path = &normalize(
        &std::path::absolute(root).with_context(|| format!("resolving {}", root.display()))?,
    );
    let files = corpus::tracked_markdown(root)?;
    let mut texts: HashMap<&str, String> = HashMap::new();
    let mut defects = Vec::new();
    for f in &files {
        match corpus::read(&root.join(f)) {
            Some(text) => {
                texts.insert(f.as_str(), text);
            }
            // Skipping in silence would leave the file counted while
            // nothing in it is checked.
            None => defects.push(format!(
                "{f}: cannot decode (not UTF-8, and no UTF-16/UTF-8 BOM) — nothing in this file was checked"
            )),
        }
    }

    let mut anchors: HashMap<&str, HashSet<String>> = HashMap::new();
    for f in &files {
        if let Some(text) = texts.get(f.as_str()) {
            anchors.insert(f.as_str(), markdown::anchors(text));
        }
    }
    let empty = HashSet::new();

    for f in &files {
        let Some(text) = texts.get(f.as_str()) else {
            continue;
        };
        let own = anchors.get(f.as_str()).unwrap_or(&empty);
        for t in markdown::link_targets(&markdown::strip_code(text)) {
            if ["http://", "https://", "mailto:", "tel:"]
                .iter()
                .any(|p| t.starts_with(p))
                || t.contains("://")
            {
                continue;
            }
            if markdown::RUSTDOC.is_match(&t) {
                defects.push(format!(
                    "{f}: rustdoc path used as a markdown link target: `{t}` (link to https://docs.rs/... instead)"
                ));
                continue;
            }
            if let Some(fragment) = t.strip_prefix('#') {
                // Fragments are URL-encoded and case-sensitive; GitHub's
                // slugger always lowercases, so `#Section` never reaches
                // `section`.
                in_page(f, &t, &unquote(fragment), own, &mut defects);
                continue;
            }
            let (path, anchor) = t.split_once('#').unwrap_or((t.as_str(), ""));
            let path = path.split('?').next().unwrap_or("");
            if path.is_empty() {
                // `[x](?view=all#frag)` still targets this page's `#frag`.
                in_page(f, &t, &unquote(anchor), own, &mut defects);
                continue;
            }
            // A rendered link is a URL: `a%20b.md` addresses `a b.md`.
            let path = unquote(path);
            // A `changelog.d/` fragment is spliced into the root CHANGELOG.md,
            // so its links resolve from the root.
            let base = if f.starts_with("changelog.d/") && !f.ends_with("/README.md") {
                root.to_path_buf()
            } else {
                root.join(f)
                    .parent()
                    .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
            };
            let resolved = normalize(&base.join(&path));
            // Off-tree existence proves nothing: `../../../../etc/passwd`
            // exists on the runner and reaches nothing when rendered.
            let Ok(rel) = resolved.strip_prefix(root) else {
                defects.push(format!("{f}: link target escapes the repository: `{t}`"));
                continue;
            };
            if !resolved.exists() {
                defects.push(format!("{f}: link target does not exist: `{t}`"));
                continue;
            }
            let rel = slash_path(rel);
            let anchor = unquote(anchor);
            if let Some(theirs) = anchors.get(rel.as_str())
                && !anchor.is_empty()
                && !theirs.contains(&anchor)
            {
                defects.push(format!(
                    "{f}: no such heading anchor in {rel}: `{t}`{}",
                    case_hint(&anchor, theirs)
                ));
            }
        }
    }

    defects.sort();
    Ok(Report {
        corpus: files.len(),
        defects,
    })
}

fn in_page(f: &str, t: &str, fragment: &str, own: &HashSet<String>, defects: &mut Vec<String>) {
    if !fragment.is_empty() && !own.contains(fragment) {
        defects.push(format!(
            "{f}: no such heading anchor in this page: `{t}`{}",
            case_hint(fragment, own)
        ));
    }
}

fn case_hint(anchor: &str, anchors: &HashSet<String>) -> &'static str {
    if anchors.contains(&anchor.to_lowercase()) {
        " (case: anchors are lowercase)"
    } else {
        ""
    }
}

/// Lexical normalisation, as Python's `os.path.normpath`: `.` dropped, `..`
/// removes the previous component and stops at the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// A relative path as git spells it, with `/` separators.
fn slash_path(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

const FIX_HINT: &str = "
FAIL: the markdown corpus has unresolvable links (listed above).

Fix each one where it lives:
  - renamed or moved page  -> point at the current path
  - page that never existed -> drop the cross-reference, or write the page
  - wrong relative depth    -> `../x.md` vs `./x.md`
  - rustdoc path            -> link to https://docs.rs/autumn-web/latest/...
  - anchor                  -> match the heading's GitHub slug (each space
                               becomes its own hyphen; `—` is stripped, so
                               `## a — b` is `#a--b`)";

/// Run the gate and print its report to `out`; on failure the fix hint goes
/// to stderr, as the script's did, so `out` carries only the report.
/// Returns whether it passed.
///
/// # Errors
///
/// Returns an error when the gate cannot run, or on a failed write.
pub fn run(root: &Path, out: &mut impl Write) -> Result<bool> {
    writeln!(out, "Checking markdown links and heading anchors...")?;
    let report = check(root)?;
    writeln!(out, "corpus: {} tracked markdown files", report.corpus)?;
    for d in &report.defects {
        writeln!(out, "  {d}")?;
    }
    writeln!(out, "defects: {}", report.defects.len())?;
    if report.passed() {
        writeln!(out, "Markdown link gate OK.")?;
    } else {
        eprintln!("{FIX_HINT}");
    }
    Ok(report.passed())
}

#[cfg(test)]
mod tests {
    //! One test per case of the self-test the bash gate carried, plus a run
    //! over this repository.

    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A throwaway git repository holding the two-page corpus every case
    /// starts from. Removed on drop.
    struct Corpus {
        dir: PathBuf,
    }

    impl Corpus {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "xtask-links-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("docs/guide")).unwrap();
            let corpus = Self { dir };
            corpus.git(&["init", "-q"]);
            // `## a — b` slugs to `a--b`: the dash goes, both spaces stay.
            corpus.write(
                "docs/guide/jobs.md",
                "# Jobs\n\n## `after_commit` — post-commit callbacks\n\ntext\n",
            );
            corpus.write(
                "docs/guide/mail.md",
                "# Mail\n\nSee [jobs](jobs.md#after_commit--post-commit-callbacks).\n",
            );
            corpus
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(&self.dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.dir.join(rel)
        }

        fn write(&self, rel: &str, text: impl AsRef<[u8]>) -> &Self {
            let path = self.path(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
            self
        }

        fn append(&self, rel: &str, text: &str) -> &Self {
            let mut body = std::fs::read(self.path(rel)).unwrap();
            body.extend_from_slice(text.as_bytes());
            self.write(rel, body)
        }

        fn mail(&self, text: &str) -> &Self {
            self.append("docs/guide/mail.md", text)
        }

        fn jobs(&self, text: &str) -> &Self {
            self.append("docs/guide/jobs.md", text)
        }

        fn report(&self) -> Report {
            self.git(&["add", "-A"]);
            check(&self.dir).unwrap()
        }

        fn passes(&self) {
            let r = self.report();
            assert!(r.passed(), "expected a pass, got {:#?}", r.defects);
        }

        fn fails(&self) -> Report {
            let r = self.report();
            assert!(!r.passed(), "expected a failure, got a pass");
            r
        }
    }

    impl Drop for Corpus {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn clean_corpus_passes_em_dash_anchor_resolves() {
        let c = Corpus::new();
        let r = c.report();
        assert_eq!(
            r,
            Report {
                corpus: 2,
                defects: vec![]
            }
        );
    }

    #[test]
    fn missing_link_target_fails() {
        let r = Corpus::new()
            .mail("See [gone](does-not-exist.md).\n")
            .fails();
        assert_eq!(
            r.defects,
            ["docs/guide/mail.md: link target does not exist: `does-not-exist.md`"]
        );
    }

    #[test]
    fn missing_cross_page_anchor_fails() {
        let r = Corpus::new()
            .mail("See [nope](jobs.md#no-such-heading).\n")
            .fails();
        assert_eq!(
            r.defects,
            [
                "docs/guide/mail.md: no such heading anchor in docs/guide/jobs.md: `jobs.md#no-such-heading`"
            ]
        );
    }

    #[test]
    fn missing_in_page_anchor_fails() {
        let r = Corpus::new()
            .mail("See [nope](#no-such-heading).\n")
            .fails();
        assert_eq!(
            r.defects,
            ["docs/guide/mail.md: no such heading anchor in this page: `#no-such-heading`"]
        );
    }

    #[test]
    fn rustdoc_path_as_link_target_fails() {
        let r = Corpus::new()
            .mail("See [`X`](autumn_web::aggregate::GroupedAggregate).\n")
            .fails();
        assert!(r.defects[0].contains("rustdoc path used as a markdown link target"));
    }

    #[test]
    fn links_inside_code_are_ignored() {
        Corpus::new()
            .mail("\n```md\n[sample](totally-made-up.md)\n```\n\nand `[inline](nope.md)`.\n")
            .passes();
    }

    #[test]
    fn external_links_are_ignored() {
        Corpus::new()
            .mail("See [ext](https://example.invalid/nope).\n")
            .passes();
    }

    #[test]
    fn duplicate_heading_suffix_resolves() {
        let c = Corpus::new();
        c.jobs("\n## Setup\n\ntext\n\n## Setup\n\ntext\n")
            .mail("See [second](jobs.md#setup-1).\n");
        c.passes();
    }

    #[test]
    fn angle_bracket_destination_resolves() {
        Corpus::new()
            .mail("See [ok](<jobs.md>) and [ok too][ref].\n\n[ref]: <jobs.md>\n")
            .passes();
    }

    #[test]
    fn broken_angle_bracket_destination_with_a_space_fails() {
        Corpus::new()
            .mail("See [gone](<missing file.md>).\n")
            .fails();
    }

    #[test]
    fn links_inside_an_indented_fence_are_ignored() {
        Corpus::new()
            .mail("\n- a nested item:\n\n    ```md\n    [example](totally-made-up.md)\n\n    ## Phantom Heading\n    ```\n")
            .passes();
    }

    #[test]
    fn heading_inside_an_indented_fence_is_not_an_anchor() {
        let c = Corpus::new();
        c.jobs("\n- a nested item:\n\n    ```md\n    ## Phantom Heading\n    ```\n")
            .mail("See [phantom](jobs.md#phantom-heading).\n");
        c.fails();
    }

    #[test]
    fn broken_link_in_a_path_containing_a_space_is_caught() {
        let c = Corpus::new();
        c.write(
            "docs/guide/setup guide.md",
            "# Setup\n\nSee [gone](does-not-exist.md).\n",
        );
        let r = c.fails();
        assert_eq!(r.corpus, 3);
        assert!(r.defects[0].starts_with("docs/guide/setup guide.md:"));
    }

    #[test]
    fn top_level_indented_backticks_do_not_hide_a_live_link() {
        Corpus::new()
            .mail("\n    ```\n\nA live paragraph with [bad](missing.md) in it.\n\n    ```\n")
            .fails();
    }

    #[test]
    fn balanced_parentheses_in_a_destination_resolve() {
        let c = Corpus::new();
        c.write("docs/guide/guide(v2).md", "x\n")
            .mail("See [guide](guide(v2).md).\n");
        c.passes();
    }

    #[test]
    fn sample_inside_a_blockquoted_fence_is_ignored() {
        Corpus::new()
            .mail("\n> A quoted sample:\n>\n> ```md\n> [sample](totally-made-up.md)\n> ```\n")
            .passes();
    }

    #[test]
    fn broken_link_in_block_quoted_prose_is_still_caught() {
        Corpus::new()
            .mail("\n> A quoted broken link: [gone](no-such-file.md).\n")
            .fails();
    }

    #[test]
    fn setext_heading_anchors_resolve() {
        let c = Corpus::new();
        c.jobs("\nSetext title\n============\n\nSub heading\n-----------\n")
            .mail("See [one](jobs.md#setext-title) and [two](jobs.md#sub-heading).\n");
        c.passes();
    }

    #[test]
    fn front_matter_closer_is_not_a_setext_heading() {
        let c = Corpus::new();
        c.write(
            "docs/guide/jobs.md",
            "---\ntitle: Jobs\ndescription: not a heading\n---\n\n# Jobs\n",
        )
        .mail("See [phantom](jobs.md#description-not-a-heading).\n");
        c.fails();
    }

    #[test]
    fn quote_marker_in_indented_code_does_not_hide_a_live_link() {
        Corpus::new()
            .mail("\n    > ```\n\nLive paragraph with [bad](missing.md).\n\n    > ```\n")
            .fails();
    }

    #[test]
    fn broken_link_with_a_single_quoted_title_is_caught() {
        let r = Corpus::new()
            .mail("\nSee [x](missing.md 'a title') and [y](gone.md (a title)).\n")
            .fails();
        assert_eq!(r.defects.len(), 2, "{:#?}", r.defects);
    }

    #[test]
    fn multiline_setext_heading_resolves_as_one_anchor() {
        let c = Corpus::new();
        c.jobs("\nFirst line\nsecond line\n---\n")
            .mail("See [ok](jobs.md#first-line-second-line).\n");
        c.passes();
    }

    #[test]
    fn last_line_alone_is_not_a_setext_anchor() {
        let c = Corpus::new();
        c.jobs("\nFirst line\nsecond line\n---\n")
            .mail("See [phantom](jobs.md#second-line).\n");
        c.fails();
    }

    #[test]
    fn outer_target_of_a_linked_image_is_checked() {
        let c = Corpus::new();
        c.write("docs/guide/diagram.png", "x\n")
            .mail("\n[![diagram](diagram.png)](no-such-page.md)\n");
        let r = c.fails();
        assert_eq!(
            r.defects,
            ["docs/guide/mail.md: link target does not exist: `no-such-page.md`"]
        );
    }

    #[test]
    fn inner_target_of_a_linked_image_is_still_checked() {
        let r = Corpus::new()
            .mail("\n[![diagram](no-such-image.png)](jobs.md)\n")
            .fails();
        assert_eq!(
            r.defects,
            ["docs/guide/mail.md: link target does not exist: `no-such-image.png`"]
        );
    }

    #[test]
    fn fence_indented_for_a_parent_list_item_is_stripped() {
        Corpus::new()
            .mail("\n- outer\n\n  - inner item\n\n  back to outer content\n\n    ```md\n    [x](totally-made-up.md)\n    ```\n")
            .passes();
    }

    #[test]
    fn multi_backtick_code_span_is_stripped() {
        Corpus::new()
            .mail("\nSee ``a span with [x](totally-made-up.md) inside`` here.\n")
            .passes();
    }

    #[test]
    fn text_after_a_closing_fence_does_not_hide_later_links() {
        Corpus::new()
            .mail("\n```\n```not-a-closer\n```\n\nLive [bad](missing.md) after the block.\n")
            .fails();
    }

    #[test]
    fn unmatched_backtick_run_does_not_form_a_code_span() {
        Corpus::new()
            .mail("\nSee `[bad](missing.md)`` here.\n")
            .fails();
    }

    #[test]
    fn escaped_bracket_in_a_label_keeps_the_link_checked() {
        Corpus::new()
            .mail("\nSee [closing \\]](missing.md).\n")
            .fails();
    }

    #[test]
    fn link_inside_an_html_comment_is_ignored() {
        Corpus::new()
            .mail("\n<!-- [draft](totally-made-up.md) -->\n")
            .passes();
    }

    #[test]
    fn backtick_in_a_fence_info_string_does_not_open_a_fence() {
        Corpus::new()
            .mail("\n```md`invalid\n\nLive [bad](missing.md) after.\n")
            .fails();
    }

    #[test]
    fn unclosed_quoted_fence_does_not_swallow_later_prose() {
        Corpus::new()
            .mail("\n> ```md\n> sample\n\nLive [bad](missing.md) after the quote.\n")
            .fails();
    }

    #[test]
    fn escaped_punctuation_in_a_destination_resolves() {
        let c = Corpus::new();
        c.write("docs/guide/guide(v2).md", "x\n")
            .mail("\nSee [guide](guide\\(v2\\).md).\n");
        c.passes();
    }

    #[test]
    fn duplicate_slug_suffix_does_not_collide_with_a_real_heading() {
        let c = Corpus::new();
        c.jobs("\n## Foo\n\n## Foo\n\n## Foo-1\n")
            .mail("See [third](jobs.md#foo-1-1).\n");
        c.passes();
    }

    #[test]
    fn escaped_bracket_in_a_reference_definition_is_parsed() {
        Corpus::new()
            .mail("\nSee [x][closing \\]].\n\n[closing \\]]: missing.md\n")
            .fails();
    }

    #[test]
    fn link_climbing_out_of_the_repository_is_rejected() {
        let r = Corpus::new()
            .mail("\nSee [host](../../../../../../etc/passwd).\n")
            .fails();
        assert!(
            r.defects[0].contains("escapes the repository"),
            "{:#?}",
            r.defects
        );
    }

    #[test]
    fn undecodable_file_is_reported_not_skipped() {
        let c = Corpus::new();
        c.write(
            "docs/guide/latin.md",
            b"# Latin\n\nCaf\xe9 and [x](missing.md).\n",
        );
        let r = c.fails();
        assert_eq!(
            r.defects,
            [
                "docs/guide/latin.md: cannot decode (not UTF-8, and no UTF-16/UTF-8 BOM) — nothing in this file was checked"
            ]
        );
    }

    #[test]
    fn heading_inside_an_html_comment_is_not_an_anchor() {
        let c = Corpus::new();
        c.jobs("\n<!--\n## Phantom\n-->\n")
            .mail("See [p](jobs.md#phantom).\n");
        c.fails();
    }

    #[test]
    fn percent_encoded_destination_resolves() {
        let c = Corpus::new();
        c.write("docs/guide/a b.md", "x\n")
            .mail("\nSee [enc](a%20b.md).\n");
        c.passes();
    }

    #[test]
    fn percent_encoded_heading_fragment_resolves() {
        let c = Corpus::new();
        c.jobs("\n## Café\n")
            .mail("See [a](jobs.md#caf%C3%A9) and [b](#caf%C3%A9).\n")
            .mail("\n## Café\n");
        c.passes();
    }

    #[test]
    fn nested_quoted_fence_ends_when_its_quote_depth_drops() {
        Corpus::new()
            .mail("\n> > ~~~md\n> > sample\n>\n> Live [bad](missing.md) in the outer quote.\n")
            .fails();
    }

    #[test]
    fn mixed_case_fragment_for_a_generated_heading_fails() {
        let c = Corpus::new();
        c.jobs("\n## Section\n")
            .mail("See [wrong](jobs.md#Section).\n");
        let r = c.fails();
        assert!(
            r.defects[0].ends_with("(case: anchors are lowercase)"),
            "{:#?}",
            r.defects
        );
    }

    #[test]
    fn explicit_html_id_matches_its_own_case() {
        let c = Corpus::new();
        c.jobs("\n<a id=\"MixedId\"></a>\n")
            .mail("See [ok](jobs.md#MixedId).\n");
        c.passes();
    }

    #[test]
    fn broken_link_with_an_escaped_title_delimiter_is_caught() {
        Corpus::new()
            .mail("\nSee [broken](missing.md \"title with \\\" quote\").\n")
            .fails();
    }

    #[test]
    fn heading_inside_a_block_quote_is_indexed() {
        let c = Corpus::new();
        c.jobs("\n> ## Quoted Heading\n")
            .mail("See [ok](jobs.md#quoted-heading).\n");
        c.passes();
    }

    #[test]
    fn escaped_opening_bracket_is_not_a_link() {
        Corpus::new()
            .mail("\nLiteral: \\[sample](totally-made-up.md) renders as text.\n")
            .passes();
    }

    #[test]
    fn even_backslash_run_leaves_the_link_live() {
        Corpus::new()
            .mail("\nEven run: \\\\\\\\[x](missing.md) is a live link.\n")
            .fails();
    }

    #[test]
    fn setext_heading_inside_a_block_quote_is_indexed() {
        let c = Corpus::new();
        c.jobs("\n> Quoted heading\n> ---\n")
            .mail("See [ok](jobs.md#quoted-heading).\n");
        c.passes();
    }

    #[test]
    fn single_quoted_and_unquoted_anchor_ids_resolve() {
        let c = Corpus::new();
        c.jobs("\n<a id='MixedId'></a>\n<a name=Bare></a>\n")
            .mail("See [q](jobs.md#MixedId) and [b](jobs.md#Bare).\n");
        c.passes();
    }

    #[test]
    fn anchor_shown_as_inline_code_is_not_indexed() {
        let c = Corpus::new();
        c.jobs("\nDisplay: `<a id=\"phantom\"></a>`\n")
            .mail("See [p](jobs.md#phantom).\n");
        c.fails();
    }

    #[test]
    fn fragment_on_a_query_only_link_is_checked() {
        let r = Corpus::new()
            .mail("\nSee [bad](?view=all#no-such-heading).\n")
            .fails();
        assert!(r.defects[0].contains("in this page"), "{:#?}", r.defects);
    }

    #[test]
    fn setext_collection_stops_at_a_quote_boundary() {
        let c = Corpus::new();
        c.jobs("\nOutside paragraph\n> Quoted heading\n> ---\n")
            .mail("See [ok](jobs.md#quoted-heading).\n");
        c.passes();
    }

    #[test]
    fn unclosed_fence_in_a_list_item_does_not_swallow_later_prose() {
        Corpus::new()
            .mail("\n- item\n\n  ```md\n  sample\n\nBack at top level: [bad](missing.md).\n")
            .fails();
    }

    #[test]
    fn a_docs_tree_named_content_is_still_checked() {
        let c = Corpus::new();
        c.write(
            "docs/content/page.md",
            "# Real docs\n\nSee [gone](does-not-exist.md).\n",
        );
        c.fails();
    }

    #[test]
    fn example_app_seed_content_is_still_excluded() {
        let c = Corpus::new();
        c.write(
            "examples/wiki/content/a.md",
            "# Seed\n\nSee [route](/docs/configuration).\n",
        );
        let r = c.report();
        assert!(r.passed(), "{:#?}", r.defects);
        assert_eq!(r.corpus, 2);
    }

    // Cases beyond the bash self-test.

    #[test]
    fn changelog_fragments_resolve_links_from_the_root() {
        let c = Corpus::new();
        c.write(
            "changelog.d/x.md",
            "### Added\n\n- see [guide](docs/guide/jobs.md)\n",
        )
        .write("changelog.d/README.md", "See [x](x.md).\n");
        c.passes();
    }

    #[test]
    fn utf16_pages_are_read() {
        let c = Corpus::new();
        let mut bytes = vec![0xff, 0xfe];
        for unit in "See [gone](missing.md).\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        c.write("docs/guide/wide.md", bytes);
        let r = c.fails();
        assert_eq!(
            r.defects,
            ["docs/guide/wide.md: link target does not exist: `missing.md`"]
        );
    }

    #[test]
    fn run_prints_the_report() {
        let c = Corpus::new();
        c.mail("See [gone](does-not-exist.md).\n");
        c.git(&["add", "-A"]);
        let mut out = Vec::new();
        assert!(!run(&c.dir, &mut out).unwrap());
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("corpus: 2 tracked markdown files"), "{out}");
        assert!(out.contains("defects: 1"), "{out}");
        assert!(!out.contains("Markdown link gate OK."), "{out}");
    }

    /// A root spelled with `.` or `..` must resolve like the plain path, or
    /// every link in a clean checkout reads as escaping it.
    #[test]
    fn a_root_with_dot_components_checks_like_the_plain_path() {
        let c = Corpus::new();
        c.git(&["add", "-A"]);
        for root in [
            c.dir.join("."),
            c.dir.join("docs/.."),
            c.dir.join("docs/guide/../.."),
        ] {
            let r = check(&root).unwrap();
            assert_eq!(
                r,
                Report {
                    corpus: 2,
                    defects: vec![]
                },
                "{}",
                root.display()
            );
        }
        // `cargo test` runs from the package directory, one below the root.
        let r = check(Path::new("..")).unwrap();
        assert!(r.passed(), "{:#?}", r.defects);
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("/a/../../..")), PathBuf::from("/"));
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("../b"));
    }

    /// The gate this replaces passed on this tree; so must the port.
    #[test]
    fn this_repository_passes() {
        let r = check(crate::repo_root()).unwrap();
        assert!(r.corpus > 100, "corpus is {}", r.corpus);
        assert!(r.passed(), "{:#?}", r.defects);
    }
}
