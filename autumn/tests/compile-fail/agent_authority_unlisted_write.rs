//! An agent-operable handler may only write the models its grant names. This
//! one is granted `writes: [Refund]` and writes `Payout`, so the coverage
//! assertion the macro emits at the call site fails const-evaluation and the
//! build stops — on every branch, whether or not a test exercises this one.

use autumn_web::agent_operable;

struct Payout;
struct NewPayout;

struct PgPayoutRepository;

impl PgPayoutRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Payout";

    async fn create(&self, _new: &NewPayout) -> Result<Payout, ()> {
        Ok(Payout)
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

#[agent_operable(grant = RefundDrafter)]
async fn draft_payout(payouts: PgPayoutRepository) -> Result<Payout, ()> {
    payouts.create(&NewPayout).await
}

fn main() {}
