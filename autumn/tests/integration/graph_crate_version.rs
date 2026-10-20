//! The graph macros record the declaring crate's version (#2520 item 14).
//!
//! The manifest adds the version to a node id when two versions of one crate
//! give the same id. Every element in this binary comes from one package.
use autumn_web::graph::{
    JobGraphDescriptor, ModelGraphDescriptor, RepositoryGraphDescriptor, RouteGraphDescriptor,
};
use autumn_web::reexports::inventory;
use autumn_web::{get, routes};

#[get("/graph-crate-version")]
async fn graph_crate_version_probe() -> &'static str {
    "ok"
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn a_route_descriptor_carries_its_crate_version() {
    let route = inventory::iter::<RouteGraphDescriptor>
        .into_iter()
        .find(|d| d.handler == "graph_crate_version_probe")
        .expect("the probe route is registered");
    assert_eq!(route.crate_version, VERSION);
    assert_eq!(routes![graph_crate_version_probe].len(), 1);
}

#[test]
fn every_descriptor_in_the_binary_carries_the_crate_version() {
    let versions = inventory::iter::<RouteGraphDescriptor>
        .into_iter()
        .map(|d| d.crate_version)
        .chain(
            inventory::iter::<ModelGraphDescriptor>
                .into_iter()
                .map(|d| d.crate_version),
        )
        .chain(
            inventory::iter::<RepositoryGraphDescriptor>
                .into_iter()
                .map(|d| d.crate_version),
        )
        .chain(
            inventory::iter::<JobGraphDescriptor>
                .into_iter()
                .map(|d| d.crate_version),
        );
    for version in versions {
        assert_eq!(version, VERSION);
    }
}

#[cfg(feature = "db")]
#[test]
fn model_and_repository_descriptors_are_present() {
    assert!(
        inventory::iter::<ModelGraphDescriptor>
            .into_iter()
            .next()
            .is_some()
    );
    assert!(
        inventory::iter::<RepositoryGraphDescriptor>
            .into_iter()
            .next()
            .is_some()
    );
}
