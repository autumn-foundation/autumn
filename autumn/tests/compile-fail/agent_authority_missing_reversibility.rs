//! `reversibility` is the one required key. A grant that does not say whether
//! its actions can be undone, compensated, or neither cannot be reviewed, and
//! it cannot drive the MCP `destructiveHint` — so it is refused at the
//! declaration rather than defaulted to the permissive answer.

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
    }
}

fn main() {}
