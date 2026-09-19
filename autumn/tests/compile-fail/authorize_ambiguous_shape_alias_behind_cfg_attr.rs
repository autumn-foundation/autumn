// The route macro replaces the whole handler with its diagnostic, which
// leaves the aliasing import unused. Silenced so the golden below is the
// macro error alone.
#![allow(unused_imports)]

// Same ambiguity as `authorize_ambiguous_shape_alias.rs`, but the aliased
// attribute is additionally wrapped in `#[cfg_attr(predicate, ...)]`. An
// earlier revision special-cased this in the macro itself, reasoning that
// `cfg_attr` stays unexpanded until every attribute macro has run (Codex
// review on #2628, fourth/fifth findings). Codex review (sixth finding)
// showed that reasoning was wrong for a *sibling* attribute macro on the
// same item: on the workspace MSRV, `cfg_attr` is resolved by the compiler
// before `#[post]` ever runs, regardless of which is written first, so by
// the time the route macro's ambiguous-shape scan sees this attribute list
// it already contains a plain, unwrapped `#[authz(...)]` -- the ordinary
// (non-`cfg_attr`) ambiguous-name path already refuses it, no special
// `cfg_attr` handling required. This fixture is the real, `rustc`-verified
// proof (a unit test constructing the input via `syn::parse_quote!` cannot
// observe this, since it bypasses the compiler's actual attribute-expansion
// order entirely).
use autumn_web::authorize as authz;
use autumn_web::post;

struct Note;

#[post("/notes/{id}")]
#[cfg_attr(all(), authz("update", resource = Note))]
async fn update_note() -> &'static str {
    "ok"
}

fn main() {}
