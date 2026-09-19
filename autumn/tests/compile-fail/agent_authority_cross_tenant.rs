//! `tenant_scope: scoped` means the action stays inside the tenant it was
//! invoked for. `across_tenants()` leaves it, so the handler needs
//! `tenant_scope: cross_tenant` — an agent that can pick the tenant is a
//! different threat model from one that cannot.

use autumn_web::agent_operable;

struct Refund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";

    fn across_tenants(&self) -> &Self {
        self
    }

    async fn save(&self, _refund: &Refund) -> Result<Refund, ()> {
        Ok(Refund)
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
async fn touch_every_tenant(repo: PgRefundRepository) -> Result<Refund, ()> {
    repo.across_tenants().save(&Refund).await
}

fn main() {}
