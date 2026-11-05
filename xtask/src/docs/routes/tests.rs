//! The script's self-test, one test per group of cases, plus runs over this
//! repository.

use super::*;

const SURFACE: &[&str] = &[
    "/actuator/health",
    "/actuator/loggers/{name}",
    "/actuator/tasks",
    "/actuator/webhooks/dlq",
    "/actuator/webhooks/replay",
];

fn surface() -> Vec<String> {
    SURFACE.iter().map(|s| (*s).to_owned()).collect()
}

fn named(path: &str) -> bool {
    resolves(path, &surface(), false)
}

fn requested(path: &str) -> bool {
    resolves(path, &surface(), true)
}

fn paths(text: &str) -> Vec<String> {
    documented(text).into_iter().map(|d| d.path).collect()
}

fn lines_and_paths(text: &str) -> Vec<(usize, String)> {
    documented(text)
        .into_iter()
        .map(|d| (d.line, d.path))
        .collect()
}

#[test]
fn resolution_follows_the_three_permissive_rules() {
    assert!(named("/actuator/health"), "exact mount resolves");
    assert!(!named("/actuator/routes"), "missing endpoint is a defect");
    assert!(
        !named("/actuator/scheduledtasks"),
        "a Spring name is a defect"
    );
    assert!(
        named("/actuator/loggers/root"),
        "mounted {{param}} matches a concrete value"
    );
    assert!(
        named("/actuator/loggers/{name}"),
        "mounted {{param}} matches the param spelling"
    );
    assert!(
        named("/actuator/webhooks"),
        "prefix of a mounted family resolves"
    );
    assert!(named("/actuator"), "bare prefix resolves");
    assert!(named("/actuator/webhooks/*"), "documented glob resolves");
    assert!(
        !named("/actuator/health/deep"),
        "a longer path than any mount is a defect"
    );
    assert!(
        !named("/actuator/task"),
        "a near-miss on a real name is still a defect"
    );
}

#[test]
fn trailing_sentence_punctuation_is_stripped() {
    let mut found =
        paths("prose `/actuator/health`.\nand `/actuator/tasks`, plus (/actuator/info)\n");
    found.sort();
    assert_eq!(
        found,
        ["/actuator/health", "/actuator/info", "/actuator/tasks"]
    );
}

#[test]
fn a_waiver_covers_the_block_above_it_and_no_further() {
    let fence = "| `/actuator/gone` |\n<!-- route-surface-allow: /actuator/gone — reason -->\n";
    assert!(waived_lines(fence)["/actuator/gone"].contains(&1));

    let far = "`/actuator/gone`\n\nfiller\n\nfiller\n\n<!-- route-surface-allow: /actuator/gone — reason -->\n";
    assert!(!waived_lines(far)["/actuator/gone"].contains(&1));

    let wrapped = "| `/actuator/gone` |\n\n<!-- route-surface-allow: /actuator/gone — a reason long\n     enough to wrap onto a second line -->\n";
    assert!(waived_lines(wrapped)["/actuator/gone"].contains(&1));
    assert_eq!(
        lines_and_paths(wrapped),
        [(1, "/actuator/gone".to_owned())],
        "a waiver's own text is not read as a documented path"
    );

    assert!(
        waived_lines("<!-- route-surface-allow: /actuator/gone -->\n").is_empty(),
        "a waiver without a reason does not parse"
    );
}

#[test]
fn the_real_surface_is_non_empty() {
    assert!(mounted_paths(crate::repo_root()).unwrap().len() > 10);
}

#[test]
fn only_a_route_mount_confers_existence() {
    let mount = "  .route(\n    &actuator_route_path(prefix, \"/served\"),\n    get(h),\n)";
    assert_eq!(
        mount_suffixes(mount),
        ["/served"],
        "a `.route(…)` mount is read"
    );
    assert!(
        mount_suffixes("paths.push(actuator_route_path(prefix, \"/inventory_only\"));").is_empty(),
        "an inventory entry is not read"
    );
    assert!(
        mount_suffixes("crate::actuator::actuator_route_path(prefix, \"/pointer_only\")")
            .is_empty(),
        "an alert `where_to_look` pointer is not read"
    );
    assert!(
        mount_suffixes(&format!("#[cfg(test)]\nmod tests {{\n{mount}\n}}\n")).is_empty(),
        "a mount inside a `#[cfg(test)] mod` is not read"
    );
}

#[test]
fn the_prefix_boundary_is_not_truncated() {
    assert_eq!(
        lines_and_paths("see /actuatorhealth for status\n"),
        [(1, "/actuatorhealth".to_owned())]
    );
    assert!(!named("/actuatorhealth"));
    assert!(
        lines_and_paths("mounted at /actuator-dashboard by the app\n").is_empty(),
        "a hyphenated sibling route is out of scope, not a typo"
    );
    assert_eq!(
        lines_and_paths("everything under /actuator is gated\n"),
        [(1, "/actuator".to_owned())]
    );
}

#[test]
fn a_trailing_slash_names_the_subtree() {
    assert!(
        named("/actuator/"),
        "a trailing slash on the prefix names the family"
    );
    assert!(
        named("/actuator/webhooks/"),
        "a trailing slash on a mounted family resolves"
    );
    assert!(
        !named("/actuator/health/"),
        "a trailing slash on a leaf is a defect"
    );
    assert_eq!(
        lines_and_paths("Disallow: /actuator/\n"),
        [(1, "/actuator/".to_owned())]
    );
}

#[test]
fn a_trailing_paren_is_punctuation() {
    assert_eq!(
        paths("GET /actuator/health(old)\n"),
        ["/actuator/health(old"]
    );
    assert!(!named("/actuator/health(old"));
}

#[test]
fn trailing_punctuation_is_dropped() {
    for sentence in [
        "probe /actuator/health.",
        "either /actuator/health, or the probe",
        "see /actuator/health; it is cheap",
        "the probe (/actuator/health) is cheap",
        "mounted at /actuator/health: the liveness view",
    ] {
        assert_eq!(
            paths(&format!("{sentence}\n")),
            ["/actuator/health"],
            "{sentence}"
        );
    }
}

#[test]
fn some_suffixes_end_a_path_without_a_defect() {
    assert_eq!(
        lines_and_paths("GET /actuator/logfile?level=warn\n"),
        [(1, "/actuator/logfile".to_owned())],
        "a query string is not part of the path"
    );
    assert_eq!(
        lines_and_paths("<http://localhost:3000/actuator/health>\n"),
        [(1, "/actuator/health".to_owned())],
        "a markdown autolink terminates the path"
    );
    let target = paths("curl -X PUT .../actuator/loggers/my_app::orders\n");
    assert!(
        named(&target[0]),
        "a `::` logger target resolves as a {{name}} value"
    );
}

#[test]
fn a_prefix_is_a_name_in_prose_and_a_404_in_a_request() {
    assert!(named("/actuator/webhooks"));
    assert!(!requested("/actuator/webhooks"));
    assert!(
        !requested("/actuator"),
        "the bare prefix does not resolve in a request either"
    );
    assert!(
        requested("/actuator/*"),
        "`*` is family notation in a request too"
    );
    assert!(
        requested("/actuator/health"),
        "a whole mounted path resolves in a request"
    );
}

#[test]
fn request_position_detection() {
    for (line, want) in [
        ("curl http://localhost:3000/actuator/webhooks", true),
        ("GET /actuator/logfile?level=warn", true),
        ("GET http://localhost:3000/actuator/webhooks", true),
        ("/actuator/*  GET      -> actuator", false),
        ("| GET | /actuator/health | liveness |", false),
        ("wget -qO- localhost:3000/actuator/health", true),
        ("xh GET localhost:3000/actuator/health", true),
        ("OPTIONS /actuator/webhooks", true),
        ("TRACE /actuator/webhooks", true),
        ("CONNECT /actuator/webhooks", true),
        ("HEAD /actuator/health", true),
        (
            "access_log_exclude = [\"/health\", \"/actuator\", \"/static\"]",
            false,
        ),
        ("Everything under `/actuator/webhooks` is sensitive.", false),
    ] {
        let got = documented(&format!("{line}\n"));
        assert_eq!(got.first().map(|d| d.requested), Some(want), "{line}");
    }
}

#[test]
fn path_characters_are_extracted_whole() {
    for spelling in [
        "/actuator/health~old",
        "/actuator/health+old",
        "/actuator/health%2Fold",
        "/actuator/health@2",
        "/actuator/health:old",
        "/actuator/health;old",
        "/actuator/health,old",
        "/actuator/health.old",
        "/actuator/health!old",
        "/actuatorhealth",
        "/actuator.health",
    ] {
        assert_eq!(
            paths(&format!("GET {spelling}\n")),
            [spelling],
            "extracted whole"
        );
        assert!(!named(spelling), "`{spelling}` does not resolve");
    }
}

#[test]
fn a_wrapped_command_is_one_command() {
    let wrapped =
        "curl -s -H \"Accept: application/json\" \\\n  http://localhost:3000/actuator/webhooks\n";
    let found = documented(wrapped);
    assert_eq!(
        found.iter().map(|d| d.requested).collect::<Vec<_>>(),
        [true]
    );
    assert_eq!(
        found.iter().map(|d| d.line).collect::<Vec<_>>(),
        [1],
        "attributed to the first line"
    );
    assert_eq!(
        lines_and_paths("a\nb /actuator/health\n"),
        [(2, "/actuator/health".to_owned())],
        "an unwrapped line is untouched by folding"
    );
}

#[test]
fn the_corpus_reaches_surfaces_a_docs_view_misses() {
    let here = corpus(crate::repo_root()).unwrap();
    for path in [
        "autumn-cli/src/templates/README.md.tmpl",
        ".claude/skills/run-autumn/SKILL.md",
        "examples/wiki/content/configuration.md",
        "autumn-admin-plugin/README.md",
    ] {
        assert!(here.iter().any(|p| p == path), "`{path}` is in the corpus");
    }
    for path in [
        "autumn/vendor/README.md",
        "benchmarks/runtime/README.md",
        "autumn-cli/src/starters/saas/README.md",
    ] {
        assert!(
            !here.iter().any(|p| p == path),
            "`{path}` is NOT in the corpus"
        );
    }
}

#[test]
fn the_did_you_mean_hint_follows_difflib() {
    let mounted = surface();
    assert_eq!(nearest("/actuator/task", &mounted), Some("/actuator/tasks"));
    assert_eq!(
        nearest("/actuator/scheduledtasks", &mounted),
        Some("/actuator/tasks")
    );
    // Python: SequenceMatcher(None, "abcd", "bcde").ratio() == 0.75.
    assert!((ratio(b"abcd", b"bcde") - 0.75).abs() < f64::EPSILON);
    assert!((ratio(b"", b"") - 1.0).abs() < f64::EPSILON);
}

#[test]
fn paths_resolve_like_python() {
    assert_eq!(normpath("./a/../b"), "b");
    assert_eq!(normpath("../x"), "../x");
    assert_eq!(normpath("a/b/../../.."), "..");
    assert_eq!(normpath(""), ".");
    assert_eq!(join("", "x"), "x");
    assert_eq!(join("a", "/x"), "/x");
    assert_eq!(parent("Cargo.toml"), ".");
    assert_eq!(parent("a/b/Cargo.toml"), "a/b");
}

#[test]
fn publishing_follows_cargo() {
    let table = |s: &str| s.parse::<toml::Table>().unwrap();
    let empty = toml::Table::new();
    assert!(published(&table(""), &empty));
    assert!(!published(&table("publish = false"), &empty));
    assert!(!published(&table("publish = []"), &empty));
    assert!(published(&table("publish = [\"crates-io\"]"), &empty));
    assert!(!published(
        &table("publish = { workspace = true }"),
        &table("publish = false")
    ));
}

#[test]
fn this_repository_passes() {
    let mut out = Vec::new();
    let passed = run(crate::repo_root(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(passed, "{out}");
    assert!(out.ends_with("Actuator path gate OK.\n"), "{out}");
}

#[test]
fn a_page_naming_an_unmounted_path_fails_with_a_hint() {
    let pages = vec!["docs/guide/a.md".to_owned()];
    let report = check_pages(&pages, surface(), &|_| {
        Ok("Use `/actuator/scheduledtasks`.\n\n`/actuator/routes`\n<!-- route-surface-allow: /actuator/routes — troubleshooting row -->\n".to_owned())
    })
    .unwrap();
    assert_eq!(report.checked, 2);
    assert_eq!(report.waived, 1);
    assert_eq!(
        report.defects,
        [Defect {
            page: "docs/guide/a.md".to_owned(),
            line: 1,
            path: "/actuator/scheduledtasks".to_owned(),
        }]
    );
}
