//! Prospect assay follow-up (PR #2546 review, Codex P2, round 3): a bare
//! `.await` on a fallible accessor — no `?` — yields the `Result` itself,
//! not the handle inside it. `expr_is_handle` must not promote that `Result`
//! to a handle, or a harmless `result.is_err()` / `.unwrap()` call gets
//! miscounted as a database query. Only `Expr::Try` (the `?` that actually
//! unwraps to the handle) triggers the accessor-name check; a matching
//! `Expr::Await` arm was deliberately *not* added to `expr_is_handle` for
//! exactly this reason (see the comment there).

use autumn_web::query_budget;

struct Conn;

struct Store;

impl Store {
    async fn conn(&self) -> Result<Conn, ()> {
        Ok(Conn)
    }
}

#[query_budget(0)]
async fn check_connectivity(store: Store) -> bool {
    let result = store.conn().await;
    result.is_err()
}

fn main() {}
