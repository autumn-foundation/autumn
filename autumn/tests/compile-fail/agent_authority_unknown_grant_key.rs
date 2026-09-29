//! A misspelled or invented grant key is refused rather than ignored: a key
//! the macro silently drops is an allowance the author believes they declared
//! and the manifest never carries.

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        audit: true,
        reversibility: compensable,
    }
}

fn main() {}
