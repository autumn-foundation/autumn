//! A helper handed a tracked handle is opaque: it may write, call out, or
//! enqueue, and the analyser cannot see which. Assuming it is effect-free
//! would be a false proof, so the site is reported and the diagnostic names
//! the annotation that discharges it.

use autumn_web::agent_operable;

struct Refund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";
}

async fn finalize(_repo: &PgRefundRepository, _id: i64) -> Result<Refund, ()> {
    Ok(Refund)
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn draft(repo: PgRefundRepository) -> Result<Refund, ()> {
    finalize(&repo, 1).await
}

fn main() {}
