// The route macro replaces the whole handler with its diagnostic, which
// leaves the aliasing import unused. Silenced so the golden below is the
// macro error alone.
#![allow(unused_imports)]

// A proc-macro attribute never sees the enclosing module's `use`
// declarations, so an aliased `#[authorize]` is indistinguishable, by name,
// from any other attribute sharing its argument grammar. Guessing either way
// is unsafe for idempotency-replay ownership (Codex review on #2628), so this
// is a compile error rather than a silent guess.
use autumn_web::authorize as authz;
use autumn_web::post;

struct Note;

#[post("/notes/{id}")]
#[authz("update", resource = Note)]
async fn update_note() -> &'static str {
    "ok"
}

fn main() {}
