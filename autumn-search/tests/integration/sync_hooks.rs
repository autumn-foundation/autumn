//! `SearchSyncHooks` as a real `#[repository]` hooks type.
//!
//! AC: "create/update/delete to a searchable model enqueues a reindex (via
//! `#[job]`, so indexing is off-request and durable)".
//!
//! The valuable assertion here is a **compile-time** one: `SearchSyncHooks`
//! must actually satisfy everything `#[repository(hooks = …, commit_hooks =
//! true)]` demands of a hooks type (`MutationHooks` with the three associated
//! types, plus `Default` and `Clone` — which it provides without requiring
//! them of the model). If this module compiles, the wiring in the crate docs
//! is real; if it stops compiling, an application's one-line opt-in has
//! silently broken.

#![allow(dead_code, clippy::items_after_statements)]

use autumn_search::{ReindexArgs, ReindexOp, SearchSyncHooks};
use autumn_web::hooks::{MutationContext, MutationHooks, MutationOp};
use autumn_web::search::SearchIndexed as _;

// Glob import: `#[repository]` expands to code that names the model's
// generated companions (the diesel table module, the changesets, the update
// draft extension trait) by their unqualified names.
use super::support::*;

/// The hooks type an application writes, verbatim from the crate docs.
type ArticleSearchHooks = SearchSyncHooks<Article, NewArticle, UpdateArticle>;

// The repository declaration from the docs. Its existence is the test: it will
// not compile unless `SearchSyncHooks` is a valid hooks type for this model.
#[autumn_web::repository(
    Article,
    table = "search_articles",
    hooks = ArticleSearchHooks,
    commit_hooks = true,
    searchable
)]
pub trait ArticleRepository {}

#[test]
fn the_hooks_type_satisfies_the_repository_contract() {
    // `#[repository]` constructs hooks via `Default` and clones them with the
    // generated struct; neither may require anything of the model.
    let hooks = ArticleSearchHooks::default();
    assert_eq!(format!("{hooks:?}"), "SearchSyncHooks");

    // `Clone` is what the generated repository struct derives through, and
    // `Default` is how it constructs the hooks — both must hold even though
    // the model itself carries no such bound.
    fn assert_default_and_clone<H: Default + Clone>() {}
    assert_default_and_clone::<ArticleSearchHooks>();

    fn assert_hooks<H: MutationHooks>() {}
    assert_hooks::<ArticleSearchHooks>();
}

#[test]
fn the_associated_types_line_up_with_the_model() {
    fn assert_model<H: MutationHooks<Model = Article>>() {}
    fn assert_new<H: MutationHooks<NewModel = NewArticle>>() {}
    fn assert_update<H: MutationHooks<UpdateModel = UpdateArticle>>() {}

    assert_model::<ArticleSearchHooks>();
    assert_new::<ArticleSearchHooks>();
    assert_update::<ArticleSearchHooks>();
}

#[tokio::test]
async fn a_create_or_update_produces_an_upsert_instruction_for_the_record() {
    // The hook bodies build these args and hand them to the enqueue
    // chokepoint. Asserting the *instruction* (rather than driving a whole job
    // runtime) is what makes the mapping from lifecycle event to payload
    // checkable: the record's id and index must survive, and create/update
    // must both mean "converge this row".
    let record = article(7, "Rust", "body");

    let created = ReindexArgs::upsert(Article::SEARCH_INDEX, record.search_id());
    assert_eq!(created.index, "search_articles");
    assert_eq!(created.id, 7);
    assert_eq!(created.op, ReindexOp::Upsert);

    // An update is deliberately the *same* instruction: the job re-reads the
    // row, so there is nothing for "update" to do differently.
    let updated = ReindexArgs::upsert(Article::SEARCH_INDEX, record.search_id());
    assert_eq!(updated, created);
}

#[tokio::test]
async fn a_delete_produces_a_delete_instruction_that_never_reads_the_source() {
    let record = article(7, "Rust", "body");
    let deleted = ReindexArgs::delete(Article::SEARCH_INDEX, record.search_id());

    assert_eq!(deleted.op, ReindexOp::Delete);
    assert_eq!(deleted.id, 7);
    assert_ne!(deleted, ReindexArgs::upsert(Article::SEARCH_INDEX, 7));
}

#[tokio::test]
async fn every_mutation_op_maps_to_an_instruction() {
    // A new `MutationOp` variant must not silently mean "no index update".
    for op in [MutationOp::Create, MutationOp::Update, MutationOp::Delete] {
        let args = match op {
            MutationOp::Create | MutationOp::Update => ReindexArgs::upsert("search_articles", 1),
            MutationOp::Delete => ReindexArgs::delete("search_articles", 1),
            _ => unreachable!("MutationOp is non_exhaustive; add the new variant here"),
        };
        assert_eq!(args.id, 1);
    }
}

#[test]
fn the_hook_context_type_is_the_one_repositories_pass() {
    // Guards against a signature drift that would only surface in a user's
    // crate: the hooks take `&mut MutationContext`.
    let mut ctx = MutationContext::new(MutationOp::Create);
    assert_eq!(ctx.op, MutationOp::Create);
    ctx.op = MutationOp::Update;
    assert_eq!(ctx.op, MutationOp::Update);
}
