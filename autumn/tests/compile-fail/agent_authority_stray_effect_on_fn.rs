//! `#[agent_effect(...)]` declares what one *statement* does. On the handler
//! itself it would read as a licence covering the whole body, which is exactly
//! the grant-bypass the hatch must not become — the handler's envelope is the
//! grant, and nothing else.

use autumn_web::agent_operable;

struct Refund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";

    async fn find_all(&self) -> Result<Vec<Refund>, ()> {
        Ok(Vec::new())
    }
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_effect(writes(Refund), reason = "the whole handler writes")]
#[agent_operable(grant = RefundDrafter)]
async fn draft(repo: PgRefundRepository) -> Result<usize, ()> {
    Ok(repo.find_all().await?.len())
}

fn main() {}
