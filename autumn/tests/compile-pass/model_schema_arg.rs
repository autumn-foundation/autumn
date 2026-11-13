// Compile-pass: `#[model(schema = "...")]` names the Postgres schema for the
// tooling. The `table!` module keeps its bare name, so codegen is unchanged.

mod schema {
    autumn_web::reexports::diesel::table! {
        marketing.campaigns (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema::campaigns;

#[autumn_web::model(managed, table = "campaigns", schema = "marketing")]
pub struct Campaign {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {
    let _new = NewCampaign {
        name: "spring".to_string(),
    };
}
