//! `writes: [Refund]` allows bounded row writes to `Refund`. It never implies
//! the authority to erase the table: an unbounded write is a separate
//! allowance (`unbounded_writes`), because the blast radius is the difference
//! between one row and all of them.

use autumn_web::agent_operable;

struct Refund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";

    async fn delete_all(&self) -> Result<usize, ()> {
        Ok(0)
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
    repo.delete_all().await
}

fn main() {}
