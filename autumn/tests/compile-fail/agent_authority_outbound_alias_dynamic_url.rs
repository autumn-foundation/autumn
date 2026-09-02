//! A configured client alias names a *host*, not a licence: `named("stripe")`
//! resolves the base URL from typed config, so it can only stand in for a
//! relative literal. Handed a URL the analysis cannot read, the alias proves
//! nothing — and reading it first would let an agent-chosen absolute URL
//! travel while the manifest claimed the reach was `alias:stripe`.

use autumn_web::agent_operable;

struct Client;

impl Client {
    fn named(&self, _alias: &str) -> Self {
        Self
    }

    fn post(&self, _url: &str) -> Self {
        Self
    }

    async fn send(self) -> Result<(), ()> {
        Ok(())
    }
}

autumn_web::authority_grant! {
    /// Payout sync, allowed to reach Stripe through the configured client.
    pub PayoutSync {
        writes: [Payout],
        tenant_scope: scoped,
        outbound: ["alias:stripe"],
        reversibility: compensable,
    }
}

#[agent_operable(grant = PayoutSync)]
async fn sync(client: Client, callback_url: String) -> Result<(), ()> {
    client.named("stripe").post(&callback_url).send().await
}

fn main() {}
