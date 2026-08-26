//!
//! Tests for compile failures using trybuild.
//!
#[test]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();

    // Route macro failures (always available)
    t.compile_fail("tests/compile-fail/empty_path.rs");
    t.compile_fail("tests/compile-fail/missing_leading_slash.rs");
    t.compile_fail("tests/compile-fail/non_async.rs");
    t.compile_fail("tests/compile-fail/non_async_main.rs");
    t.compile_fail("tests/compile-fail/non_function.rs");
    t.compile_fail("tests/compile-fail/routes_nonexistent.rs");

    // Route-level `seo(...)` defaults (#1182): typos and repeated keys are
    // compile errors rather than silently-ignored metadata.
    t.compile_fail("tests/compile-fail/route_seo_unknown_key.rs");
    t.compile_fail("tests/compile-fail/route_seo_duplicate_key.rs");
    t.compile_fail("tests/compile-fail/route_seo_empty_group.rs");

    // Static route macro failures
    t.compile_fail("tests/compile-fail/static_get_path_params.rs");
    t.compile_fail("tests/compile-fail/static_get_non_async.rs");
    t.compile_fail("tests/compile-fail/static_get_params_no_placeholders.rs");
    t.compile_fail("tests/compile-fail/static_get_seo_unknown_key.rs");

    // Edge-lane refusals (#1790). Always available: `#[edge]` is re-exported
    // unconditionally (a route can be *marked* without the `edge` feature), and
    // each of these is rejected inside the route macro before any code is
    // emitted, so the fixtures never name `autumn_edge` and compile the same way
    // with or without the feature. The edge lane is read-path only, carries no
    // session or auth state, and adds nothing to a page that is already
    // pre-rendered CDN-side.
    t.compile_fail("tests/compile-fail/edge_on_post.rs");
    t.compile_fail("tests/compile-fail/edge_with_secured.rs");
    t.compile_fail("tests/compile-fail/edge_with_intercept.rs");
    t.compile_fail("tests/compile-fail/edge_with_extension.rs");
    t.compile_fail("tests/compile-fail/edge_on_static_get.rs");

    // Lifecycle macro failures (always available — the `lifecycle` macro is not
    // feature-gated). Firing an undeclared transition, leaving a terminal
    // state, starting from a non-initial state, or naming an unknown initial
    // state are all compile errors by construction (#1675).
    t.compile_fail("tests/compile-fail/lifecycle_undeclared_transition.rs");
    t.compile_fail("tests/compile-fail/lifecycle_terminal_has_no_exit.rs");
    t.compile_fail("tests/compile-fail/lifecycle_start_only_on_initial.rs");
    t.compile_fail("tests/compile-fail/lifecycle_unknown_initial.rs");
    t.compile_fail("tests/compile-fail/lifecycle_terminal_source.rs");

    // Model macro failures (require db feature)
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_on_enum.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_shard_key_unknown.rs");

    // Two m2m associations to the same target type with no `helper = "..."`
    // override collide on their target-derived mutation helpers (#1785).
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_m2m_helper_collision.rs");

    // `#[commentable]` compile-time guards (#1367). The counter is maintained
    // with `SET c = c + 1` and read back as `i64`, `commentable_id` is one
    // column, the emitted `{Model}Comments` trait can only exist once, and the
    // depth cap has to stay measurable by the runtime's recursive probe — each
    // is a directed error rather than a runtime surprise.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_commentable_missing_counter_column.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_commentable_counter_not_i64.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_commentable_duplicate.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_commentable_composite_key.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_commentable_max_depth_too_large.rs");
    // `author_id` is `i64` everywhere in the comments API, so a non-integer
    // author key is a compile error rather than a 401 from every POST.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_commentable_author_key_not_integer.rs");

    // Declarative reactions (#1362): every `#[votable(...)]` misuse is a
    // directed compile error rather than a runtime surprise on the first vote.
    // `by =` is required (no positional head); only one `#[votable]` per model
    // (the `{Model}Reactions` methods would collide); `sum`/`count` are the
    // only aggregates; the aggregate column must exist on the model (otherwise
    // a runtime `42703`); and `value_column` is meaningless in count mode.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_votable_missing_by.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_votable_duplicate.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_votable_unknown_aggregate.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_votable_missing_aggregate_column.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_votable_value_column_in_count_mode.rs");

    // Counter caches (#1325): `counter_cache` is a `belongs_to` option (the
    // child owns the foreign key and runs the maintenance), the column name is
    // spliced into generated SQL so it must be a plain identifier, and two legs
    // resolving onto one column would double-count every insert.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_counter_cache_on_has_many.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_counter_cache_bad_column.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_counter_cache_duplicate_column.rs");

    // Declarative-schema markers (#1975, slice 3.5): the `#[model]` macro
    // ACCEPTS `#[model(managed)]` / `#[unique]` / `#[references(...)]` but
    // rejects malformed shapes with a clear, actionable `compile_error!`.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_bogus_arg.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_managed_with_args.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_unique_with_args.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_references_bad_key.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_references_namevalue.rs");

    // #1911: `#[state_machine(lifecycle = T)]` where `T` is not a `#[lifecycle]`
    // enum fails with an unsatisfied `T: Lifecycle` trait bound.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/state_machine_lifecycle_not_lifecycle.rs");

    // Repository hooks failures (require db feature)
    #[cfg(feature = "db")]
    compile_repository_hooks_not_default(&t);

    // Cached macro failures
    t.compile_fail("tests/compile-fail/cached_self_receiver.rs");

    // `policy = T` rejects a type that doesn't impl `Policy<Model>`
    // at compile time, closing the silent-typo / wrong-type path that
    // would otherwise only fail at request time with `500`.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_invalid_policy_type.rs");

    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_bulk_upsert_many_hooks.rs");

    // `story!` blocks must be zero-arg pure functions: the block is coerced
    // to a plain `fn() -> Markup`, so environment capture cannot compile
    // (issue #1526).
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/story_captures_environment.rs");

    // Typed accessible UI primitives (#1706): an accessible name is a
    // compile-time obligation, so inaccessible construction does not build.
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_img_missing_alt.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_button_missing_name.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_textfield_unlabeled.rs");
    // The presentational/validation attributes must not open a render path for
    // an unlabeled field: setting them all and calling `.render()` still fails.
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_textfield_attrs_unlabeled.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_link_missing_text.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_menuitem_missing_name.rs");
    // The multi-line / dropdown / boolean / file-input form primitives carry the
    // same type-level label obligation as `TextField`: an unlabeled one has no
    // `.render()` and does not build.
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_textarea_unlabeled.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_select_unlabeled.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_checkbox_unlabeled.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_filefield_unlabeled.rs");

    // Compile-time query budgets (#1667). Always available: `#[query_budget]`
    // is re-exported unconditionally and the analysis is purely syntactic, so
    // these fixtures name no database types of their own.
    t.compile_fail("tests/compile-fail/query_budget_n_plus_one.rs");
    t.compile_fail("tests/compile-fail/query_budget_over_budget.rs");
    t.compile_fail("tests/compile-fail/query_budget_opaque_helper.rs");
    t.compile_fail("tests/compile-fail/query_budget_loop_closure.rs");
    t.compile_fail("tests/compile-fail/query_budget_macro_body.rs");
    t.compile_fail("tests/compile-fail/query_budget_bad_attr.rs");
}

#[test]
fn compile_pass_tests() {
    let t = trybuild::TestCases::new();

    // Route macro passes (always available)
    t.pass("tests/compile-pass/valid_handlers.rs");
    t.pass("tests/compile-pass/async_main.rs");
    t.pass("tests/compile-pass/static_get_basic.rs");
    t.pass("tests/compile-pass/static_routes_basic.rs");
    t.pass("tests/compile-pass/static_get_parameterized.rs");

    // Interceptor macro
    t.pass("tests/compile-pass/intercept_basic.rs");

    // Lifecycle macro (always available): a well-formed lifecycle builds and
    // exercises the typestate machine + metadata (#1675).
    t.pass("tests/compile-pass/lifecycle_valid.rs");

    // Compile-time query budgets (#1667): every in-budget handler shape, plus
    // the three escape hatches, plus the `StaticQueryBudget` proof the
    // expansion leaves behind.
    t.pass("tests/compile-pass/query_budget_valid.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/query_budget_route.rs");

    // Maud + form/json handlers (require maud feature)
    #[cfg(feature = "maud")]
    t.pass("tests/compile-pass/json_form_handlers.rs");

    // Typed accessible UI primitives (#1706): the accessible forms build.
    #[cfg(feature = "maud")]
    t.pass("tests/compile-pass/a11y_primitives.rs");

    // Model derive (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_derive.rs");

    // Declarative-schema markers (#1975, slice 3.5): `#[model(managed)]`,
    // `#[unique]`, and `#[references(...)]` are accepted, validated, and
    // stripped — the model still generates its normal write types.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_schema_markers.rs");

    // Model field enum (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_field_enum.rs");

    // Two m2m associations to the same target type disambiguated by distinct
    // `helper = "..."` overrides — the followers/following pattern (#1785).
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_m2m_helper_override.rs");

    // Declarative reactions (#1362): `#[votable]` with every override key set,
    // and again on a soft-deleted target — both emitter branches build, and
    // the attribute is stripped before the Diesel struct is emitted.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_votable_overrides.rs");

    // Counter caches (#1325): the bare flag, an explicit column override, a
    // nullable foreign key, a soft-deleting child, and a `belongs_to` with no
    // counter cache at all — every branch of the spec emitter, plus the
    // convention-derived names asserted at run time.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_counter_cache.rs");

    // Model draft accessors (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_draft_accessors.rs");

    // Model factory builder (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_factory.rs");

    // Encrypted column field attribute (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_encrypted.rs");

    // Full versioned repository over an encrypted model (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_encrypted.rs");

    // A HOOKS-enabled repository over an encrypted model (requires db feature).
    // `hooks = ...` / `broadcasts = true` route updates through the hooks-aware
    // `update_many` path, which must bind an OWNED proposed row to `.set(..)`:
    // diesel implements `AsChangeset` only for the owned model once a field
    // uses `serialize_as`, as every `#[encrypted]` field does (#1340).
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_encrypted_hooks.rs");

    // Sharding extractors + repository with_pool over a shard (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/sharded_handlers.rs");

    // Model factory composition (#[factory_assoc]) — requires db feature
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_factory_composition.rs");

    // Repository compile-pass (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_no_hooks.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_replica_reads.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_with_hooks.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_hooks_serde_skipped_model.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_with_api.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_with_hooks_and_api.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_with_policy.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_policy_non_serialize_new.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_api_validated.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_api_cursor.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_versioned.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_tenant_scoped_versioned_optional_tenant.rs");

    // Cached macro
    t.pass("tests/compile-pass/cached_basic.rs");
    t.pass("tests/compile-pass/cached_result.rs");

    // One-off operational task macro
    t.pass("tests/compile-pass/task_basic.rs");
    t.pass("tests/compile-pass/scheduled_coordination.rs");

    // #[job] with a three-arg (AppState, Args, JobContext) signature and the
    // generated enqueue_tracked/enqueue_tracked_for companions
    t.pass("tests/compile-pass/job_tracked_three_arg.rs");

    // WebSocket macro (requires ws feature)
    #[cfg(feature = "ws")]
    t.pass("tests/compile-pass/ws_basic.rs");

    // Optimistic concurrency control: #[lock_version] (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_lock_version.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_lock_version.rs");

    // Declarative state machines: #[state_machine(transitions(...))] (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_state_machine.rs");

    // #1911: `#[state_machine(lifecycle = Enum)]` derives its table from a
    // `#[lifecycle]` enum.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_state_machine_lifecycle.rs");

    // #1973: an `on_commit = <Job>` edge emits the connection-taking
    // `transition_{field}_to_on_conn` method; guards + effects compose.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_state_machine_on_commit.rs");

    // #1973: a sync `on = "handler"` edge also emits the connection-taking
    // method; `on` composes with `guard` and `on_commit` on one edge.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_state_machine_on.rs");

    // Issue #1973: `lifecycle = <Enum>` + binding-site `effects(...)` per-edge
    // effects converge onto the shared connection-taking method; the transition
    // table still comes from the enum.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_state_machine_lifecycle_effects.rs");

    // Soft delete (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_soft_delete.rs");

    // shard_key model attribute (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_shard_key.rs");

    // Sharded repository: self-routing FromRequestParts (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_sharded.rs");
}

#[cfg(feature = "db")]
#[rustversion::before(1.95)]
fn compile_repository_hooks_not_default(t: &trybuild::TestCases) {
    t.compile_fail("tests/compile-fail/repository_hooks_not_default.rs");
}

#[cfg(feature = "db")]
#[rustversion::since(1.95)]
fn compile_repository_hooks_not_default(t: &trybuild::TestCases) {
    t.compile_fail("tests/compile-fail/repository_hooks_not_default_1_95.rs");
}
