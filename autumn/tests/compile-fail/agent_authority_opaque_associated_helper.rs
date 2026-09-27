//! An associated function is not framework surface just because its path
//! starts with an uppercase segment.
//!
//! `Billing::wipe(repo)` and a generated static finder are the same shape, and
//! the helper can perform `repo.delete_all()` inside — under a grant that
//! allows no unbounded write at all. The analysis cannot read the callee, so
//! it refuses the site and names both hatches rather than assuming the call is
//! harmless.

use autumn_web::agent_operable;

struct Refund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";

    async fn delete_all(&self) -> Result<usize, ()> {
        Ok(0)
    }
}

struct Billing;

impl Billing {
    async fn wipe(repo: &PgRefundRepository) -> Result<usize, ()> {
        repo.delete_all().await
    }
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        unbounded_writes: [],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn purge(repo: PgRefundRepository) -> Result<usize, ()> {
    Billing::wipe(&repo).await
}

fn main() {}
