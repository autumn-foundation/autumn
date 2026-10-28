//! A governed delete through a cascade that loops is refused (#2451). Autumn
//! cannot list every model a loop reaches, so the grant cannot cover it.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_loop_as (id) {
            id -> Int8,
            b_id -> Nullable<Int8>,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_loop_bs (id) {
            id -> Int8,
            a_id -> Nullable<Int8>,
        }
    }
}

use autumn_web::prelude::*;
use schema::{aa_loop_as, aa_loop_bs};

#[autumn_web::model]
#[has_many(AaLoopB, fk = "a_id", dependent = destroy)]
pub struct AaLoopA {
    #[id]
    pub id: i64,
    pub b_id: Option<i64>,
}

#[autumn_web::model]
#[has_many(AaLoopA, fk = "b_id", dependent = destroy)]
pub struct AaLoopB {
    #[id]
    pub id: i64,
    pub a_id: Option<i64>,
}

#[autumn_web::repository(AaLoopA)]
pub trait AaLoopARepository {}

#[autumn_web::repository(AaLoopB)]
pub trait AaLoopBRepository {}

autumn_web::authority_grant! {
    /// Lists both models. That is not enough for a loop.
    pub LoopRemover {
        writes: [AaLoopA, AaLoopB],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = LoopRemover)]
async fn remove_a(items: PgAaLoopARepository, id: i64) -> AutumnResult<()> {
    items.delete_by_id(id).await?;
    Ok(())
}

fn main() {}
