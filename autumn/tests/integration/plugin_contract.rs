//! The versioned plugin API stability contract (issue #1601).
//!
//! Three things are under test here:
//!
//! 1. The **surface registry** — the machine-readable half of
//!    `docs/plugins.md`'s stability tiers. Every plugin-facing API is declared
//!    `Stable` or `Experimental`, and the registry is what the docs gate and
//!    the reference plugin are checked against.
//! 2. The **compatibility contract** — a plugin declares the `autumn-web`
//!    range it supports, and an incompatible pairing fails loudly at app
//!    startup with a message naming *both* versions.
//! 3. The **conformance report** — `autumn plugin-check`'s library half tells
//!    an author when their plugin leans on experimental surface.

use std::borrow::Cow;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::{
    AUTUMN_WEB_VERSION, ContractVerdict, PLUGIN_CONTRACT_MARKER, PLUGIN_SURFACES, PluginContract,
    SurfaceTier, evaluate, stable_surface_names, surface,
};

// ── 1. The surface registry ────────────────────────────────────────────────

#[test]
fn every_declared_surface_carries_a_tier_a_name_and_a_note() {
    assert!(
        !PLUGIN_SURFACES.is_empty(),
        "the plugin surface registry is the declared contract; it cannot be empty"
    );
    for s in PLUGIN_SURFACES {
        assert!(!s.name.is_empty(), "surface with an empty name");
        assert!(
            !s.note.is_empty(),
            "surface `{}` has no note; the note is what a plugin author reads",
            s.name
        );
        assert!(
            matches!(s.tier, SurfaceTier::Stable | SurfaceTier::Experimental),
            "surface `{}` has no tier",
            s.name
        );
    }
}

#[test]
fn surface_names_are_unique_and_sorted() {
    let names: Vec<&str> = PLUGIN_SURFACES.iter().map(|s| s.name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        names.len(),
        sorted.len(),
        "duplicate surface name in the registry"
    );
    assert_eq!(
        names, sorted,
        "the registry is kept sorted so the docs table and the diff stay readable"
    );
}

#[test]
fn the_registry_declares_both_tiers() {
    assert!(
        PLUGIN_SURFACES
            .iter()
            .any(|s| s.tier == SurfaceTier::Stable),
        "no stable surface declared"
    );
    assert!(
        PLUGIN_SURFACES
            .iter()
            .any(|s| s.tier == SurfaceTier::Experimental),
        "no experimental surface declared; the tier would be untested vocabulary"
    );
}

#[test]
fn the_core_plugin_entry_points_are_stable() {
    for name in ["Plugin::build", "Plugin::name", "AppBuilder::on_startup"] {
        let s = surface(name).unwrap_or_else(|| panic!("`{name}` is not in the registry"));
        assert_eq!(
            s.tier,
            SurfaceTier::Stable,
            "`{name}` is the plugin system's front door; it must be stable"
        );
    }
}

#[test]
fn unknown_surface_lookup_returns_none() {
    assert!(surface("AppBuilder::definitely_not_a_real_api").is_none());
}

#[test]
fn stable_surface_names_lists_exactly_the_stable_tier() {
    let listed: Vec<&str> = stable_surface_names().collect();
    let expected: Vec<&str> = PLUGIN_SURFACES
        .iter()
        .filter(|s| s.tier == SurfaceTier::Stable)
        .map(|s| s.name)
        .collect();
    assert_eq!(listed, expected);
}

/// Does `decl` in `source` carry `#[non_exhaustive]`?
///
/// Walks back from the declaration over its other attributes, doc comments and
/// blank lines, and stops at the first line that is none of those.
fn carries_non_exhaustive(source: &str, decl: &str) -> Option<bool> {
    let lines: Vec<&str> = source.lines().collect();
    let idx = lines.iter().position(|l| l.trim_start() == decl)?;
    for line in lines[..idx].iter().rev() {
        let t = line.trim_start();
        if t == "#[non_exhaustive]" {
            return Some(true);
        }
        if t.starts_with("#[") || t.starts_with("///") || t.starts_with("//") || t.is_empty() {
            continue;
        }
        break;
    }
    Some(false)
}

/// The scan above only means something if it can say *no*. A test that always
/// passes is worse than no test, so prove both answers against synthetic input
/// before trusting it on the real source.
#[test]
fn the_non_exhaustive_scan_can_fail() {
    let annotated = "/// Docs.\n#[derive(Debug)]\n#[non_exhaustive]\npub enum Thing {\n}\n";
    let bare = "/// Docs.\n#[derive(Debug)]\npub enum Thing {\n}\n";
    let elsewhere =
        "#[non_exhaustive]\npub enum Other {}\n\n#[derive(Debug)]\npub enum Thing {\n}\n";

    assert_eq!(
        carries_non_exhaustive(annotated, "pub enum Thing {"),
        Some(true)
    );
    assert_eq!(
        carries_non_exhaustive(bare, "pub enum Thing {"),
        Some(false)
    );
    assert_eq!(
        carries_non_exhaustive(elsewhere, "pub enum Thing {"),
        Some(false),
        "an attribute on a NEIGHBOURING item must not count"
    );
    assert_eq!(carries_non_exhaustive(bare, "pub enum Missing {"), None);
}

/// `CHANGELOG.md` and `docs/migrations/next.md` both tell plugin authors that
/// the plugin-facing types are `#[non_exhaustive]`. That promise is what lets a
/// later release add a verdict, a tier, or a check's configuration without
/// breaking every plugin — so it has to be kept, and an annotation is exactly
/// the kind of one-line edit that goes missing in a rebase without any test
/// noticing. (One did, on this very branch, and a review caught it.)
///
/// Nothing inside `autumn-web` can notice on its own: `#[non_exhaustive]` has
/// no effect in the defining crate, so an in-crate literal or exhaustive match
/// compiles either way. The ideal guard is a `trybuild` fixture compiling from
/// outside; its expected stderr is rustc-version-specific, though, and this
/// repository builds on more than one toolchain. So this reads the source
/// instead: weaker than a compile, but it catches the failure mode that
/// actually happens — the annotation not being there at all — on every
/// toolchain.
#[test]
fn every_type_the_docs_promise_as_non_exhaustive_carries_the_attribute() {
    const CONTRACT_SRC: &str = include_str!("../../src/plugin_contract.rs");
    const CONFORMANCE_SRC: &str = include_str!("../../src/plugin_conformance.rs");

    let promised: &[(&str, &str, &str)] = &[
        ("plugin_contract", CONTRACT_SRC, "pub enum SurfaceTier {"),
        (
            "plugin_contract",
            CONTRACT_SRC,
            "pub struct PluginSurface {",
        ),
        (
            "plugin_contract",
            CONTRACT_SRC,
            "pub struct PluginContract {",
        ),
        (
            "plugin_contract",
            CONTRACT_SRC,
            "pub enum ContractVerdict {",
        ),
        (
            "plugin_contract",
            CONTRACT_SRC,
            "pub struct PluginCompatibilityError {",
        ),
        (
            "plugin_conformance",
            CONFORMANCE_SRC,
            "pub struct ConformanceConfig {",
        ),
    ];

    for (module, source, decl) in promised {
        let carries = carries_non_exhaustive(source, decl)
            .unwrap_or_else(|| panic!("`{decl}` not found in {module} — was it renamed?"));
        assert!(
            carries,
            "`{decl}` in {module} is documented as #[non_exhaustive] but does not carry the \
             attribute. Either add it back, or stop promising it in CHANGELOG.md and \
             docs/migrations/next.md."
        );
    }
}

// ── 2. The compatibility contract ──────────────────────────────────────────

#[test]
fn a_contract_matching_this_framework_is_compatible() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web(current_minor_series());
    assert!(
        matches!(
            evaluate(&contract, AUTUMN_WEB_VERSION),
            ContractVerdict::Compatible
        ),
        "a plugin declaring this framework's own series must be accepted"
    );
}

#[test]
fn an_undeclared_contract_is_undeclared_not_incompatible() {
    let contract = PluginContract::new("autumn-plugin-demo");
    assert!(matches!(
        evaluate(&contract, AUTUMN_WEB_VERSION),
        ContractVerdict::Undeclared
    ));
}

#[test]
fn an_excluding_range_is_incompatible_and_the_diagnostic_names_both_versions() {
    let contract = PluginContract::new("autumn-plugin-demo")
        .plugin_version("2.3.4")
        .autumn_web("0.1");
    let ContractVerdict::Incompatible(err) = evaluate(&contract, "9.9.9") else {
        panic!("0.1 must not accept autumn-web 9.9.9");
    };
    let msg = err.to_string();
    assert!(msg.contains("autumn-plugin-demo"), "{msg}");
    assert!(msg.contains("2.3.4"), "the plugin's own version: {msg}");
    assert!(msg.contains("0.1"), "the declared range: {msg}");
    assert!(msg.contains("9.9.9"), "the framework version in use: {msg}");
}

#[test]
fn the_incompatibility_diagnostic_names_both_remedies() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.1");
    let ContractVerdict::Incompatible(err) = evaluate(&contract, "9.9.9") else {
        panic!("expected incompatible");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("cargo update"),
        "a diagnostic that does not say what to do is a wall: {msg}"
    );
    assert!(
        msg.contains("autumn-web = \"0.1\""),
        "the other remedy is pinning the framework the plugin supports: {msg}"
    );
}

#[test]
fn a_comparator_range_is_evaluated_with_cargo_semantics() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web(">=0.6, <0.9");
    assert!(matches!(
        evaluate(&contract, "0.7.0"),
        ContractVerdict::Compatible
    ));
    assert!(matches!(
        evaluate(&contract, "0.9.0"),
        ContractVerdict::Incompatible(_)
    ));
}

#[test]
fn a_prerelease_framework_build_satisfies_its_own_series() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.7");
    assert!(
        matches!(
            evaluate(&contract, "0.7.0-rc.1"),
            ContractVerdict::Compatible
        ),
        "a release candidate of the series the plugin supports is not a mismatch"
    );
}

/// A requirement that names a prerelease is pinning one deliberately, so the
/// framework version's prerelease must NOT be stripped — otherwise an exact pin
/// fails against the very build it names.
#[test]
fn an_exact_prerelease_pin_matches_that_prerelease() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web("=0.8.0-rc.1");
    assert!(matches!(
        evaluate(&contract, "0.8.0-rc.1"),
        ContractVerdict::Compatible
    ));
    assert!(
        matches!(
            evaluate(&contract, "0.8.0"),
            ContractVerdict::Incompatible(_)
        ),
        "the pin still means only that build"
    );
}

#[test]
fn an_unparseable_requirement_is_reported_rather_than_silently_ignored() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web("not a version req");
    let ContractVerdict::Unparseable { requirement, .. } = evaluate(&contract, AUTUMN_WEB_VERSION)
    else {
        panic!("a malformed requirement must not read as Compatible");
    };
    assert_eq!(requirement, "not a version req");
}

#[test]
fn an_unparseable_framework_version_is_reported() {
    let contract = PluginContract::new("autumn-plugin-demo").autumn_web("0.7");
    assert!(matches!(
        evaluate(&contract, "not-a-version"),
        ContractVerdict::Unparseable { .. }
    ));
}

#[test]
fn experimental_surfaces_are_carried_on_the_contract() {
    let contract = PluginContract::new("autumn-plugin-demo")
        .autumn_web("0.7")
        .uses_experimental("AppBuilder::with_edge_kv");
    assert_eq!(
        contract.experimental_surfaces,
        vec!["AppBuilder::with_edge_kv".to_owned()]
    );
}

#[test]
fn a_contract_round_trips_through_json() {
    let contract = PluginContract::new("autumn-plugin-demo")
        .plugin_version("1.2.3")
        .autumn_web("0.7")
        .uses_experimental("AppBuilder::with_edge_kv");
    let json = serde_json::to_string(&contract).expect("serialize");
    let back: PluginContract = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.plugin, contract.plugin);
    assert_eq!(back.plugin_version, contract.plugin_version);
    assert_eq!(back.autumn_web, contract.autumn_web);
    assert_eq!(back.experimental_surfaces, contract.experimental_surfaces);
}

#[test]
fn the_dump_marker_is_a_stable_machine_readable_prefix() {
    assert_eq!(PLUGIN_CONTRACT_MARKER, "[autumn:plugin-contract] ");
}

// ── 3. Enforcement on the builder ──────────────────────────────────────────

struct ContractPlugin {
    requirement: Option<&'static str>,
    experimental: Vec<&'static str>,
}

impl ContractPlugin {
    const fn supporting(requirement: &'static str) -> Self {
        Self {
            requirement: Some(requirement),
            experimental: vec![],
        }
    }
}

impl Plugin for ContractPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-plugin-contract-fixture")
    }

    fn contract(&self) -> Option<PluginContract> {
        let mut c = PluginContract::new("autumn-plugin-contract-fixture").plugin_version("1.0.0");
        if let Some(req) = self.requirement {
            c = c.autumn_web(req);
        }
        for e in &self.experimental {
            c = c.uses_experimental(*e);
        }
        Some(c)
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app
    }
}

struct NoContractPlugin;

impl Plugin for NoContractPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("autumn-plugin-no-contract")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app
    }
}

#[test]
fn a_plugin_without_a_contract_still_registers() {
    let builder = autumn_web::app().plugin(NoContractPlugin);
    assert!(builder.has_plugin("autumn-plugin-no-contract"));
    assert!(
        builder.plugin_contracts().is_empty(),
        "no declaration means nothing to record"
    );
}

#[test]
fn a_compatible_plugin_registers_and_its_contract_is_recorded() {
    let builder = autumn_web::app().plugin(ContractPlugin::supporting(current_minor_series()));
    assert!(builder.has_plugin("autumn-plugin-contract-fixture"));
    let contracts = builder.plugin_contracts();
    assert_eq!(contracts.len(), 1);
    assert_eq!(contracts[0].plugin, "autumn-plugin-contract-fixture");
}

#[test]
#[should_panic(expected = "autumn-plugin-contract-fixture")]
fn an_incompatible_plugin_fails_loudly_at_registration() {
    with_contract_env(None, || {
        let _ = autumn_web::app().plugin(ContractPlugin::supporting("0.0"));
    });
}

#[test]
fn the_registration_panic_names_the_framework_version_in_use() {
    let panic = with_contract_env(None, || {
        std::panic::catch_unwind(|| {
            let _ = autumn_web::app().plugin(ContractPlugin::supporting("0.0"));
        })
        .expect_err("an incompatible plugin must not register")
    });
    let msg = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(msg.contains(AUTUMN_WEB_VERSION), "{msg}");
    assert!(msg.contains("0.0"), "{msg}");
}

/// The one thing an application author cannot fix is a stale literal in
/// somebody else's crate, so the panic names its own escape hatch.
///
/// Serialised against the other env-reading tests in this file: `AppBuilder`
/// reads the variable at registration, and the process environment is shared.
#[test]
fn the_escape_hatch_downgrades_the_panic_to_a_warning() {
    let registered = with_contract_env(Some("warn"), || {
        autumn_web::app()
            .plugin(ContractPlugin::supporting("0.0"))
            .has_plugin("autumn-plugin-contract-fixture")
    });
    assert!(
        registered,
        "AUTUMN_PLUGIN_CONTRACT=warn must let the plugin register"
    );
}

#[test]
fn any_other_value_of_the_escape_hatch_still_panics() {
    let panicked = with_contract_env(Some("yes-please"), || {
        std::panic::catch_unwind(|| {
            let _ = autumn_web::app().plugin(ContractPlugin::supporting("0.0"));
        })
        .is_err()
    });
    assert!(panicked, "only the exact value `warn` opts out");
}

#[test]
fn the_panic_text_names_the_escape_hatch() {
    let panic = with_contract_env(None, || {
        std::panic::catch_unwind(|| {
            let _ = autumn_web::app().plugin(ContractPlugin::supporting("0.0"));
        })
        .expect_err("an incompatible plugin must not register")
    });
    let msg = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(msg.contains("AUTUMN_PLUGIN_CONTRACT=warn"), "{msg}");
}

/// `AppBuilder` fills this in from `Plugin::name()`, which is a different
/// identity from the contract's crate name. `autumn plugin-check` matches on
/// either, so both have to survive the dump.
#[test]
fn the_builder_records_the_name_the_plugin_registered_under() {
    let builder = autumn_web::app().plugin(ContractPlugin::supporting(current_minor_series()));
    let contracts = builder.plugin_contracts();
    assert_eq!(
        contracts[0].registered_as.as_deref(),
        Some("autumn-plugin-contract-fixture")
    );
}

#[test]
fn a_plugin_declaring_experimental_surface_registers_and_records_it() {
    let builder = autumn_web::app().plugin(ContractPlugin {
        requirement: Some(current_minor_series()),
        experimental: vec!["AppBuilder::with_edge_kv"],
    });
    let contracts = builder.plugin_contracts();
    assert_eq!(contracts.len(), 1);
    assert_eq!(
        contracts[0].experimental_surfaces,
        vec!["AppBuilder::with_edge_kv".to_owned()]
    );
}

#[test]
fn a_duplicate_registration_records_the_contract_once() {
    let builder = autumn_web::app()
        .plugin(ContractPlugin::supporting(current_minor_series()))
        .plugin(ContractPlugin::supporting(current_minor_series()));
    assert_eq!(builder.plugin_contracts().len(), 1);
}

// ── 4. The conformance harness reports experimental dependence ─────────────

mod conformance {
    use autumn_web::plugin_conformance::{CheckStatus, ConformanceConfig, run_conformance};
    use autumn_web::plugin_contract::PluginContract;
    use autumn_web::route_listing::{RouteInfo, RouteSource};

    fn routes() -> Vec<RouteInfo> {
        vec![RouteInfo {
            method: "GET".to_owned(),
            path: "/demo".to_owned(),
            handler: "demo::index".to_owned(),
            source: RouteSource::Plugin("autumn-plugin-demo".to_owned()),
            ..RouteInfo::default()
        }]
    }

    fn check<'a>(
        report: &'a autumn_web::plugin_conformance::ConformanceReport,
        name: &str,
    ) -> &'a autumn_web::plugin_conformance::CheckResult {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no `{name}` check in the report"))
    }

    #[test]
    fn without_a_contract_the_experimental_check_is_skipped() {
        let config = ConformanceConfig::new("autumn-plugin-demo").prefix("/demo");
        let report = run_conformance(&config, &routes());
        let result = check(&report, "experimental-surface");
        assert_eq!(result.status, CheckStatus::Skip);
        assert!(
            result.message.contains("contract"),
            "the skip has to say how to opt in: {}",
            result.message
        );
    }

    #[test]
    fn a_stable_only_plugin_passes_with_no_diagnostics() {
        let config = ConformanceConfig::new("autumn-plugin-demo")
            .prefix("/demo")
            .contract(PluginContract::new("autumn-plugin-demo").autumn_web("0.7"));
        let report = run_conformance(&config, &routes());
        let result = check(&report, "experimental-surface");
        assert_eq!(result.status, CheckStatus::Pass);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn a_plugin_on_experimental_surface_is_reported_by_name() {
        let config = ConformanceConfig::new("autumn-plugin-demo")
            .prefix("/demo")
            .contract(
                PluginContract::new("autumn-plugin-demo")
                    .autumn_web("0.7")
                    .uses_experimental("AppBuilder::with_edge_kv"),
            );
        let report = run_conformance(&config, &routes());
        let result = check(&report, "experimental-surface");
        assert_eq!(
            result.status,
            CheckStatus::Pass,
            "leaning on experimental surface is reported, not failed"
        );
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.contains("AppBuilder::with_edge_kv")),
            "{:?}",
            result.diagnostics
        );
        assert!(
            report.passed(),
            "the overall report still passes; this is a report, not a gate"
        );
    }

    #[test]
    fn an_experimental_surface_diagnostic_carries_the_registry_note() {
        let config = ConformanceConfig::new("autumn-plugin-demo")
            .prefix("/demo")
            .contract(
                PluginContract::new("autumn-plugin-demo")
                    .autumn_web("0.7")
                    .uses_experimental("AppBuilder::with_edge_kv"),
            );
        let report = run_conformance(&config, &routes());
        let result = check(&report, "experimental-surface");
        let joined = result.diagnostics.join("\n");
        let note = autumn_web::plugin_contract::surface("AppBuilder::with_edge_kv")
            .expect("registry entry")
            .note;
        assert!(joined.contains(note), "{joined}");
    }

    #[test]
    fn a_surface_name_not_in_the_registry_fails_the_check() {
        let config = ConformanceConfig::new("autumn-plugin-demo")
            .prefix("/demo")
            .contract(
                PluginContract::new("autumn-plugin-demo")
                    .autumn_web("0.7")
                    .uses_experimental("AppBuilder::typo_not_a_surface"),
            );
        let report = run_conformance(&config, &routes());
        let result = check(&report, "experimental-surface");
        assert_eq!(
            result.status,
            CheckStatus::Fail,
            "a declaration nobody can resolve is worse than no declaration"
        );
        assert!(!report.passed());
    }

    #[test]
    fn declaring_a_stable_surface_as_experimental_fails_the_check() {
        let config = ConformanceConfig::new("autumn-plugin-demo")
            .prefix("/demo")
            .contract(
                PluginContract::new("autumn-plugin-demo")
                    .autumn_web("0.7")
                    .uses_experimental("Plugin::build"),
            );
        let report = run_conformance(&config, &routes());
        assert_eq!(
            check(&report, "experimental-surface").status,
            CheckStatus::Fail
        );
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Serialises every test that depends on `AUTUMN_PLUGIN_CONTRACT` — the ones
/// that set it *and* the ones that require it unset.
///
/// `temp_env` mutates the process environment, and this binary runs its tests
/// concurrently, so without a lock a `should_panic` test could observe another
/// test's `warn` (or lose its own). The crate is already a dev-dependency and
/// keeps the `unsafe` out of a crate that forbids it.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    // A test that panics inside the lock poisons the mutex; the poison carries
    // no state this cares about, so take the guard anyway.
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `body` with `AUTUMN_PLUGIN_CONTRACT` set to `value` (or removed for
/// `None`), holding the serialising lock for its duration.
fn with_contract_env<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _lock = env_lock();
    temp_env::with_var("AUTUMN_PLUGIN_CONTRACT", value, body)
}

/// The `MAJOR.MINOR` series of the framework this test binary links against —
/// the requirement a lockstep first-party plugin declares.
fn current_minor_series() -> &'static str {
    // `AUTUMN_WEB_VERSION` is a compile-time constant, so this leak is one
    // allocation for the life of the test process.
    let mut parts = AUTUMN_WEB_VERSION.split('.');
    let major = parts.next().unwrap_or("0");
    let minor = parts.next().unwrap_or("0");
    Box::leak(format!("{major}.{minor}").into_boxed_str())
}
