// The route macro's `Extension<T>` refusal is a syntactic, token-level scan
// for the name `Extension` in a parameter's type — see
// `autumn_macros::route::has_extension_param`. A type alias hides that name,
// so the macro cannot catch this shape; the sealed `EdgeExtract` whitelist in
// `autumn_edge::handler` is what actually refuses it, at the `edge_get` call
// the route macro emits.
#![allow(unused_imports)]

use autumn_web::{edge, get};

#[derive(Clone)]
struct TenantConfig;

type Hidden = axum::Extension<TenantConfig>;

#[get("/tenant")]
#[edge]
async fn tenant(ext: Hidden) -> &'static str {
    let _ = ext;
    "tenant"
}

fn main() {}
