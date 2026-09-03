//! `#[agent_operable(grant = ...)]` accepts every handler shape whose proved
//! effects stay inside the declared grant (#1691), and leaves a readable proof
//! behind for the manifest.
//!
//! The types here are local stand-ins named exactly like the framework ones —
//! the analysis keys on the effect-issuing *surface* (`…Repository` handles,
//! `Client`, `enqueue`), so the fixture proves the gate without dragging a
//! database feature into a compile-time test. A generated repository publishes
//! its model as `__AUTUMN_MODEL_IDENT`, so the stand-in does too.

use autumn_web::agent_authority::{EffectKind, EffectProvenance, Reversibility, TenantScope};
use autumn_web::agent_operable;

struct Refund;
struct NewRefund;

struct PgRefundRepository;

impl PgRefundRepository {
    pub const __AUTUMN_MODEL_IDENT: &str = "Refund";

    async fn find_all(&self) -> Result<Vec<Refund>, ()> {
        Ok(Vec::new())
    }

    async fn count(&self) -> Result<i64, ()> {
        Ok(0)
    }

    async fn create(&self, _new: &NewRefund) -> Result<Refund, ()> {
        Ok(Refund)
    }

    async fn save(&self, _refund: &Refund) -> Result<Refund, ()> {
        Ok(Refund)
    }

    async fn delete_all(&self) -> Result<usize, ()> {
        Ok(0)
    }

    fn on_primary(&self) -> &Self {
        self
    }
}

struct Client;

impl Client {
    fn post(&self, _url: &str) -> Self {
        Self
    }

    async fn send(self) -> Result<(), ()> {
        Ok(())
    }
}

struct NotifyFinance;

impl NotifyFinance {
    async fn enqueue(_refund: &Refund) -> Result<(), ()> {
        Ok(())
    }
}

async fn finalize(_repo: &PgRefundRepository, _id: i64) -> Result<Refund, ()> {
    Ok(Refund)
}

fn render(_rows: &[Refund]) -> String {
    String::new()
}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [Refund],
        unbounded_writes: [],
        tenant_scope: scoped,
        outbound: ["https://api.stripe.com/v1/refunds"],
        webhooks: [],
        jobs: [NotifyFinance],
        rate: "10/min",
        spend: "500.00 USD",
        reversibility: compensable,
    }
}

autumn_web::authority_grant! {
    /// Housekeeping authority: the same models, plus the authority to erase
    /// them — which is a separate allowance, and an irreversible one.
    pub RefundJanitor {
        writes: [Refund],
        unbounded_writes: [Refund],
        tenant_scope: scoped,
        reversibility: irreversible,
    }
}

/// A read is not an effect: the action is registered with an empty effect set
/// rather than dropped from the manifest.
#[agent_operable(grant = RefundDrafter)]
async fn list_refunds(repo: PgRefundRepository) -> Result<usize, ()> {
    Ok(repo.find_all().await?.len())
}

/// The conforming handler: one granted write, one allowlisted host, one listed
/// job — the three dimensions that have no signature chokepoint between them.
#[agent_operable(grant = RefundDrafter)]
async fn draft_refund(
    repo: PgRefundRepository,
    client: Client,
    new: NewRefund,
) -> Result<Refund, ()> {
    let refund = repo.create(&new).await?;
    client
        .post("https://api.stripe.com/v1/refunds")
        .send()
        .await?;
    NotifyFinance::enqueue(&refund).await?;
    Ok(refund)
}

/// Builder refinements change *how* the write runs, not *what* it writes.
#[agent_operable(grant = RefundDrafter)]
async fn save_refund(repo: PgRefundRepository) -> Result<Refund, ()> {
    repo.on_primary().save(&Refund).await
}

/// An unbounded write is a separate allowance, and it carries a reversibility
/// floor: this grant says `irreversible`, so the floor is satisfied.
#[agent_operable(grant = RefundJanitor)]
async fn purge_refunds(repo: PgRefundRepository) -> Result<usize, ()> {
    repo.delete_all().await
}

/// Escape hatch 1: an opaque helper declares the effects it performs. They are
/// checked against the grant exactly like proved ones.
#[agent_operable(grant = RefundDrafter)]
async fn declared_helper(repo: PgRefundRepository) -> Result<Refund, ()> {
    #[agent_effect(writes(Refund), reason = "finalize() performs the row write")]
    let refund = finalize(&repo, 1).await?;
    Ok(refund)
}

/// Escape hatch 2: a statement verified effect-free is discharged, with the
/// reason recorded next to the code it excuses.
#[agent_operable(grant = RefundDrafter)]
async fn effect_free_helper(repo: PgRefundRepository) -> Result<String, ()> {
    let rows = repo.find_all().await?;
    #[agent_effect(none, reason = "pure formatting helper; verified effect-free")]
    let summary = render(&rows);
    Ok(summary)
}

/// A loop that only reads issues no effect at all, however many rows it walks.
#[agent_operable(grant = RefundDrafter)]
async fn summarise(repo: PgRefundRepository) -> Result<i64, ()> {
    let rows = repo.find_all().await?;
    let mut total = repo.count().await?;
    for _row in &rows {
        total += 1;
    }
    Ok(total)
}

fn main() {
    // The expansion leaves a readable proof behind for the manifest and for
    // MCP's `destructiveHint`.
    let read_only = &__AUTUMN_AGENT_AUTHORITY_list_refunds;
    assert_eq!(read_only.action, "list_refunds");
    assert_eq!(read_only.grant.name, "RefundDrafter");
    assert!(
        read_only.effects.is_empty(),
        "a read-only action proves no effects"
    );
    assert!(matches!(read_only.grant.tenant_scope, TenantScope::Scoped));
    assert_eq!(read_only.grant.rate, Some("10/min"));
    assert_eq!(read_only.grant.spend, Some("500.00 USD"));

    let draft = &__AUTUMN_AGENT_AUTHORITY_draft_refund;
    assert!(
        draft
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Write) && e.subject == "Refund"),
        "the row write is proved: {:?}",
        draft.effects
    );
    assert!(
        draft
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Outbound)
                && e.subject == "https://api.stripe.com/v1/refunds"),
        "the outbound host is proved: {:?}",
        draft.effects
    );
    assert!(
        draft
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Job) && e.subject == "NotifyFinance"),
        "the job is proved: {:?}",
        draft.effects
    );
    assert!(
        !draft
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::UnboundedWrite)),
        "a bounded write is not an unbounded one: {:?}",
        draft.effects
    );

    let purge = &__AUTUMN_AGENT_AUTHORITY_purge_refunds;
    assert!(
        purge
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::UnboundedWrite) && e.subject == "Refund"),
        "the unbounded write is proved: {:?}",
        purge.effects
    );
    assert!(matches!(
        purge.grant.reversibility,
        Reversibility::Irreversible
    ));

    // A declared effect is in the set, and says so.
    let declared = &__AUTUMN_AGENT_AUTHORITY_declared_helper;
    assert!(
        declared
            .effects
            .iter()
            .any(|e| matches!(e.kind, EffectKind::Write)
                && e.subject == "Refund"
                && matches!(e.provenance, EffectProvenance::Declared)),
        "the declared effect carries its provenance: {:?}",
        declared.effects
    );

    // An `#[agent_effect(none, ...)]` site is counted rather than forgotten:
    // the row it belongs to is `declared`, not `provable`.
    assert_eq!(
        __AUTUMN_AGENT_AUTHORITY_effect_free_helper.asserted_effect_free_sites,
        1
    );

    assert!(__AUTUMN_AGENT_AUTHORITY_save_refund.effects.len() == 1);
    assert!(__AUTUMN_AGENT_AUTHORITY_summarise.effects.is_empty());
}
