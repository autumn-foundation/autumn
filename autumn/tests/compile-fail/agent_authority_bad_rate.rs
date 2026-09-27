//! A declared cap is only useful if it parses the same way for every reader.
//! `rate` is `<n>/<sec|min|hour|day>`; prose is rejected at the declaration
//! rather than surfacing in the manifest as an uninterpretable string.

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        rate: "ten per minute",
        reversibility: compensable,
    }
}

fn main() {}
