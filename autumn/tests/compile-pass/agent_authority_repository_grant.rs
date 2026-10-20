//! `#[repository(.., mcp, grant = X)]` governs every generated MCP tool
//! (#2451). The grant lists the model, so the build passes, and each tool
//! carries the authority that keeps it off the `ungoverned_tools` list.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_grant_widgets (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema::aa_grant_widgets;

#[autumn_web::model]
pub struct AaGrantWidget {
    #[id]
    pub id: i64,
    pub name: String,
}

autumn_web::authority_grant! {
    /// May write widgets.
    pub WidgetEditor {
        writes: [AaGrantWidget],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[autumn_web::repository(AaGrantWidget, api = "/api/aa-grant-widgets", mcp, grant = WidgetEditor)]
pub trait AaGrantWidgetRepository {}

fn main() {
    let routes = [
        __autumn_route_info_aa_grant_widget_api_list(),
        __autumn_route_info_aa_grant_widget_api_get(),
        __autumn_route_info_aa_grant_widget_api_create(),
        __autumn_route_info_aa_grant_widget_api_update(),
        __autumn_route_info_aa_grant_widget_api_delete(),
    ];
    for route in routes {
        let authority = route
            .api_doc
            .agent_authority
            .expect("a granted repository governs every MCP tool it generates");
        assert_eq!(authority.grant.name, "WidgetEditor");
    }
}
