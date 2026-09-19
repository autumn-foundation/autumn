//! `#[agent_effect(none, ...)]` asserts that a statement the analyser cannot
//! read performs no effect at all. That assertion is only as good as the
//! reason recorded beside it, so a blank one is refused.

use autumn_web::agent_operable;

struct Refund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";
}

fn render(_repo: &PgRefundRepository) -> String {
    String::new()
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
async fn draft(repo: PgRefundRepository) -> Result<String, ()> {
    #[agent_effect(none, reason = "   ")]
    let summary = render(&repo);
    Ok(summary)
}

fn main() {}
