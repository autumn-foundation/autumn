//! Isolated integration test: `SandboxArtifact::seal` must refuse a manifest
//! from its own fields, not from its rendering.
//!
//! `seal` is a public packaging entry point and `SandboxManifest`'s fields are
//! public, so the caller chooses how large the value is. Validation happens in
//! `SandboxManifest::parse`, which only sees TOML — so rendering first means a
//! manifest that is refusable from its fields alone (over `MAX_ROUTES`) is
//! serialized into a second, larger copy on the way to being rejected.
//!
//! The verdict is identical either way — `seal` returns `Err` before and after
//! — which is why this is measured rather than asserted on the result. Both
//! measured calls are refusals differing only in route count, so the count is
//! the only variable; comparing a refusal against a manifest that seals would
//! measure the successful render instead and pass no matter what `seal` did.
//!
//! Its own test binary because `allocation-counter` installs a counting
//! `#[global_allocator]`, a process-wide side effect per CLAUDE.md's
//! isolated-test rules.

#![cfg(feature = "plugin-sandbox")]

use autumn_web::plugin_sandbox::{
    CapabilityGrants, CapabilityQuotas, DeclaredRoute, ResourceLimits, SandboxArtifact,
    SandboxCapability, SandboxManifest,
};

/// Far past `MAX_ROUTES` (256), so the route-count check refuses it before the
/// quadratic duplicate indexes are reserved — and, with this fix, before the
/// whole list is rendered to TOML.
const OVERSIZED_ROUTES: usize = 20_000;

/// The shortest list that is already over the ceiling, so both measured calls
/// are refused for the same reason at the same check.
const MINIMAL_OVERSIZED: usize = 257;

fn manifest(routes: usize) -> SandboxManifest {
    SandboxManifest {
        name: "autumn-plugin-hello".to_owned(),
        version: "0.1.0".to_owned(),
        wire_version: 1,
        prefix: "/hello".to_owned(),
        capabilities: vec![SandboxCapability::HttpRequest],
        sha256: "0".repeat(64),
        routes: (0..routes)
            .map(|i| DeclaredRoute {
                method: "GET".to_owned(),
                path: format!("/hello/r{i}"),
            })
            .collect(),
        limits: ResourceLimits::default(),
        grants: CapabilityGrants::default(),
        quotas: CapabilityQuotas::default(),
    }
}

#[test]
fn sealing_refuses_an_oversized_route_list_without_rendering_it() {
    let module =
        wat::parse_str(r#"(module (memory (export "memory") 1) (func (export "_start") (nop)))"#)
            .expect("the fixture is valid WAT");

    // Both manifests are built outside the measured windows. What is measured
    // is what `seal` does with a list it is handed, not the cost of building
    // one.
    let small = manifest(MINIMAL_OVERSIZED);
    let oversized = manifest(OVERSIZED_ROUTES);

    // Warm-up outside the windows.
    drop(SandboxArtifact::seal(
        manifest(MINIMAL_OVERSIZED),
        module.clone(),
    ));

    let baseline = allocation_counter::measure(|| {
        let outcome = SandboxArtifact::seal(small.clone(), module.clone());
        std::hint::black_box(&outcome);
    });
    let refused = allocation_counter::measure(|| {
        let outcome = SandboxArtifact::seal(oversized.clone(), module.clone());
        std::hint::black_box(&outcome);
    });

    // Sanity: both must actually be refusals, or the two windows are not
    // comparable and this would be measuring a successful render.
    assert!(
        SandboxArtifact::seal(manifest(MINIMAL_OVERSIZED), module.clone()).is_err(),
        "the shorter list must also be over MAX_ROUTES",
    );
    assert!(
        SandboxArtifact::seal(manifest(OVERSIZED_ROUTES), module).is_err(),
        "the oversized list must be refused",
    );

    // `clone()` inside each window copies the routes, so the floor is one copy
    // of the list; rendering it to TOML on top of that is what this refuses.
    // A generous multiple of the clone still catches a full render, which adds
    // roughly a path's worth of string per route.
    let extra = refused.bytes_total.saturating_sub(baseline.bytes_total);
    let per_route_floor = (OVERSIZED_ROUTES - MINIMAL_OVERSIZED) as u64;
    assert!(
        extra < per_route_floor * 64,
        "sealing a {OVERSIZED_ROUTES}-route manifest allocated {extra} bytes more than a \
         {MINIMAL_OVERSIZED}-route one — more than the clone the call itself makes, so the \
         list is being rendered before the route-count ceiling refuses it",
    );
}
