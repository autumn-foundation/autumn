use crate::hooks::PageHooks;
use crate::models::{NewPage, Page, PageDraftExt, UpdatePage};
use crate::schema::pages;

#[autumn_web::repository(
    Page,
    hooks = PageHooks,
    api = "/api/v1/pages",
    searchable,
    commit_hooks = true
)]
pub trait PageRepository {
    fn find_by_slug(slug: String) -> Vec<Page>;
    fn find_by_status(status: String) -> Vec<Page>;
}
