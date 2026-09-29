// Compile-pass: declarative-schema markers on a model (#1975, slice 3.5).
//
// Acceptance-only: `#[model(managed)]`, `#[unique]`, and `#[references(...)]`
// (both the explicit `table = "..."` form and the bare, inferred form) must be
// ACCEPTED and validated by the `#[model]` macro, stripped from the generated
// query struct, and change NO codegen — the model still generates its normal
// New*/Update* types over the Diesel table below.

mod schema {
    autumn_web::reexports::diesel::table! {
        memberships (id) {
            id -> Int8,
            slug -> Text,
            account_id -> Int8,
            team_id -> Int8,
        }
    }
}

use schema::memberships;

#[autumn_web::model(managed)]
pub struct Membership {
    #[id]
    pub id: i64,
    #[unique]
    pub slug: String,
    #[references(table = "accounts")]
    pub account_id: i64,
    // Bare `#[references]` — the target table is inferred from the field name
    // by the CLI toolchain; the macro simply accepts and strips it.
    #[references]
    pub team_id: i64,
}

fn build() {
    // The markers must not touch the generated write types: NewMembership still
    // carries the plain user columns.
    let _new = NewMembership {
        slug: "acme".to_string(),
        account_id: 1,
        team_id: 2,
    };
    let _update = UpdateMembership {
        slug: autumn_web::hooks::Patch::Set("beta".to_string()),
        account_id: autumn_web::hooks::Patch::Unchanged,
        team_id: autumn_web::hooks::Patch::Unchanged,
    };
    let _row = Membership {
        id: 1,
        slug: "acme".to_string(),
        account_id: 1,
        team_id: 2,
    };
}

fn main() {
    build();
}
