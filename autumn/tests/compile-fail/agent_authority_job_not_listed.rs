//! A background job outlives the request that enqueued it, so the set of jobs
//! an agent-operable action may start is part of its envelope. `wire_transfer`
//! is not in `jobs: [...]`, and a free-function `enqueue` is still an effect —
//! there is no signature handle to hide behind.

use autumn_web::agent_operable;

mod job {
    pub async fn enqueue(_name: &str, _payload: ()) -> Result<(), ()> {
        Ok(())
    }
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        jobs: [NotifyFinance],
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn draft() -> Result<(), ()> {
    job::enqueue("wire_transfer", ()).await
}

fn main() {}
