//! The escape hatch declares, it never grants. `#[agent_effect(writes(Payout))]`
//! makes an opaque statement's effects visible to the analysis — and they are
//! then checked against the grant exactly like a proved one. Otherwise the
//! hatch would be a grant bypass with better ergonomics than deleting the
//! grant.

use autumn_web::agent_operable;

struct Refund;
struct Payout;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";
}

async fn issue(_repo: &PgRefundRepository) -> Result<Payout, ()> {
    Ok(Payout)
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
async fn draft(repo: PgRefundRepository) -> Result<Payout, ()> {
    #[agent_effect(writes(Payout), reason = "issue() performs the payout write")]
    let payout = issue(&repo).await?;
    Ok(payout)
}

fn main() {}
