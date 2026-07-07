//! `autumn generate system-test` — emit a system-test skeleton.
//!
//! Creates:
//!   - `tests/system/<snake>.rs` — test file wired to the `system-tests` feature
//!   - `Cargo.toml` — adds `[features] system-tests` and `[[test]]` entry if absent
//!
//! # Usage
//!
//!   autumn generate system-test `TodoFlow`
//!   autumn generate system-test `TodoFlow` --dry-run

use std::fmt::Write as _;
use std::path::Path;

use super::emit::Plan;
use super::model::validate_resource_name;
use super::naming::{pascal, snake};
use super::{GenerateError, ensure_project_root};

/// Returns `true` if `trimmed` is a TOML section header matching `name`,
/// with or without a trailing inline comment (e.g. `[workspace] # root`).
fn is_toml_header(trimmed: &str, name: &str) -> bool {
    let pat = format!("[{name}]");
    trimmed == pat
        || trimmed
            .strip_prefix(pat.as_str())
            .is_some_and(|rest| rest.trim_start().starts_with('#'))
}

fn is_virtual_workspace(cargo_toml: &str) -> bool {
    let mut has_workspace = false;
    let mut has_package = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if is_toml_header(trimmed, "workspace") {
            has_workspace = true;
        } else if is_toml_header(trimmed, "package") {
            has_package = true;
        }
    }
    has_workspace && !has_package
}

/// Compute the file actions for `autumn generate system-test`.
///
/// # Errors
/// Project layout and name validation errors surface here.
pub fn plan_system_test(project_root: &Path, name: &str) -> Result<Plan, GenerateError> {
    ensure_project_root(project_root)?;
    validate_resource_name(name)?;

    let snake_name = snake(name);
    let pascal_name = pascal(name);
    let mut plan = Plan::new(project_root);

    // Patch Cargo.toml: add system-tests feature + [[test]] entry.
    let cargo_path = project_root.join("Cargo.toml");
    let existing = std::fs::read_to_string(&cargo_path).map_err(GenerateError::Io)?;

    // Reject virtual workspace manifests (they have [workspace] but no
    // [package]). Patching a virtual manifest with [features] or [[test]]
    // would corrupt it; the user should run this command inside a package.
    if is_virtual_workspace(&existing) {
        return Err(GenerateError::Config(
            "Cargo.toml is a virtual workspace manifest (no [package] section). \
             Run `autumn generate system-test` from inside a package directory."
                .to_owned(),
        ));
    }

    // Resolve the dep key before generating the file so the import uses the
    // right crate name (e.g. `autumn::prelude::*` instead of `autumn_web::prelude::*`
    // when the project renames the dependency).
    let dep_key = resolve_autumn_web_dep_key(&existing);
    let dep_crate = dep_key.replace('-', "_");

    // Ensure tests/system/ directory exists by placing the file there.
    plan.create(
        project_root
            .join("tests")
            .join("system")
            .join(format!("{snake_name}.rs")),
        render_system_test_file(&snake_name, &pascal_name, &dep_crate),
    );

    let patched = patch_cargo_toml(&existing, &snake_name);
    if patched != existing {
        plan.modify(cargo_path.clone(), patched);
    }
    // Pushed unconditionally — see `plan_cargo_deps`'s matching comment in
    // model.rs: destroy recomputes this plan against the already-generated
    // Cargo.toml, where the `[[test]]` entry is by definition already
    // present.
    plan.push_revert(crate::generate::emit::Revert::SystemTestCargoPatch {
        path: cargo_path,
        snake_name,
    });

    Ok(plan)
}

/// Returns `true` if `trimmed` is a TOML array-of-tables header `[[name]]`,
/// with or without a trailing inline comment.
fn is_array_table_header(trimmed: &str, name: &str) -> bool {
    let pat = format!("[[{name}]]");
    trimmed == pat
        || trimmed
            .strip_prefix(pat.as_str())
            .is_some_and(|rest| rest.trim_start().starts_with('#'))
}

/// Find the local dependency key name for the `autumn-web` crate.
///
/// In the common case the key is `autumn-web`.  When a project renames the
/// dependency (`autumn = { package = "autumn-web", ... }`), the feature line
/// must reference the *alias* (`autumn/system-tests`), not
/// `autumn-web/system-tests`, or Cargo rejects the manifest with
/// "feature includes autumn-web/… but autumn-web is not a dependency".
///
/// Returns `"autumn-web"` when no renamed entry is found.
fn resolve_autumn_web_dep_key(cargo_toml: &str) -> String {
    // Strip an inline TOML comment from a value fragment (everything after the
    // first unquoted `#`).  This is a best-effort scan: it handles the common
    // case of `foo = "bar" # package = "autumn-web"` correctly.
    fn strip_inline_comment(s: &str) -> &str {
        let mut in_str = false;
        let mut prev_backslash = false;
        for (i, ch) in s.char_indices() {
            if in_str {
                if ch == '\\' && !prev_backslash {
                    prev_backslash = true;
                    continue;
                }
                if ch == '"' && !prev_backslash {
                    in_str = false;
                }
            } else if ch == '"' {
                in_str = true;
            } else if ch == '#' {
                return s[..i].trim_end();
            }
            prev_backslash = false;
        }
        s
    }

    fn mentions_autumn_web(s: &str) -> bool {
        let s = strip_inline_comment(s);
        s.contains(r#"package = "autumn-web""#) || s.contains(r#"package="autumn-web""#)
    }

    let mut in_dep_section = false;
    // Key from a `[dependencies.KEY]` / `[dev-dependencies.KEY]` subtable header.
    let mut subtable_key: Option<String> = None;
    // Set when we open a multi-line inline table `key = {` with no closing `}`
    // on the same line.  Only valid inside an open brace block; cleared on `}`.
    let mut open_table_key: Option<String> = None;

    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            subtable_key = None;
            open_table_key = None;
            // Plain `[dependencies]` / `[dev-dependencies]` (with optional comment)
            in_dep_section = trimmed == "[dependencies]"
                || trimmed == "[dev-dependencies]"
                || trimmed.starts_with("[dependencies] #")
                || trimmed.starts_with("[dev-dependencies] #");
            // Subtable form: `[dependencies.KEY]` or `[dev-dependencies.KEY]`
            if !in_dep_section {
                for prefix in &["[dependencies.", "[dev-dependencies."] {
                    if let Some(rest) = trimmed.strip_prefix(prefix) {
                        // rest is like `autumn]` or `autumn] # comment`
                        let key = rest
                            .split(']')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .trim_matches('"')
                            .to_owned();
                        if !key.is_empty() {
                            subtable_key = Some(key);
                            in_dep_section = true;
                        }
                        break;
                    }
                }
            }
            continue;
        }
        if !in_dep_section || trimmed.starts_with('#') {
            continue;
        }
        // Inside a `[dependencies.KEY]` subtable — if we see `package = "autumn-web"`
        // the key is the subtable name itself.
        if let Some(ref key) = subtable_key {
            if mentions_autumn_web(trimmed) {
                return key.clone();
            }
            continue;
        }
        // Inside an open multi-line table `key = {\n ... \n}` — every line
        // belongs to the same entry until we see the closing `}`.
        if let Some(ref key) = open_table_key {
            if trimmed.contains('}') {
                open_table_key = None;
            } else if mentions_autumn_web(trimmed) {
                return key.clone();
            }
            continue;
        }
        // New key=value line.
        if let Some((key, rest)) = trimmed.split_once('=') {
            let key = key.trim().trim_matches('"').to_owned();
            let rest = rest.trim();
            if mentions_autumn_web(rest) {
                return key;
            }
            // If the value opens a brace without closing it, enter multi-line mode.
            let open_count = rest.chars().filter(|&c| c == '{').count();
            let close_count = rest.chars().filter(|&c| c == '}').count();
            if open_count > close_count {
                open_table_key = Some(key);
            }
        }
    }
    "autumn-web".to_owned()
}

/// Returns `true` if `trimmed` is a `[features]` table header, with or without
/// a trailing inline comment (e.g. `[features] # project features`).
fn is_features_header(trimmed: &str) -> bool {
    trimmed == "[features]"
        || trimmed
            .strip_prefix("[features]")
            .is_some_and(|rest| rest.trim_start().starts_with('#'))
}

/// Returns `true` if the `[features]` table in `cargo_toml` already contains
/// a key named `key` (i.e. the key appears within the `[features]` section,
/// not merely anywhere in the file).
fn features_section_has_key(cargo_toml: &str, key: &str) -> bool {
    let mut in_features = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if is_features_header(trimmed) {
            in_features = true;
            continue;
        }
        if in_features {
            // Another table header ends the [features] section.
            if trimmed.starts_with('[') {
                break;
            }
            // Check if this line declares the key (bare or TOML-quoted form).
            let quoted_key = format!("\"{key}\"");
            let bare_match = trimmed
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='));
            let quoted_match = trimmed
                .strip_prefix(quoted_key.as_str())
                .is_some_and(|rest| rest.trim_start().starts_with('='));
            if bare_match || quoted_match {
                return true;
            }
        }
    }
    false
}

/// Returns `true` if any `[[test]]` section in `cargo_toml` has `name = test_name`.
///
/// Scans section-by-section so key order and whitespace within the section
/// don't cause false negatives.
fn test_section_names_test(cargo_toml: &str, test_name: &str) -> bool {
    let expected = format!("\"{test_name}\"");
    let mut in_test = false;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if is_array_table_header(trimmed, "test") {
            in_test = true;
            continue;
        }
        if in_test {
            if trimmed.starts_with('[') {
                // This header ends the current [[test]] section. If it's
                // another [[test]], re-enter immediately so we don't skip it.
                in_test = is_array_table_header(trimmed, "test");
                continue;
            }
            // Accept both bare `name` and quoted `"name"` key forms.
            let after_name = trimmed
                .strip_prefix("\"name\"")
                .or_else(|| trimmed.strip_prefix("name"));
            if let Some(after) = after_name {
                let after = after.trim_start();
                if let Some(val) = after.strip_prefix('=') {
                    // Strip any trailing TOML inline comment before comparing.
                    let val = val.trim();
                    let val = val.split_once(" #").map_or(val, |(v, _)| v.trim());
                    if val == test_name || val == expected {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Returns the byte offset immediately after the `[features]` header line
/// (i.e. after the newline that terminates the header), handling both LF and
/// CRLF line endings and any inline comments on the header line.
fn find_features_header_end(cargo_toml: &str) -> Option<usize> {
    let mut pos = 0;
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        let is_header = is_features_header(trimmed);
        // Advance pos past this line (include its line ending).
        pos += line.len();
        // Account for \r\n vs \n.
        if cargo_toml[pos..].starts_with("\r\n") {
            pos += 2;
        } else if cargo_toml[pos..].starts_with('\n') {
            pos += 1;
        }
        if is_header {
            return Some(pos);
        }
    }
    None
}

/// Patch `Cargo.toml` content to add the `system-tests` feature (under
/// `[features]` only, not in `[dependencies]`) and a `[[test]]` entry for this
/// test file if they are not already present.
pub(super) fn patch_cargo_toml(existing: &str, snake_name: &str) -> String {
    let mut out = existing.to_owned();

    // 1. Add [features] system-tests entry if not already in the [features] table.
    // We scan only the [features] section so that a dev-dependency enabling
    // autumn-web/system-tests does not suppress the local feature definition
    // (which is required by `--features system-tests` and `#[cfg(feature = ...)]`).
    let dep_key = resolve_autumn_web_dep_key(&out);
    let feature_line = format!("system-tests = [\"{dep_key}/system-tests\"]");
    if !features_section_has_key(&out, "system-tests") {
        // Find the byte offset of the end of the "[features]" header line so we
        // can insert immediately after it regardless of line ending style (LF or
        // CRLF) or trailing inline comments on the header.
        if let Some(insert_pos) = find_features_header_end(&out) {
            // If the header had no trailing newline (EOF case), add one before
            // the feature line so the manifest stays valid TOML.
            let prefix = if insert_pos > 0 && !out[..insert_pos].ends_with('\n') {
                "\n"
            } else {
                ""
            };
            out.insert_str(insert_pos, &format!("{prefix}{feature_line}\n"));
        } else {
            let _ = write!(out, "\n[features]\n{feature_line}\n");
        }
    }

    // 2. Add [[test]] entry if no [[test]] section already names this test.
    // Scan section-by-section so key order and whitespace don't matter.
    if !test_section_names_test(&out, snake_name) {
        let _ = write!(
            out,
            "\n[[test]]\nname = \"{snake_name}\"\npath = \"tests/system/{snake_name}.rs\"\n"
        );
    }

    out
}

/// Inverse of [`patch_cargo_toml`] (`autumn destroy`, issue #1048).
///
/// Removes this test's own `[[test]]` entry — the exact block
/// `patch_cargo_toml` appends for `snake_name` — then removes the shared
/// `system-tests` feature declaration too, but only if no OTHER
/// `tests/system/*.rs` entry remains, mirroring the "shared while still
/// needed" rule `emit::SHARED_MAIN_MODULE_NAMES` applies to `src/main.rs`
/// module declarations (multiple system-test resources, and `generate pwa`'s
/// own smoke test, all share this one feature declaration) — AND only if no
/// hand-written `#[cfg(feature = "system-tests")]` elsewhere in the project
/// still gates on it (issue #1048 PR review): the feature may have
/// pre-existed for code this generator never touched.
///
/// A no-op if this test's `[[test]]` entry isn't present.
pub(super) fn remove_cargo_toml_patch(
    existing: &str,
    snake_name: &str,
    project_root: &std::path::Path,
    excluding: &[std::path::PathBuf],
) -> String {
    let test_block =
        format!("\n[[test]]\nname = \"{snake_name}\"\npath = \"tests/system/{snake_name}.rs\"\n");
    let Some(pos) = existing.find(&test_block) else {
        return existing.to_owned();
    };
    let mut without_test_entry = String::with_capacity(existing.len() - test_block.len());
    without_test_entry.push_str(&existing[..pos]);
    without_test_entry.push_str(&existing[pos + test_block.len()..]);

    if without_test_entry.contains("tests/system/") {
        return without_test_entry;
    }
    if super::emit::cargo_feature_still_gated_elsewhere("system-tests", project_root, excluding) {
        return without_test_entry;
    }

    let dep_key = resolve_autumn_web_dep_key(&without_test_entry);
    let feature_line = format!("system-tests = [\"{dep_key}/system-tests\"]\n");
    let Some(fpos) = without_test_entry.find(&feature_line) else {
        return without_test_entry;
    };
    let mut without_feature = String::with_capacity(without_test_entry.len() - feature_line.len());
    without_feature.push_str(&without_test_entry[..fpos]);
    without_feature.push_str(&without_test_entry[fpos + feature_line.len()..]);
    strip_empty_features_header(&without_feature)
}

/// If removing the `system-tests` feature line left the `[features]` table
/// with no other keys, remove the header (plus one preceding blank
/// separator line) too — mirroring `patch_cargo_toml`'s "create a brand new
/// `[features]` section from scratch" branch, so a project that never had
/// one before `generate system-test`/`generate pwa` ends up with none after
/// `destroy`, byte-identical to its pre-generate state. A pre-existing
/// `[features]` table with other keys is left untouched.
fn strip_empty_features_header(existing: &str) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let Some(header_idx) = lines.iter().position(|l| is_features_header(l.trim())) else {
        return existing.to_owned();
    };
    let section_end = lines[header_idx + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map_or(lines.len(), |off| header_idx + 1 + off);
    if lines[header_idx + 1..section_end]
        .iter()
        .any(|l| !l.trim().is_empty())
    {
        return existing.to_owned();
    }
    let mut start = header_idx;
    if start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len());
    new_lines.extend_from_slice(&lines[..start]);
    new_lines.extend_from_slice(&lines[section_end..]);
    let mut out = new_lines.join("\n");
    if existing.ends_with('\n') && !new_lines.is_empty() {
        out.push('\n');
    }
    out
}

fn render_system_test_file(snake_name: &str, pascal_name: &str, dep_crate: &str) -> String {
    format!(
        r#"//! System test: {pascal_name}
//!
//! Generated by `autumn generate system-test {pascal_name}`.
//!
//! Run:
//!   cargo test --features system-tests --test {snake_name} -- --include-ignored
//!
//! Requires Chromium:
//!   apt-get install chromium-browser          # Ubuntu/Debian
//!   brew install --cask chromium              # macOS
//!   AUTUMN_CHROMIUM=/path/to/chrome cargo test # custom binary

#![cfg(feature = "system-tests")]

use {dep_crate}::prelude::*;
use {dep_crate}::system_test::SystemTest;

// ── Route handlers under test ──────────────────────────────────────────────

#[get("/")]
async fn index() -> String {{
    format!(
        "<!DOCTYPE html><html><head><title>{pascal_name}</title></head>\
         <body><h1>{pascal_name}</h1></body></html>"
    )
}}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Happy-path: visit the index page and assert on rendered content.
///
/// Requires Chromium on the host. Skipped in CI unless `AUTUMN_CHROMIUM` or
/// a system Chromium binary is available.
#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn {snake_name}_index_renders() {{
    let runner = SystemTest::new()
        .routes(routes![index])
        .build()
        .await
        .expect("system test runner");

    let page = runner.page().await.expect("page");
    page.visit("/").await.expect("visit /");
    page.expect_text("{pascal_name}").await.expect("page title visible");
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::Flags;
    use std::fs;
    use tempfile::TempDir;

    fn temp_project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        // Minimal Cargo.toml so ensure_project_root passes.
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"test-project\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn plan_creates_system_test_file() {
        let tmp = temp_project();
        let plan = plan_system_test(tmp.path(), "TodoFlow").unwrap();
        plan.execute(Flags::default()).unwrap();

        let test_file = tmp.path().join("tests").join("system").join("todo_flow.rs");
        assert!(test_file.exists(), "expected {}", test_file.display());

        let content = fs::read_to_string(&test_file).unwrap();
        assert!(content.contains("TodoFlow"), "missing pascal name");
        assert!(content.contains("system-tests"), "missing feature gate");
        assert!(content.contains("#[tokio::test]"), "missing test attr");
        assert!(
            content.contains("#[ignore"),
            "test must be #[ignore] by default (requires Chromium)"
        );
    }

    #[test]
    fn generate_then_destroy_round_trips_to_original_cargo_toml() {
        let tmp = temp_project();
        let cargo_path = tmp.path().join("Cargo.toml");
        let original_cargo = fs::read_to_string(&cargo_path).unwrap();

        let plan = plan_system_test(tmp.path(), "TodoFlow").unwrap();
        plan.execute(Flags::default()).unwrap();
        assert!(
            fs::read_to_string(&cargo_path)
                .unwrap()
                .contains("system-tests")
        );

        let destroy_plan = plan_system_test(tmp.path(), "TodoFlow").unwrap();
        destroy_plan.revert(Flags::default()).unwrap();

        assert!(
            !tmp.path()
                .join("tests")
                .join("system")
                .join("todo_flow.rs")
                .exists()
        );
        assert_eq!(fs::read_to_string(&cargo_path).unwrap(), original_cargo);
    }

    #[test]
    fn destroying_one_of_two_system_tests_keeps_the_others_feature_and_entry() {
        let tmp = temp_project();
        plan_system_test(tmp.path(), "TodoFlow")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_system_test(tmp.path(), "CheckoutFlow")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let cargo_path = tmp.path().join("Cargo.toml");

        plan_system_test(tmp.path(), "TodoFlow")
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(
            !tmp.path()
                .join("tests")
                .join("system")
                .join("todo_flow.rs")
                .exists()
        );
        assert!(
            tmp.path()
                .join("tests")
                .join("system")
                .join("checkout_flow.rs")
                .exists()
        );
        let cargo_after = fs::read_to_string(&cargo_path).unwrap();
        assert!(
            cargo_after.contains("system-tests"),
            "the shared feature must survive — the other system test still needs it"
        );
        assert!(cargo_after.contains("checkout_flow"));
        assert!(!cargo_after.contains("todo_flow"));
    }

    #[test]
    fn destroying_the_only_system_test_keeps_a_pre_existing_hand_written_feature_gate() {
        // The project already gates unrelated hand-written code on
        // `system-tests` (independent of this generator) before `generate
        // system-test` ever runs. Destroying the only generated system test
        // must not strip that pre-existing feature declaration out from
        // under the hand-written code (issue #1048 PR review).
        let tmp = temp_project();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("lib.rs"),
            "#[cfg(feature = \"system-tests\")]\npub fn only_for_browser_tests() {}\n",
        )
        .unwrap();

        plan_system_test(tmp.path(), "TodoFlow")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        plan_system_test(tmp.path(), "TodoFlow")
            .unwrap()
            .revert(Flags::default())
            .unwrap();

        assert!(
            !tmp.path()
                .join("tests")
                .join("system")
                .join("todo_flow.rs")
                .exists()
        );
        let cargo_after = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo_after.contains("system-tests"),
            "pre-existing hand-written feature gate must survive destroy"
        );
    }

    #[test]
    fn plan_snake_cases_name() {
        let tmp = temp_project();
        plan_system_test(tmp.path(), "MyFeatureTest")
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let test_file = tmp
            .path()
            .join("tests")
            .join("system")
            .join("my_feature_test.rs");
        assert!(test_file.exists());
    }

    #[test]
    fn plan_rejects_invalid_name() {
        let tmp = temp_project();
        assert!(plan_system_test(tmp.path(), "123-invalid").is_err());
    }

    #[test]
    fn plan_dry_run_writes_nothing() {
        let tmp = temp_project();
        plan_system_test(tmp.path(), "DryRunTest")
            .unwrap()
            .execute(Flags {
                dry_run: true,
                force: false,
            })
            .unwrap();

        let test_file = tmp
            .path()
            .join("tests")
            .join("system")
            .join("dry_run_test.rs");
        assert!(!test_file.exists(), "dry run should not write files");
    }

    #[test]
    fn plan_collides_without_force() {
        let tmp = temp_project();
        let flags = Flags::default();
        plan_system_test(tmp.path(), "Collision")
            .unwrap()
            .execute(flags)
            .unwrap();
        // Second attempt should fail.
        let result = plan_system_test(tmp.path(), "Collision")
            .unwrap()
            .execute(flags);
        assert!(result.is_err());
    }

    #[test]
    fn plan_patches_cargo_toml() {
        let tmp = temp_project();
        plan_system_test(tmp.path(), "TodoFlow")
            .unwrap()
            .execute(Flags::default())
            .unwrap();

        let cargo = fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert!(
            cargo.contains("system-tests"),
            "Cargo.toml must define system-tests feature"
        );
        assert!(
            cargo.contains("[[test]]"),
            "Cargo.toml must have a [[test]] entry"
        );
        assert!(
            cargo.contains("todo_flow"),
            "[[test]] must reference the generated file"
        );
    }

    #[test]
    fn patch_cargo_toml_crlf_features_header() {
        // Cargo.toml with CRLF line endings should still have the feature inserted.
        let crlf = "[package]\r\nname = \"x\"\r\n\r\n[features]\r\nother = []\r\n";
        let patched = patch_cargo_toml(crlf, "my_test");
        assert!(
            patched.contains("system-tests"),
            "feature must be inserted even with CRLF line endings"
        );
        assert!(
            patched.contains("[[test]]"),
            "[[test]] entry must also be present"
        );
    }

    #[test]
    fn patch_cargo_toml_features_header_with_comment() {
        let src = "[package]\nname = \"x\"\n\n[features] # project features\nother = []\n";
        let patched = patch_cargo_toml(src, "my_test");
        assert!(
            patched.contains("system-tests"),
            "feature must be inserted after a commented header"
        );
    }

    #[test]
    fn patch_cargo_toml_idempotent_with_commented_features_header() {
        // If [features] already has system-tests under a commented header,
        // patching again must not insert a second key.
        let src = "[package]\nname = \"x\"\n\n[features] # project features\nsystem-tests = [\"autumn-web/system-tests\"]\nother = []\n";
        let patched = patch_cargo_toml(src, "my_test");
        let count = patched.matches("system-tests =").count();
        assert_eq!(
            count, 1,
            "system-tests key must appear exactly once; got {count}"
        );
    }

    #[test]
    fn find_features_header_end_no_trailing_newline() {
        // [features] as the very last line with no trailing newline — pos does
        // not advance past a newline but the header is still found.
        let src = "[package]\nname = \"x\"\n\n[features]";
        let pos = super::find_features_header_end(src);
        // Should point to the byte just past "[features]" (end of string).
        assert!(
            pos.is_some(),
            "should find header even without trailing newline"
        );
        let pos = pos.unwrap();
        assert_eq!(pos, src.len(), "pos should be at end of string");
    }

    #[test]
    fn find_features_header_end_no_features_section() {
        let src = "[package]\nname = \"x\"\n";
        assert!(
            super::find_features_header_end(src).is_none(),
            "should return None when no [features] header present"
        );
    }

    #[test]
    fn features_section_has_key_recognizes_commented_header_with_key() {
        let src = "[features] # project features\nsystem-tests = [\"autumn-web/system-tests\"]\n";
        assert!(
            super::features_section_has_key(src, "system-tests"),
            "should detect key under a commented [features] header"
        );
    }

    #[cfg(unix)]
    #[test]
    fn plan_system_test_errors_on_unreadable_cargo_toml() {
        use std::os::unix::fs::PermissionsExt;
        // Skip when running as root (CI containers) since chmod 000 is ineffective.
        if std::fs::read("/etc/shadow").is_ok() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let cargo_path = tmp.path().join("Cargo.toml");
        fs::write(
            &cargo_path,
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::set_permissions(&cargo_path, fs::Permissions::from_mode(0o000)).unwrap();
        let result = plan_system_test(tmp.path(), "IoError");
        fs::set_permissions(&cargo_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            result.is_err(),
            "plan_system_test must propagate Cargo.toml read errors"
        );
    }

    #[test]
    fn patch_cargo_toml_no_trailing_newline_on_features_header() {
        // When [features] is the last line with no newline, the feature line
        // must be on a new line (not run together as "[features]system-tests").
        let src = "[package]\nname = \"x\"\n\n[features]";
        let patched = patch_cargo_toml(src, "eof_test");
        assert!(
            patched.contains("[features]\nsystem-tests"),
            "feature line must follow [features] on a new line; got: {patched:?}"
        );
    }

    #[test]
    fn test_section_names_test_multiple_sections() {
        // When there are multiple [[test]] sections, the scanner must check
        // all of them and not stop after the first one.
        let src = "[[test]]\nname = \"other_test\"\npath = \"tests/other.rs\"\n\
                   \n[[test]]\nname = \"my_test\"\npath = \"tests/my_test.rs\"\n";
        assert!(
            super::test_section_names_test(src, "my_test"),
            "should find my_test in the second [[test]] section"
        );
        assert!(
            !super::test_section_names_test(src, "missing"),
            "should return false for a name not in any section"
        );
    }

    #[test]
    fn plan_rejects_virtual_workspace() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let result = plan_system_test(tmp.path(), "MyTest");
        assert!(
            result.is_err(),
            "should reject a virtual workspace manifest"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("virtual workspace"),
            "error should mention virtual workspace, got: {msg}"
        );
    }

    #[test]
    fn plan_rejects_virtual_workspace_with_comment() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace] # root manifest\nmembers = [\"app\"]\n",
        )
        .unwrap();
        let result = plan_system_test(tmp.path(), "MyTest");
        assert!(
            result.is_err(),
            "should reject a virtual workspace with a commented header"
        );
    }

    #[test]
    fn test_section_names_test_inline_comment() {
        // name values with trailing TOML inline comments must still be recognised.
        let src =
            "[[test]]\nname = \"todo_flow\" # browser test\npath = \"tests/system/todo_flow.rs\"\n";
        assert!(
            super::test_section_names_test(src, "todo_flow"),
            "should match name even when it has a trailing inline comment"
        );
    }

    #[test]
    fn patch_cargo_toml_idempotent_with_quoted_feature_key() {
        // A valid TOML manifest may quote the key: `"system-tests" = [...]`.
        // The generator must recognise this and not insert a duplicate bare key.
        let src = "[package]\nname = \"x\"\n\n[features]\n\"system-tests\" = [\"autumn-web/system-tests\"]\n";
        let patched = patch_cargo_toml(src, "my_test");
        // Confirm no bare (unquoted) system-tests key was inserted.
        assert!(
            !patched.contains("\nsystem-tests ="),
            "must not insert a bare system-tests key when a quoted one already exists; patched: {patched:?}"
        );
        // The feature content should not have grown — the original and patched
        // [features] section should be identical.
        let orig_lines: Vec<_> = src.lines().filter(|l| l.contains("system-tests")).collect();
        let patch_lines: Vec<_> = patched
            .lines()
            .filter(|l| l.contains("system-tests"))
            .collect();
        assert_eq!(
            orig_lines, patch_lines,
            "patching must not change the existing system-tests feature line"
        );
    }

    #[test]
    fn resolve_dep_key_subtable_syntax() {
        // `[dev-dependencies.autumn]` with `package = "autumn-web"` in the body.
        let src = "[package]\nname = \"x\"\n\n[dev-dependencies.autumn]\nversion = \"0.1\"\npackage = \"autumn-web\"\n";
        assert_eq!(
            super::resolve_autumn_web_dep_key(src),
            "autumn",
            "should resolve key from subtable header"
        );
    }

    #[test]
    fn resolve_dep_key_ignores_inline_comment_mention() {
        // A comment mentioning package = "autumn-web" must not fool the resolver.
        let src = "[dependencies]\nfoo = \"1\" # package = \"autumn-web\"\nautumn-web = \"0.1\"\n";
        assert_eq!(
            super::resolve_autumn_web_dep_key(src),
            "autumn-web",
            "comment mention must not be treated as a package alias"
        );
    }

    #[test]
    fn resolve_dep_key_not_fooled_by_prior_dependency() {
        // `serde` precedes `autumn`; stale pending_key must not return "serde".
        let src = "[dependencies]\nserde = \"1\"\nautumn = { package = \"autumn-web\", version = \"0.1\" }\n";
        assert_eq!(
            super::resolve_autumn_web_dep_key(src),
            "autumn",
            "should not attribute the alias to a prior dependency entry"
        );
    }

    #[test]
    fn resolve_dep_key_multiline_table() {
        // `autumn = {` on one line, `package = "autumn-web"` on the next.
        let src = "[dependencies]\nautumn = {\npackage = \"autumn-web\"\nversion = \"0.1\"\n}\n";
        assert_eq!(
            super::resolve_autumn_web_dep_key(src),
            "autumn",
            "should resolve key from multiline table value"
        );
    }

    #[test]
    fn plan_creates_system_test_file_with_renamed_dep() {
        // When autumn-web is aliased, the generated file must import via the alias.
        let tmp = TempDir::new().unwrap();
        let cargo = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nautumn = { package = \"autumn-web\", version = \"0.1\" }\n\n[features]\n";
        fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();
        plan_system_test(tmp.path(), "MyTest")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        let generated = fs::read_to_string(tmp.path().join("tests/system/my_test.rs")).unwrap();
        assert!(
            generated.contains("use autumn::prelude::*"),
            "import must use the alias 'autumn', got:\n{generated}"
        );
        assert!(
            !generated.contains("autumn_web::"),
            "import must not reference 'autumn_web' when dep is aliased, got:\n{generated}"
        );
    }

    #[test]
    fn test_section_names_test_quoted_name_key() {
        // A valid TOML quoted key `"name" = "todo_flow"` must be recognised.
        let src = "[[test]]\n\"name\" = \"todo_flow\"\npath = \"tests/system/todo_flow.rs\"\n";
        assert!(
            super::test_section_names_test(src, "todo_flow"),
            "should match quoted name key"
        );
    }

    #[test]
    fn plan_force_overwrites() {
        let tmp = temp_project();
        plan_system_test(tmp.path(), "Force")
            .unwrap()
            .execute(Flags::default())
            .unwrap();
        // Second attempt with force should succeed.
        plan_system_test(tmp.path(), "Force")
            .unwrap()
            .execute(Flags {
                dry_run: false,
                force: true,
            })
            .unwrap();
    }
}
