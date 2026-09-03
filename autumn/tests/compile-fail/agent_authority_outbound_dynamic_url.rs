//! A `format!`-built URL proves nothing about the host that will be reached:
//! the base comes from config, the path from the request. The analyser refuses
//! the call site rather than recording an outbound effect it cannot defend,
//! and names the annotation that declares one deliberately.

use autumn_web::agent_operable;

struct Client;

impl Client {
    fn post(&self, _url: &str) -> Self {
        Self
    }

    async fn send(self) -> Result<(), ()> {
        Ok(())
    }
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        tenant_scope: scoped,
        outbound: ["https://api.stripe.com/v1/refunds"],
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn call_out(client: Client, base: String, id: i64) -> Result<(), ()> {
    let url = format!("{base}/v1/refunds/{id}");
    client.post(&url).send().await
}

fn main() {}
