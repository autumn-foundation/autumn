//! The same unlisted write, against the real `#[model]` / `#[repository]`
//! surface (#1691). The generated `PgAaNoteRepository` publishes the model it
//! writes, so the subject the grant is checked against is the model itself —
//! resolved through the repository type, not guessed from its name — and the
//! check is a const assertion, so it holds even when the grant is declared in
//! another crate.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_refunds (id) {
            id -> Int8,
            amount -> Int8,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_notes (id) {
            id -> Int8,
            body -> Text,
        }
    }
}

use autumn_web::prelude::*;
use schema::{aa_notes, aa_refunds};

#[autumn_web::model]
pub struct AaRefund {
    #[id]
    pub id: i64,
    pub amount: i64,
}

#[autumn_web::model]
pub struct AaNote {
    #[id]
    pub id: i64,
    pub body: String,
}

#[autumn_web::repository(AaRefund)]
pub trait AaRefundRepository {}

#[autumn_web::repository(AaNote)]
pub trait AaNoteRepository {}

autumn_web::authority_grant! {
    /// Draft-only refund authority for the support agent.
    pub RefundDrafter {
        writes: [AaRefund],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = RefundDrafter)]
async fn annotate(notes: PgAaNoteRepository, note: NewAaNote) -> AutumnResult<AaNote> {
    notes.save(&note).await
}

fn main() {}
