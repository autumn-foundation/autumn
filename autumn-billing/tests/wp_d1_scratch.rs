//! TEMPORARY WP-D1 scratch target: compiles only support/reconcile/dunning
//! while sibling modules of the consolidated binary are still being written.
//! Deleted before hand-off.
mod cases {
    #[path = "dunning.rs"]
    mod dunning;
    #[path = "reconcile.rs"]
    mod reconcile;
    #[path = "support.rs"]
    pub mod support;
}
