// The route macro replaces the whole handler with its diagnostic, which leaves
// the imports unused. Silenced so the golden below is the macro error alone —
// rustc's unused-import note text is not a thing this fixture is testing.
#![allow(unused_imports)]

use autumn_web::{edge, get};

#[derive(Clone)]
struct TenantConfig;

// `Extension<T>` extracts from any router state, so the EdgeHandler bound
// alone cannot refuse it — but the capsule installs no request extensions, and
// a missing one would be served as axum's 500 instead of falling through to
// the origin. The macro refuses the combination outright.
#[get("/tenant")]
#[edge]
async fn tenant(ext: axum::Extension<TenantConfig>) -> &'static str {
    "tenant"
}

fn main() {}
