//!
//! Tests for compile failures using trybuild.
//!
#[test]
// A flat registry of trybuild fixtures: one `t.compile_fail(...)` per case, plus
// the comment explaining why each case must not compile. It grows by a line per
// guarantee and has no structure worth extracting.
#[allow(clippy::too_many_lines)]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();

    // Route macro failures (always available)
    t.compile_fail("tests/compile-fail/empty_path.rs");
    t.compile_fail("tests/compile-fail/missing_leading_slash.rs");
    t.compile_fail("tests/compile-fail/non_async.rs");
    t.compile_fail("tests/compile-fail/non_async_main.rs");
    t.compile_fail("tests/compile-fail/non_function.rs");
    t.compile_fail("tests/compile-fail/routes_nonexistent.rs");

    // Optional tokio runtime arguments on `#[autumn_web::main]`: a typo'd
    // argument, or one the chosen flavor would silently ignore, is a compile
    // error rather than a knob that quietly does nothing. Before these
    // arguments existed the attribute discarded its whole argument list.
    t.compile_fail("tests/compile-fail/main_unknown_runtime_arg.rs");
    t.compile_fail("tests/compile-fail/main_worker_threads_current_thread.rs");

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

    // Ledgered entities (issue #1699). A ledgered entity's history is the
    // record: every way of erasing or redacting it is refused at the repository
    // seam rather than silently weakening the as-of / tamper-evidence guarantee.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_ledgered_requires_soft_delete.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_ledgered_purge_rejected.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_ledgered_sensitive_columns.rs");

    // Model macro failures (require db feature)
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_on_enum.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_shard_key_unknown.rs");

    // `#[validate(nested)]` collides with this crate's own `ValidateExt` when
    // both are in scope in the struct's own defining module -- true of
    // `#[model]`-generated structs too, since `#[model]` forwards
    // `#[validate(...)]` verbatim (issue #1751). Not `db`-gated: reproduced
    // with a plain hand-rolled `#[derive(validator::Validate)]` struct, since
    // the hazard lives in `validator_derive` + `ValidateExt`, not in anything
    // `#[model]`-specific.
    t.compile_fail("tests/compile-fail/validate_nested_collides_with_validate_ext.rs");

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

    // Model-declared dependent cascades (#1702): `dependent = <action>` /
    // `on_delete = <action>` is a `has_many`/`has_one` option, only the four
    // documented actions are accepted, and it cannot ride on a `through =`
    // association (whose fk names a join-table column, not one on the target).
    // Each is a directed compile error rather than a silently-inert key.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_dependent_on_belongs_to.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_dependent_unknown_action.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_dependent_on_through.rs");

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

    // #1654: compile-time data classification. A classified column cannot reach
    // the `Json` response sink -- not as a whole model, not lifted into a DTO --
    // and a boundary declared for one field cannot release another's data. The
    // `.stderr` goldens pin that the diagnostic names the field and the sink.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_json_model_leak.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_json_field_leak.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_wrong_boundary.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_non_string.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_with_encrypted.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_released_for_sink_is_sealed.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_column_wrapper_cannot_retype.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_write_struct_leak.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/classified_factory_leak.rs");

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
}

/// The `state_migration!` fixtures get their own `TestCases` for the same
/// reason `query_budget` does: they are a self-contained feature (#1674) whose
/// guarantee — an in-place upgrade's old->new state mapping is total or the
/// build fails — is worth being able to run on its own.
#[test]
fn state_migration_compile_fail_tests() {
    let t = trybuild::TestCases::new();

    // Always available: `state_migration!` is exported unconditionally and the
    // fixtures name only the live-state traits and serde.
    //
    // A field of the new shape left unmapped is `missing field ... in
    // initializer` — the upgrade cannot quietly leave it at its default.
    t.compile_fail("tests/compile-fail/state_migration_missing_field.rs");
    // ...and there is no rest-pattern escape hatch to opt out with.
    t.compile_fail("tests/compile-fail/state_migration_rest_pattern.rs");
    // For an enum shape, a forgotten variant is a non-exhaustive `match`...
    t.compile_fail("tests/compile-fail/state_migration_missing_variant.rs");
    // ...and a catch-all arm is not expressible: the grammar takes variant
    // names, not patterns, so `_` is refused by the macro itself.
    t.compile_fail("tests/compile-fail/state_migration_wildcard_arm.rs");
    // A shape change without the matching `VERSION` bump is refused too: the
    // two shapes would be indistinguishable on the wire, so the migration
    // could never run and the old payload would be fed to the new shape.
    t.compile_fail("tests/compile-fail/state_migration_same_version.rs");
}

/// The `#[query_budget]` fixtures get their own `TestCases` rather than
/// riding along in `compile_fail_tests`: they are a self-contained feature
/// (#1667), and keeping them separate holds that function under the
/// `clippy::too_many_lines` ceiling.
#[test]
fn query_budget_compile_fail_tests() {
    let t = trybuild::TestCases::new();

    // Compile-time query budgets (#1667). Always available: `#[query_budget]`
    // is re-exported unconditionally and the analysis is purely syntactic, so
    // these fixtures name no database types of their own.
    t.compile_fail("tests/compile-fail/query_budget_n_plus_one.rs");
    t.compile_fail("tests/compile-fail/query_budget_over_budget.rs");
    t.compile_fail("tests/compile-fail/query_budget_opaque_helper.rs");
    t.compile_fail("tests/compile-fail/query_budget_loop_closure.rs");
    t.compile_fail("tests/compile-fail/query_budget_macro_body.rs");
    t.compile_fail("tests/compile-fail/query_budget_bad_attr.rs");
    // The same N+1, against the real generated repository surface.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/query_budget_repository_n_plus_one.rs");
    // Prospect assay (ledger, 2026-09-06): the accessor-tracking path
    // (`state.db()`) that a `#[job]`/`#[scheduled]` handler is structurally
    // limited to, since neither macro's signature can name a typed
    // `Db`/`…Repository` parameter the way a route handler does. Both catch
    // the N+1 with no code change to the analysis — see the report for the
    // full assay.
    t.compile_fail("tests/compile-fail/query_budget_accessor_handle_n_plus_one.rs");
    t.compile_fail("tests/compile-fail/query_budget_job_shaped_accessor_n_plus_one.rs");
    // The real `#[job]`/`#[scheduled]` attributes stacked with
    // `#[query_budget]` (PR #2546 review): the fixtures above prove the
    // accessor-tracking mechanism, but only the real attributes prove the
    // two macros actually compose against each other.
    t.compile_fail("tests/compile-fail/query_budget_real_job_accessor_n_plus_one.rs");
    t.compile_fail("tests/compile-fail/query_budget_real_scheduled_accessor_n_plus_one.rs");
    // A handle obtained through an async/fallible accessor (PR #2546 review,
    // round 2): `self.conn().await?`, the real shape in
    // `autumn-search/src/postgres.rs`'s `write_documents`.
    t.compile_fail("tests/compile-fail/query_budget_await_try_accessor_n_plus_one.rs");
    // The `.expect(...)`/`.unwrap()` idiom `autumn/src/seed.rs` documents as
    // its own canonical usage (PR #2546 review, round 5) — the same
    // accessor-tracking gap as the `?` shape above, for a different
    // unwrapping spelling.
    t.compile_fail("tests/compile-fail/query_budget_expect_accessor_n_plus_one.rs");
}

/// Every `#[agent_operable]` / `authority_grant!` compile-fail fixture, with
/// whether it needs the `db` feature. Shared with the guide-drift test below,
/// so the guide's violation matrix is pinned against the fixtures that
/// actually run rather than a hand-maintained copy of the list (#1691).
const AGENT_AUTHORITY_FIXTURES: &[(&str, bool)] = &[
    // A write to a model the grant never names.
    ("agent_authority_unlisted_write", false),
    // `writes: [X]` never implies the authority to erase the table.
    ("agent_authority_unbounded_write", false),
    // `tenant_scope: scoped` means the action stays in its tenant.
    ("agent_authority_cross_tenant", false),
    // A literal URL outside the outbound allowlist.
    ("agent_authority_outbound_not_allowlisted", false),
    // A `format!`-built URL proves nothing about the host reached.
    ("agent_authority_outbound_dynamic_url", false),
    // A client alias stands in for a relative literal, never for a URL the
    // analysis cannot read — the exfiltration shape the alias branch hid.
    ("agent_authority_outbound_alias_dynamic_url", false),
    // A job the grant does not list, enqueued through the free function that
    // has no signature handle to key on.
    ("agent_authority_job_not_listed", false),
    // A helper handed a tracked handle is opaque, never assumed effect-free.
    ("agent_authority_opaque_helper", false),
    // Including an *associated* one: an uppercase path segment is a shape, not
    // evidence that the callee is framework surface.
    ("agent_authority_opaque_associated_helper", false),
    // `#[agent_operable]` with no `grant = ...`.
    ("agent_authority_bad_attr", false),
    // `#[agent_effect]`'s reason is what makes the assertion reviewable.
    ("agent_authority_blank_effect_reason", false),
    // The statement hatch is not a handler-wide licence.
    ("agent_authority_stray_effect_on_fn", false),
    // `reversibility` is the one required grant key.
    ("agent_authority_missing_reversibility", false),
    // A declared cap that no reader can interpret is not a cap.
    ("agent_authority_bad_rate", false),
    // The hatch declares, it never grants.
    ("agent_authority_declared_effect_outside_grant", false),
    // The edge lane is read-only; an audited agent action cannot run there.
    ("agent_authority_edge_with_agent_operable", false),
    // An invented grant key is refused rather than silently dropped.
    ("agent_authority_unknown_grant_key", false),
    // The same unlisted write against the real generated repository surface,
    // where the model subject is resolved through the repository type.
    ("agent_authority_repository_unlisted_write", true),
];

/// The `#[agent_operable]` fixtures get their own `TestCases` for the same
/// reason `#[query_budget]` does: a self-contained feature (#1691) worth being
/// able to run on its own, and one fewer line in the umbrella registry.
#[test]
fn agent_authority_compile_fail_tests() {
    let t = trybuild::TestCases::new();

    // Build-time authority envelopes (#1691). Mostly always-available: the
    // analysis is syntactic, so the fixtures name local stand-in types rather
    // than a database surface. The one exception is gated on `db`.
    for (fixture, needs_db) in AGENT_AUTHORITY_FIXTURES {
        if *needs_db && !cfg!(feature = "db") {
            continue;
        }
        t.compile_fail(format!("tests/compile-fail/{fixture}.rs"));
    }
}

/// The `#[agent_operable]` compile-*pass* half (#1691), for the reason its
/// compile-fail sibling has its own test: a self-contained feature worth
/// running on its own, and two fewer lines in the `compile_pass_tests_*` halves,
/// which are already over the line limit.
#[test]
fn agent_authority_compile_pass_tests() {
    let t = trybuild::TestCases::new();

    // Every proved effect, both hatch forms, and the effect-free handler —
    // the fixture asserts its own manifest rows in `main`.
    t.pass("tests/compile-pass/agent_authority_valid.rs");
    // The same analysis against the real route/`#[repository]` surface, with
    // the attribute stacked in both orders and under `#[secured]`.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/agent_authority_route.rs");
}

/// Build-time cache coherence (#1716). Its own `TestCases` for the same
/// reason `#[query_budget]` has one: a self-contained feature, and one fewer
/// line in the umbrella registry.
#[test]
fn cache_coherence_compile_fail_tests() {
    let t = trybuild::TestCases::new();

    // The declaration surface refuses to accept a claim it cannot defend.
    t.compile_fail("tests/compile-fail/cached_reads_empty.rs");
    t.compile_fail("tests/compile-fail/cached_acknowledge_stale_blank_reason.rs");

    // An invalidation edge is resolved by rustc: `invalidates(path)` rewrites
    // to the id constant `#[cached]` generates beside the function, so naming
    // anything else cannot compile.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_invalidates_unknown_read.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_invalidates_empty.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/repository_acknowledge_stale_blank_reason.rs");
}

// Split into `_a` / `_b` halves so CI can run them as two parallel trybuild
// shards (see the `trybuild` job in .github/workflows/ci.yml). Each half owns a
// disjoint slice of the SAME fixture list — nothing is gated on the split, so a
// new fixture may be appended to either half. `compile_pass` cases are the
// expensive ones: unlike a `compile_fail` case, which stops at the first
// diagnostic, each one compiles AND links a whole crate against autumn-web —
// which is why they were 25 of the 37 minutes trybuild spent on Windows in the
// run that motivated the split.
#[test]
fn compile_pass_tests_a() {
    let t = trybuild::TestCases::new();

    // Build-time cache coherence (#1716): a declared dependency set, an
    // acknowledged-stale opt-out, macro-derived dependencies, and both a
    // trait-level and a method-level invalidation edge.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/cached_coherence.rs");

    // `#[validate(nested)]` compiles cleanly when the struct's own defining
    // module does not import `ValidateExt`/the prelude, even though another
    // module in the same crate does -- the workaround for the collision in
    // `validate_nested_collides_with_validate_ext.rs` (issue #1751).
    t.pass("tests/compile-pass/validate_nested_without_validate_ext.rs");

    // Route macro passes (always available)
    t.pass("tests/compile-pass/valid_handlers.rs");
    t.pass("tests/compile-pass/async_main.rs");
    t.pass("tests/compile-pass/main_runtime_args.rs");
    t.pass("tests/compile-pass/main_runtime_current_thread.rs");
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
    // A ledgered repository (issue #1699) type-checks end to end: the ledger
    // write emitted into every version-history site, the generated
    // `LedgeredRecord` impl (default and `valid_time = "..."` variants), and the
    // as-of / diff / verify / head query surface.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/repository_ledgered.rs");
    t.pass("tests/compile-pass/query_budget_valid.rs");
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/query_budget_route.rs");
    // Prospect assay control (ledger, 2026-09-06): the job-shaped accessor
    // pattern batched ahead of the loop compiles clean — the analysis is
    // actually counting, not just always rejecting the job/scheduled shape.
    t.pass("tests/compile-pass/query_budget_job_shaped_accessor_batched.rs");
    // A bare `.await` (no `?`) on a fallible accessor must not promote the
    // `Result` itself to a handle (PR #2546 review, round 3) — otherwise
    // `result.is_err()` here would be miscounted as a database query.
    t.pass("tests/compile-pass/query_budget_bare_await_not_promoted.rs");
    // An awaited call whose name collides with a `HANDLE_BUILDERS` entry is
    // the terminal query, not a handle-refining step (PR #2546 review,
    // round 4) — its result must not be promoted to a handle either.
    t.pass("tests/compile-pass/query_budget_awaited_builder_name_not_promoted.rs");

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
}

// The second half of the `compile_pass` fixture list; see `compile_pass_tests_a`.
#[test]
fn compile_pass_tests_b() {
    let t = trybuild::TestCases::new();

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

    // #1654: a `#[classified]` column released at a declared declassification
    // boundary is a plain value again and reaches the `Json` sink.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/classified_declassify.rs");
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

/// The `#[query_budget]` guide is the reference a developer reaches for when a
/// build fails, so the diagnostic it prints has to be the diagnostic the macro
/// actually emits (#1667). Pins the guide against the trybuild golden, and
/// against the compile-fail fixtures it names.
#[test]
fn query_budget_guide_matches_the_real_diagnostics() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let guide = std::fs::read_to_string(root.join("docs/guide/query-budgets.md"))
        .expect("docs/guide/query-budgets.md exists");
    let golden = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/compile-fail/query_budget_n_plus_one.stderr"),
    )
    .expect("N+1 golden exists");

    // The guide reproduces the error verbatim; compare on collapsed whitespace
    // so the two wrappings (markdown block vs. rustc gutter) don't matter.
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide_flat = collapse(&guide);
    let golden_flat = collapse(&golden);

    let message_start = golden_flat
        .find("`#[query_budget(2)]` cannot be proven")
        .expect("golden carries the budget diagnostic");
    let message_end = golden_flat
        .find("--> tests/compile-fail")
        .expect("golden carries a span line");
    let message = &golden_flat[message_start..message_end].trim_end();

    assert!(
        guide_flat.contains(message),
        "docs/guide/query-budgets.md has drifted from the real diagnostic.\n\n\
         expected the guide to contain:\n{message}\n\n\
         Regenerate with TRYBUILD=overwrite and copy the message into the guide."
    );

    // Every fixture the guide points at must exist.
    for fixture in [
        "autumn/tests/compile-fail/query_budget_n_plus_one.rs",
        "autumn/tests/compile-pass/query_budget_valid.rs",
    ] {
        assert!(
            guide.contains(fixture),
            "guide no longer references {fixture}"
        );
        assert!(
            root.join(fixture).exists(),
            "guide references a fixture that does not exist: {fixture}"
        );
    }
}

/// The `#[agent_operable]` guide is the reference a developer reaches for when
/// a grant violation stops the build, so the diagnostic it prints has to be
/// the diagnostic the macro actually emits (#1691). Pins the guide against the
/// trybuild golden, and against the fixtures the suite registers.
#[test]
fn agent_authority_guide_matches_the_real_diagnostics() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let guide = std::fs::read_to_string(root.join("docs/guide/agent-authority.md"))
        .expect("docs/guide/agent-authority.md exists");
    let golden = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/compile-fail/agent_authority_unlisted_write.stderr"),
    )
    .expect("unlisted-write golden exists");

    // The guide reproduces the message verbatim; compare on collapsed
    // whitespace so the two wrappings (markdown block vs. rustc gutter) don't
    // matter. Only the message itself is pinned, never const-eval's own
    // framing around it — that text changes with the toolchain.
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide_flat = collapse(&guide);
    let golden_flat = collapse(&golden);

    let message_start = golden_flat
        .find("agent authority:")
        .expect("golden carries the authority diagnostic");
    let message_end = golden_flat[message_start..]
        .find("docs/guide/agent-authority.md")
        .map(|end| message_start + end + "docs/guide/agent-authority.md".len())
        .expect("golden's diagnostic ends with the guide link");
    let message = &golden_flat[message_start..message_end];

    assert!(
        guide_flat.contains(message),
        "docs/guide/agent-authority.md has drifted from the real diagnostic.\n\n\
         expected the guide to contain:\n{message}\n\n\
         Regenerate with TRYBUILD=overwrite and copy the message into the guide."
    );

    // The guide's violation matrix is the table a reviewer reads to find out
    // what the gate refuses. One row per registered fixture: a fixture missing
    // from it is a refusal nobody documented, and a row naming a fixture that
    // no longer exists is a promise the suite stopped keeping.
    //
    // The row's *label* is checked too. `E0080` (a failing const assertion:
    // the effect was proved and the grant did not cover it) and `macro` (the
    // macro refusing a site it cannot prove) are different promises to a
    // reader — one says "widen the grant", the other says "the analysis cannot
    // read this" — and a mislabelled row sends them to the wrong fix.
    for (fixture, needs_db) in AGENT_AUTHORITY_FIXTURES {
        assert!(
            guide.contains(fixture),
            "the guide's violation matrix has no row for {fixture}"
        );
        assert!(
            root.join(format!("autumn/tests/compile-fail/{fixture}.rs"))
                .exists(),
            "the fixture registry names a fixture that does not exist: {fixture}"
        );
        if *needs_db && !cfg!(feature = "db") {
            continue;
        }
        let golden = std::fs::read_to_string(
            root.join(format!("autumn/tests/compile-fail/{fixture}.stderr")),
        )
        .unwrap_or_else(|_| panic!("{fixture} has a committed golden"));
        let first_error = golden
            .lines()
            .find(|line| line.starts_with("error"))
            .unwrap_or_else(|| panic!("{fixture}'s golden opens with an error"));
        let row = guide
            .lines()
            .find(|line| line.starts_with('|') && line.contains(fixture))
            .unwrap_or_else(|| panic!("the guide's matrix row for {fixture} is not a table row"));
        let labelled_e0080 = row.contains("`E0080`");
        let labelled_macro = row.contains("`macro`");
        assert!(
            labelled_e0080 != labelled_macro,
            "the guide's row for {fixture} must carry exactly one of `E0080` / `macro`: {row}"
        );
        assert_eq!(
            labelled_e0080,
            first_error.starts_with("error[E0080]"),
            "the guide labels {fixture} wrongly.\n\nrow: {row}\ngolden: {first_error}"
        );
    }

    // Every fixture the guide points at must exist.
    for fixture in [
        "autumn/tests/compile-fail/agent_authority_unlisted_write.rs",
        "autumn/tests/compile-pass/agent_authority_valid.rs",
    ] {
        assert!(
            guide.contains(fixture),
            "guide no longer references {fixture}"
        );
        assert!(
            root.join(fixture).exists(),
            "guide references a fixture that does not exist: {fixture}"
        );
    }
}
