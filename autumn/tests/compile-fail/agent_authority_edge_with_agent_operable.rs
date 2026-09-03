// The route macro replaces the whole handler with its diagnostic, which leaves
// the imports unused. Silenced so the golden below is the macro error alone.
#![allow(unused_imports)]

//! The edge lane is read-path only: it carries no session, no auth state, and
//! no audit sink. An agent-operable action is a mutating, audited call by
//! construction, so stacking the two attributes is a compile error in either
//! order.

use autumn_web::{agent_operable, edge, get};

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[get("/refunds")]
#[edge]
#[agent_operable(grant = RefundDrafter)]
async fn list() -> &'static str {
    "refunds"
}

fn main() {}
