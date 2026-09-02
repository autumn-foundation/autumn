//! `#[agent_operable]` composed with the real route macro, `#[api_doc(mcp)]`
//! and `#[secured]` (#1691).
//!
//! The sibling `agent_authority_valid.rs` fixture exercises the analysis
//! against local stand-in types; this one proves the attribute stacks on an
//! actual MCP-exposed handler in BOTH orders. The order matters: when the
//! route macro expands first it never sees `#[agent_operable]` at all, so it
//! reads the marker const from the body instead — and either way the route's
//! `ApiDoc` carries the authority, which is what keeps the handler on the
//! manifest.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_route_refunds (id) {
            id -> Int8,
            amount -> Int8,
        }
    }
}

use autumn_web::prelude::*;
use schema::aa_route_refunds;

#[autumn_web::model]
pub struct AaRouteRefund {
    #[id]
    pub id: i64,
    pub amount: i64,
}

#[autumn_web::repository(AaRouteRefund)]
pub trait AaRouteRefundRepository {}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [AaRouteRefund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

/// Route macro outermost: it expands first and never sees the attribute, so
/// the marker const inside the body is what fills `ApiDoc::agent_authority`.
#[post("/aa-refunds")]
#[api_doc(mcp, summary = "Draft a refund")]
#[agent_operable(grant = RefundDrafter)]
async fn draft_refund(
    repo: PgAaRouteRefundRepository,
    Json(new): Json<NewAaRouteRefund>,
) -> AutumnResult<Json<AaRouteRefund>> {
    Ok(Json(repo.save(&new).await?))
}

/// The other order: `#[agent_operable]` outermost, so the route macro sees the
/// live attribute.
#[agent_operable(grant = RefundDrafter)]
#[post("/aa-refunds/again")]
#[api_doc(mcp, summary = "Draft a refund, again")]
async fn draft_refund_again(
    repo: PgAaRouteRefundRepository,
    Json(new): Json<NewAaRouteRefund>,
) -> AutumnResult<Json<AaRouteRefund>> {
    Ok(Json(repo.save(&new).await?))
}

/// Stacked with the auth guard, which rewrites the body into an `async` block.
/// The analysis walks through that, so the effect set is the same as it would
/// be without it — pinned here because nothing else would notice if a guard's
/// rewrite started hiding writes.
#[post("/aa-refunds/secure")]
#[api_doc(mcp, summary = "Draft a refund as an admin")]
#[secured]
#[agent_operable(grant = RefundDrafter)]
async fn draft_refund_secure(
    repo: PgAaRouteRefundRepository,
    Json(new): Json<NewAaRouteRefund>,
) -> AutumnResult<Json<AaRouteRefund>> {
    Ok(Json(repo.save(&new).await?))
}

/// A read-only route with no grant carries no authority: the field is `None`,
/// and the manifest reports it as an ungoverned tool rather than inventing a
/// row for it.
#[get("/aa-refunds")]
#[api_doc(mcp, summary = "List refunds")]
async fn list_refunds(repo: PgAaRouteRefundRepository) -> AutumnResult<Json<Vec<AaRouteRefund>>> {
    Ok(Json(repo.find_all().await?))
}

fn main() {
    for route in [
        __autumn_route_info_draft_refund(),
        __autumn_route_info_draft_refund_again(),
        __autumn_route_info_draft_refund_secure(),
    ] {
        let authority = route
            .api_doc
            .agent_authority
            .expect("an agent-operable route carries its authority in the ApiDoc");
        assert_eq!(authority.grant.name, "RefundDrafter");
        assert!(
            authority
                .effects
                .iter()
                .any(|e| e.subject == "AaRouteRefund"),
            "the write is proved through the generated repository: {:?}",
            authority.effects
        );
    }

    assert!(
        __autumn_route_info_list_refunds()
            .api_doc
            .agent_authority
            .is_none(),
        "a route with no grant carries no authority"
    );
}
