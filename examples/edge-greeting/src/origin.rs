//! The origin-only half of the app: everything the edge lane cannot serve.
//!
//! This module is `#[cfg(not(target_arch = "wasm32"))]` in
//! [`lib.rs`](crate) and imports `autumn_web` freely — sessions, a database, a
//! write path, anything. None of it compiles into the capsule, and none of it
//! has to be split into a second crate.
//!
//! The origin also mounts every `#[edge]` route from [`crate::handlers`]. That
//! is what makes fallthrough free of author glue: the origin already serves
//! everything, so a request the edge declines needs no special handling — it
//! just arrives.

use autumn_web::prelude::*;

/// `POST /feedback` — the write path, which never leaves the origin.
///
/// There is no `#[edge]` here and there could not be: the macro refuses
/// `#[edge]` on anything but `#[get]`. At the edge this request is declined
/// with `method_not_edge_eligible` before a line of handler code runs, and the
/// host forwards it here.
#[post("/feedback")]
pub async fn feedback(note: String) -> String {
    format!(
        "Thanks — the origin recorded {} byte(s) of feedback.\n",
        note.len()
    )
}
