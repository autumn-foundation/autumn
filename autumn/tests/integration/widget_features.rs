//! Spec for the widget family features (#3408).
//!
//! Each opt-in family is one module in `src/widgets/`. Its feature gates the
//! module, its CSS file and its JS part. Every family is default-on, so a
//! default build does not change.

#![cfg(feature = "maud")]

use std::path::Path;

/// One opt-in widget family.
struct Family {
    /// The Cargo feature.
    feature: &'static str,
    /// The module in `src/widgets/`.
    module: &'static str,
    /// The JS part in `vendor/`, if the family has one.
    js: Option<&'static str>,
}

const FAMILIES: &[Family] = &[
    Family {
        feature: "widgets-search",
        module: "search",
        js: Some("autumn-widgets-search.js"),
    },
    Family {
        feature: "widgets-social",
        module: "social",
        js: None,
    },
    Family {
        feature: "widgets-workflow",
        module: "workflow",
        js: None,
    },
    Family {
        feature: "widgets-charts",
        module: "charts",
        js: None,
    },
];

/// Modules that compile whenever `maud` is on.
const CORE_MODULES: &[&str] = &["layout", "nav", "feedback", "data"];

/// True when this test build has `feature` on.
fn enabled(feature: &str) -> bool {
    match feature {
        "widgets-search" => cfg!(feature = "widgets-search"),
        "widgets-social" => cfg!(feature = "widgets-social"),
        "widgets-workflow" => cfg!(feature = "widgets-workflow"),
        "widgets-charts" => cfg!(feature = "widgets-charts"),
        other => panic!("unknown widget feature `{other}`: add it to `enabled`"),
    }
}

fn crate_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Read a file relative to the crate root, with LF line ends.
fn read(path: &str) -> String {
    std::fs::read_to_string(crate_root().join(path))
        .unwrap_or_else(|err| panic!("read {path}: {err}"))
        .replace("\r\n", "\n")
}

fn features() -> toml::Table {
    let manifest: toml::Table = read("Cargo.toml").parse().expect("parse Cargo.toml");
    manifest["features"].as_table().expect("[features]").clone()
}

fn feature_list(features: &toml::Table, name: &str) -> Vec<String> {
    features[name]
        .as_array()
        .unwrap_or_else(|| panic!("feature `{name}` is not a list"))
        .iter()
        .map(|v| v.as_str().expect("feature entry").to_owned())
        .collect()
}

#[test]
fn every_family_is_a_default_feature_that_enables_maud() {
    let features = features();
    let default = feature_list(&features, "default");
    for family in FAMILIES {
        assert!(
            features.contains_key(family.feature),
            "missing feature `{}`",
            family.feature
        );
        assert!(
            default.iter().any(|f| f == family.feature),
            "`{}` must be in `default`",
            family.feature
        );
        assert!(
            feature_list(&features, family.feature)
                .iter()
                .any(|f| f == "maud"),
            "`{}` must enable `maud`",
            family.feature
        );
    }
}

#[test]
fn widgets_is_a_directory_module() {
    assert!(
        !crate_root().join("src/widgets.rs").exists(),
        "src/widgets.rs must be split into src/widgets/"
    );
    assert!(crate_root().join("src/widgets/mod.rs").is_file());
    for module in CORE_MODULES
        .iter()
        .chain(FAMILIES.iter().map(|f| &f.module))
    {
        let path = format!("src/widgets/{module}.rs");
        assert!(crate_root().join(&path).is_file(), "missing {path}");
    }
}

#[test]
fn every_module_file_is_a_known_module() {
    let dir = crate_root().join("src/widgets");
    for entry in std::fs::read_dir(&dir).expect("read src/widgets") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).expect("stem");
        assert!(
            stem == "mod"
                || CORE_MODULES.contains(&stem)
                || FAMILIES.iter().any(|f| f.module == stem),
            "src/widgets/{stem}.rs is not in CORE_MODULES or FAMILIES of this spec"
        );
    }
}

/// The attribute lines just above the first line equal to `line`.
fn attrs_above(source: &str, line: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim() == line)
        .unwrap_or_else(|| panic!("no `{line}` in src/widgets/mod.rs"));
    lines[..at]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with("#["))
        .map(|l| l.trim().to_owned())
        .collect()
}

#[test]
fn every_family_module_is_gated_and_reexported() {
    let mod_rs = read("src/widgets/mod.rs");
    for family in FAMILIES {
        let gate = format!("#[cfg(feature = \"{}\")]", family.feature);
        for line in [
            format!("mod {};", family.module),
            format!("pub use {}::*;", family.module),
        ] {
            assert!(
                attrs_above(&mod_rs, &line).contains(&gate),
                "`{line}` lacks `{gate}`"
            );
        }
    }
    for module in CORE_MODULES {
        for line in [format!("mod {module};"), format!("pub use {module}::*;")] {
            let attrs = attrs_above(&mod_rs, &line);
            assert!(
                !attrs.iter().any(|a| a.contains("widgets-")),
                "core `{line}` must not have a family gate: {attrs:?}"
            );
        }
    }
}

#[test]
fn css_bundle_holds_only_enabled_families() {
    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS;
    let core = read("src/ui/widgets.css");
    assert!(css.starts_with(&core), "the core rules come first");
    for family in FAMILIES {
        let part = read(&format!("src/ui/widgets/{}.css", family.module));
        assert!(!part.trim().is_empty(), "{} CSS is empty", family.module);
        assert_eq!(
            css.contains(&part),
            enabled(family.feature),
            "WIDGETS_COMPONENT_CSS must hold {}.css only when `{}` is on",
            family.module,
            family.feature
        );
    }
    assert!(
        autumn_web::ui::WIDGETS_CSS.ends_with(css),
        "WIDGETS_CSS is the tokens, then the component rules"
    );
}

#[cfg(feature = "htmx")]
#[test]
fn js_bundle_holds_only_enabled_families_before_the_core() {
    let js = std::str::from_utf8(autumn_web::htmx::AUTUMN_WIDGETS_JS).expect("utf8");
    let core = read("vendor/autumn-widgets.js");
    assert!(js.ends_with(&core), "the core runtime comes last");
    for family in FAMILIES {
        let Some(file) = family.js else { continue };
        let part = read(&format!("vendor/{file}"));
        assert_eq!(
            js.contains(&part),
            enabled(family.feature),
            "autumn-widgets.js must hold {file} only when `{}` is on",
            family.feature
        );
    }
}

#[test]
fn core_js_has_no_search_runtime() {
    let core = read("vendor/autumn-widgets.js");
    for needle in ["data-ac-value-id", "acMinLength", "initAutocomplete"] {
        assert!(
            !core.contains(needle),
            "core autumn-widgets.js holds `{needle}`; it belongs in autumn-widgets-search.js"
        );
    }
    let search = read("vendor/autumn-widgets-search.js");
    for needle in ["data-ac-value-id", "acMinLength", "'autumn:init'"] {
        assert!(
            search.contains(needle),
            "autumn-widgets-search.js has no `{needle}`"
        );
    }
}
