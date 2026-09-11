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

    // An attribute matching #[authorize]'s argument grammar under a
    // different name is refused rather than guessed at, whether it's really
    // an aliased #[authorize] or an unrelated attribute that happens to
    // share the shape — see `authorize::reject_if_ambiguous_authorize_shape`
    // (Codex review on #2628, two rounds: guessing either direction is
    // unsafe for idempotency-replay ownership).
    t.compile_fail("tests/compile-fail/authorize_ambiguous_shape_alias.rs");
    t.compile_fail("tests/compile-fail/authorize_ambiguous_shape_unrelated.rs");
    // Same ambiguity reached through `#[cfg_attr(predicate, ...)]` -- proof
    // that this is refused via the ordinary (non-`cfg_attr`) path, since
    // `cfg_attr` is already resolved by the compiler before the route macro
    // ever sees this attribute (Codex review on #2628, sixth finding).
    t.compile_fail("tests/compile-fail/authorize_ambiguous_shape_alias_behind_cfg_attr.rs");

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
    // The sealed `EdgeExtract` whitelist (issue #2244) closes the two
    // bypasses the macro's own token-level `Extension` scan cannot see: a
    // type alias hiding the name, and the whole-`Request` extractor reading
    // `.extensions()` directly.
    t.compile_fail("tests/compile-fail/edge_with_extension_alias.rs");
    t.compile_fail("tests/compile-fail/edge_with_request.rs");
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
    // #1675 AC3: the whole-graph proofs the typestate cannot see. Registered
    // from the shared list so the guide-drift test below cannot pin a fixture
    // that no longer runs.
    for (fixture, _) in LIFECYCLE_GRAPH_FIXTURES {
        t.compile_fail(format!("tests/compile-fail/{fixture}.rs"));
    }

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

    // Derivations (#1769): one `filter` declaration is lowered to both a Rust
    // predicate and a SQL predicate, so the grammar admits only what provably
    // lowers the same way in both, its identifiers must name real fields, and
    // `sum` needs a non-nullable integer. `column` is required and, as for a
    // counter cache, two declarations onto one parent column would
    // double-count every insert.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_bad_filter.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_filter_non_field.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_missing_column.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_unknown_key.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_sum_non_integer.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_duplicate_column.rs");
    // The two macro-time guards that carry the injection argument: the
    // maintained column is spliced into `UPDATE <parent> SET ...`, and a brace
    // in a filter literal could forge the `{c}` child-alias placeholder.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_bad_column.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_brace_literal.rs");
    // A filter names the column after the Rust field, so a field renamed by
    // `#[diesel(column_name = ...)]` cannot appear in one.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_diesel_column_name.rs");
    // Two `#[belongs_to]` legs to one parent leave the default foreign key
    // ambiguous, and `tenant` names a column of the child.
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_ambiguous_fk.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_tenant_non_field.rs");
    // `sum(...)` takes exactly one field name, and `tenant` cannot name a field
    // renamed by `#[diesel(column_name = ...)]` (the lowering spells the
    // discriminator after the Rust field).
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_sum_extra_tokens.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_derivation_tenant_renamed.rs");

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
    // A radio group carries two obligations: a name for each choice and a name
    // for the group. Neither can be skipped.
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_radiogroup_unlabeled.rs");
    #[cfg(feature = "maud")]
    t.compile_fail("tests/compile-fail/a11y_radiooption_missing_label.rs");
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
/// The `#[lifecycle]` whole-graph fixtures (#1675), paired with the diagnostic
/// substring the guide must reproduce. Shared between the trybuild registration
/// above and the guide-drift test below, so the guide is pinned against
/// fixtures that actually run rather than a hand-maintained copy of the list.
const LIFECYCLE_GRAPH_FIXTURES: &[(&str, &str)] = &[
    (
        "lifecycle_unreachable_state",
        "state `Refunded` is unreachable",
    ),
    (
        "lifecycle_dead_end_state",
        "state `OnHold` is a non-terminal dead-end",
    ),
];

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

    // Derivations (#1769): every production of the filter grammar, both
    // transforms, a model carrying a counter cache and derivations at once, and
    // one with neither — each branch of the shared spec emitter, with the two
    // lowerings of every filter asserted at run time.
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_derivation.rs");
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

/// The lifecycle guide is where a developer lands when a lifecycle stops the
/// build, so the diagnostics it prints have to be the ones the macro actually
/// emits (#1675). Pins the guide against both whole-graph goldens, and against
/// the fixtures the suite registers.
#[test]
fn lifecycle_guide_matches_the_real_diagnostics() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let guide = std::fs::read_to_string(root.join("docs/guide/lifecycle.md"))
        .expect("docs/guide/lifecycle.md exists");

    // Collapse whitespace so the markdown block and the rustc gutter compare
    // equal however each is wrapped.
    let collapse = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide_flat = collapse(&guide);

    for (fixture, needle) in LIFECYCLE_GRAPH_FIXTURES {
        let golden = format!("{fixture}.stderr");
        let golden_text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/compile-fail")
                .join(&golden),
        )
        .unwrap_or_else(|_| panic!("{golden} exists"));
        let golden_flat = collapse(&golden_text);

        let start = golden_flat
            .find(*needle)
            .unwrap_or_else(|| panic!("{golden} carries the diagnostic"));
        let end = golden_flat
            .find("--> tests/compile-fail")
            .unwrap_or_else(|| panic!("{golden} carries a span line"));
        let message = golden_flat[start..end].trim_end();

        assert!(
            guide_flat.contains(message),
            "docs/guide/lifecycle.md has drifted from the real diagnostic.\n\n\
             expected the guide to contain:\n{message}\n\n\
             Regenerate with TRYBUILD=overwrite and copy the message into the guide."
        );
    }

    // Every fixture the guide points at must exist, and the guide must quote the
    // span line as well as the message — a fixture edit that shifts a line
    // number has to reach the page.
    let fixtures = LIFECYCLE_GRAPH_FIXTURES
        .iter()
        .map(|(fixture, _)| format!("autumn/tests/compile-fail/{fixture}.rs"))
        .chain(std::iter::once(
            "autumn/tests/compile-pass/lifecycle_valid.rs".to_owned(),
        ));
    for fixture in fixtures {
        assert!(
            guide.contains(&fixture),
            "guide no longer references {fixture}"
        );
        assert!(
            root.join(&fixture).exists(),
            "guide references a fixture that does not exist: {fixture}"
        );
    }
    for (fixture, _) in LIFECYCLE_GRAPH_FIXTURES {
        let golden = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/compile-fail")
                .join(format!("{fixture}.stderr")),
        )
        .expect("golden exists");
        let span = golden
            .lines()
            .find(|l| l.trim_start().starts_with("--> "))
            .expect("golden carries a span line")
            .trim();
        assert!(
            guide_flat.contains(&collapse(span)),
            "docs/guide/lifecycle.md is missing {fixture}'s span line:\n  {span}"
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

/// The "first run" journey's flagship code — README.md's `## Example` and the
/// runtime-tuning snippets in `docs/guide/getting-started.md` — was never
/// actually compiled by anything. `scripts/check-docs-macro-args.sh` names the
/// gap directly: "the markdown fences are not compiled by anything at all."
/// The docs corpus's other gates check that a fence's *names* resolve (a
/// link, a CLI flag, a config key, an import path); none of them catch a
/// signature that has drifted out from under a fence that still typechecks
/// against nothing, the way `inject_consent_banner`'s `csrf_cookie_name`
/// drifted from `&str` to `Option<&str>` underneath the CLI scaffold template
/// (#2459, #2620) — a widened-parameter class of break a string-equality
/// check can never see.
///
/// Some authors already reached for rustdoc's own "compiles, don't execute"
/// convention by hand on these fences (`rust,no_run`) even though nothing
/// here has ever enforced it — a `#[autumn_web::main]` snippet binds a real
/// port and blocks forever, exactly what `no_run` exists to avoid running.
/// That rules out `trybuild::TestCases::pass`, which this module uses
/// everywhere else: a `pass` fixture is a real compiled binary that trybuild
/// then *executes* to check its exit status, so pointing it at one of these
/// snippets would compile clean and then hang the test suite on the running
/// server, forever, on purpose.
///
/// Also unlike every other fixture in this file, not every `no_run` fence is
/// a complete program: one (the CSRF form handler) is a bare `async fn`, the
/// same "elide the surrounding main for brevity" shape rustdoc itself accepts
/// because it wraps a mainless doctest in one automatically. Nothing here
/// does that wrapping, so a fence containing `fn main` becomes its own
/// `src/bin/*.rs` — the same binary-crate shape a reader actually pastes a
/// complete example into — and every other fence is nested in its own `mod`
/// of one shared `src/lib.rs`, where a bare function is legal without one.
/// The split matters, not just the bookkeeping: a binary's `fn main` obeys
/// real entry-point rules a library `mod` does not enforce — an `async fn
/// main` with no `#[autumn_web::main]` is `E0752` at the crate root of a
/// binary, but merely an ordinary (unremarkable, silently-compiling) async
/// function nested in a module (Codex review, PR #2707, round 3). Treating
/// every fence as a library item would have let that exact class of typo
/// through the gate uncaught.
///
/// Extracts every `rust,no_run` fence from the two files this way and
/// `cargo check`s the result — real compilation against the in-tree crate,
/// with no linked binary ever run. A new `no_run` fence in either file is
/// picked up automatically; nothing here needs updating when one is added,
/// renamed, or removed — only a fence that drops the tag silently opts back
/// out, which is the same trust the tag already carries for rustdoc itself.
#[test]
fn doc_getting_started_snippets_compile() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");

    let mut fixtures = Vec::new();
    for (doc, slug) in [
        ("README.md", "readme"),
        ("docs/guide/getting-started.md", "getting_started"),
    ] {
        let path = root.join(doc);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        for (start_line, body) in extract_rust_no_run_fences(&text) {
            fixtures.push((format!("{slug}_l{start_line}"), body));
        }
    }

    // A parser regression that silently matched nothing would make this test
    // trivially green without compiling a single snippet. 5 is today's known
    // count (README's one `## Example`, the CSRF form handler, and three
    // runtime-tuning snippets, all in the guide) — the assertion only guards
    // against the extractor going quietly blind, not against that count
    // changing.
    assert!(
        fixtures.len() >= 5,
        "expected at least 5 `rust,no_run` fences across README.md and \
         docs/guide/getting-started.md, found {}. Either a fence lost its \
         `no_run` tag or extract_rust_no_run_fences is broken.",
        fixtures.len()
    );

    // A throwaway crate, checked once, under this checkout's own (gitignored)
    // `target/` rather than the system-wide temp dir — two checkouts (or two
    // users) sharing `/tmp` would otherwise race to write the same
    // `Cargo.toml`/`src/lib.rs` and could compile one checkout's snippets
    // against another's `autumn-web` path (Codex review, PR #2707). Suffixed
    // with this process's PID so two invocations of the same checkout (an
    // IDE test run overlapping a pre-push check) get separate directories
    // too, rather than one's `RemoveDirOnDrop` deleting files the other is
    // still using (Codex review, round 2). Its own `[workspace]` keeps it
    // from being folded into this checkout's real workspace despite living
    // under `target/`. Removed on the way out, pass or fail, so a later run
    // never inherits a stale lockfile or build artifact from this one.
    let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/doc-snippet-check")
        .join(std::process::id().to_string());
    let _cleanup = RemoveDirOnDrop(scratch.clone());
    let src_bin = scratch.join("src/bin");
    std::fs::create_dir_all(&src_bin).expect("scratch crate directories");

    // Dependencies mirror what `autumn new` actually writes
    // (`autumn-cli/src/templates/Cargo.toml.tmpl`), not a guess at what a
    // fence might need: a generated app has no *direct* `axum` dependency
    // (Codex review, PR #2707) — one snippet used bare `axum::Router` and
    // only compiled here because an earlier version of this scratch crate
    // added `axum` itself, masking that the snippet would fail an
    // undeclared-crate error in a real `autumn new` project. Fixed at the
    // doc layer (that snippet now goes through the same
    // `autumn_web::reexports::axum` a real user would have to) rather than
    // by widening this manifest to match. `maud`'s `html!` macro expands to
    // code that names the `maud` crate directly, so a generated app depends
    // on it too, and a fence exercising it needs the same direct dependency
    // here.
    //
    // A name-only check couldn't have caught the template changing a
    // dependency's *version or features* — e.g. `maud` dropping the `axum`
    // feature would still pass a `contains("maud")` check while a fence
    // exercising that feature kept compiling here against a stale spec
    // hand-typed below (Codex review, PR #2707, round 4). Extracting each
    // dependency's exact declaration line and using it verbatim removes the
    // hand-typed copy entirely, so there is nothing left to drift.
    let template_manifest =
        std::fs::read_to_string(root.join("autumn-cli/src/templates/Cargo.toml.tmpl"))
            .expect("read autumn-cli/src/templates/Cargo.toml.tmpl");
    let template_dep_line = |name: &str| -> String {
        template_manifest
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{name} =")))
            .unwrap_or_else(|| {
                panic!(
                    "autumn-cli/src/templates/Cargo.toml.tmpl has no `{name} = ...` \
                     dependency line for doc_getting_started_snippets_compile to mirror \
                     — update it to match what `autumn new` actually writes."
                )
            })
            .trim()
            .to_string()
    };
    let maud_dep = template_dep_line("maud");
    let diesel_migrations_dep = template_dep_line("diesel_migrations");
    // `autumn-web` itself is special-cased below to a local path dependency
    // instead of the templated `{{autumn_version}}` placeholder (this test
    // checks against the in-tree crate, not a published version) — the
    // template's own line carries no features/extra detail beyond that
    // placeholder, so a presence check is enough for it alone. Must still go
    // through the same active-line test `template_dep_line` uses rather than
    // a plain substring search: the template also carries a *commented*
    // example (`# autumn-web = { ..., features = [...] }`) right above the
    // real line, which a bare `.contains("autumn-web")` would match even if
    // the real line were removed or commented out too (Codex review, PR
    // #2707).
    assert!(
        template_manifest
            .lines()
            .any(|line| line.trim_start().starts_with("autumn-web =")),
        "autumn-cli/src/templates/Cargo.toml.tmpl no longer declares an active `autumn-web` dependency"
    );

    let autumn_web_path = root.join("autumn");
    std::fs::write(
        scratch.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"autumn-doc-snippet-check\"\n\
             version = \"0.0.0\"\n\
             edition = \"2024\"\n\
             publish = false\n\
             \n\
             [dependencies]\n\
             autumn-web = {{ path = {autumn_web_path:?} }}\n\
             {maud_dep}\n\
             {diesel_migrations_dep}\n\
             \n\
             [workspace]\n"
        ),
    )
    .expect("write scratch Cargo.toml");

    // Fences containing `fn main` are complete programs — the same shape a
    // reader pastes into `src/main.rs` — so each becomes its own binary
    // target, where rustc enforces the real entry-point rules (`E0752` on an
    // async `main` missing `#[autumn_web::main]`, etc.). Everything else is a
    // bare-item fragment nested in its own `mod` of one shared `src/lib.rs`,
    // where no `main` is required (Codex review, PR #2707, round 3).
    // `declares_fn_main` tolerates whitespace between `main` and `(` (`fn
    // main ()`, a line break) rather than requiring the literal `fn main(`
    // spelling: a fence in that shape was falling through to the library
    // path, where the exact defect this split exists to catch — a dropped
    // `#[autumn_web::main]` on an async `main` — silently compiles instead
    // of hitting the binary-only `E0752` (Codex review, round 6).
    let mut lib_rs = String::from("#![allow(dead_code, unused_variables, unused_imports)]\n\n");
    for (name, body) in &fixtures {
        if declares_fn_main(body) {
            std::fs::write(src_bin.join(format!("{name}.rs")), body)
                .unwrap_or_else(|e| panic!("write scratch src/bin/{name}.rs: {e}"));
        } else {
            use std::fmt::Write as _;
            let _ = write!(lib_rs, "mod {name} {{\n{body}\n}}\n\n");
        }
    }
    std::fs::write(scratch.join("src/lib.rs"), lib_rs).expect("write scratch src/lib.rs");

    // Seed the scratch crate's lockfile from the real workspace's (Codex
    // review, round 2): with no `Cargo.lock` of its own, `cargo check` has to
    // resolve autumn-web's entire dependency graph from scratch, and without
    // `--offline` that means an index/registry round trip for every
    // transitive crate — reproduced hanging on an `aes-gcm` fetch — even
    // though every one of those versions was already downloaded and built
    // moments earlier compiling `autumn-web` itself for the outer test
    // binary. Copying the workspace's lock in (not asserted immutable via
    // `--locked`, since this crate's own root package has no entry in it)
    // gives the resolver every version it needs already pinned; the only
    // node left to add is this crate itself, which introduces no new
    // external requirement, so `--offline` never has to leave the local
    // cache.
    std::fs::copy(root.join("Cargo.lock"), scratch.join("Cargo.lock"))
        .expect("seed the scratch crate's Cargo.lock from the workspace's");

    // Point at the real workspace's own target dir rather than a fresh one
    // under `scratch` (Codex review, PR #2707, round 4): every dependency
    // this crate needs was just compiled there for the outer test binary, at
    // the same locked versions this crate's seeded lockfile now shares, so
    // cargo reuses those artifacts instead of rebuilding the entire
    // `autumn-web` dependency graph a second time — this repo's CI already
    // runs close to its disk ceiling, and a silent full second build would
    // cost real minutes on every run. `resolve_cargo_target_dir` asks cargo
    // itself rather than assuming `root.join("target")`, since a
    // `CARGO_TARGET_DIR` env var, a `.cargo/config.toml` `build.target-dir`,
    // or the outer invocation's own `--target-dir` would each relocate it
    // (Codex review, round 5). Left in place afterward (unlike `scratch`
    // itself): it's cargo's own build output directory, already shared and
    // already excluded from version control by definition.
    // `--profile test`, not the default `dev` (Codex review, round 12): the
    // workspace root overrides `[profile.test]`'s debug/incremental
    // settings, and cargo's fingerprint bakes in the resolved profile, so a
    // `dev` check can't reuse the `test`-profile artifacts already here.
    let output = std::process::Command::new(env!("CARGO"))
        .args(["check", "--offline", "--profile", "test", "--target-dir"])
        .arg(resolve_cargo_target_dir())
        .current_dir(&scratch)
        .output()
        .expect("failed to run cargo check on the extracted doc snippets");

    assert!(
        output.status.success(),
        "one or more `rust,no_run` snippets in README.md / \
         docs/guide/getting-started.md no longer compile against the in-tree \
         autumn-web crate:\n\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Whether a fence's body declares a `main` function, tolerating whitespace
/// (including a line break) between `main` and `(` — `fn main ()`, not just
/// `fn main()` — since a plain `body.contains("fn main(")` fell through to
/// the library path on that spelling, the same silent-miss this whole split
/// exists to prevent (Codex review, PR #2707, round 6).
fn declares_fn_main(body: &str) -> bool {
    let mut search_from = 0;
    while let Some(offset) = body[search_from..].find("fn main") {
        let after = search_from + offset + "fn main".len();
        if body[after..].trim_start().starts_with('(') {
            return true;
        }
        search_from = after;
    }
    false
}

/// Finds the workspace's actual build output directory the same way cargo
/// itself already answered it for *this* process, rather than re-deriving it
/// from a fresh `cargo metadata` call: a `CARGO_TARGET_DIR` env var and a
/// `.cargo/config.toml` `build.target-dir` both propagate to a spawned
/// subprocess and so are visible to `cargo metadata` too, but the outer
/// invocation's own one-shot `--target-dir` CLI flag is not — it applies
/// only to that single command, so a child `cargo metadata` run resolves the
/// *default* location instead and reuse silently stops working for exactly
/// the supported case round 5 meant to cover (Codex review, round 6, with a
/// reproduction: a probed child process saw `CARGO_TARGET_DIR=None` and
/// still reported the default). The one thing that cannot lie about where
/// cargo actually put this build's artifacts is where it put *this test
/// binary*: `target-dir/<profile>/deps/<this binary>`, three path
/// components down from `current_exe()`, however that target-dir was
/// chosen.
///
/// A `cargo test --target <triple>` build inserts one more directory —
/// `target-dir/<triple>/<profile>/deps/<binary>` — so the same three-level
/// walk lands one level short, at `target-dir/<triple>` instead of
/// `target-dir` itself (Codex review, round 7; this repo's own CI never
/// passes `--target` today, so it doesn't hit this, but the function's own
/// doc comment claimed to handle "however that target-dir was chosen" and
/// didn't). Rather than sniff a target triple out of a path component —
/// nothing distinguishes one from a custom profile name by shape alone —
/// this checks for `CACHEDIR.TAG`, which cargo unconditionally writes in the
/// real target-dir root (a stable, documented marker other tools already
/// rely on to know a directory is cache-like); its absence means the walk
/// landed one level short, so go up once more. Deliberately doesn't try to
/// detect or propagate the triple into the nested `cargo check` itself: this
/// gate exists to prove a doc snippet compiles the way a reader's own
/// machine would build it, which is a host-target question regardless of
/// what target the outer test suite happens to be exercising elsewhere.
fn resolve_cargo_target_dir() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("resolve the running test binary's own path");
    let mut dir = exe
        .ancestors()
        .nth(3)
        .unwrap_or_else(|| {
            panic!(
                "test binary path {} is not nested as target-dir/<profile>/deps/<binary>",
                exe.display()
            )
        })
        .to_path_buf();
    if !dir.join("CACHEDIR.TAG").exists()
        && let Some(parent) = dir.parent()
    {
        dir = parent.to_path_buf();
    }
    // A `--target <triple>` build inserts an extra `<target-dir>/<triple>/`
    // level that also carries CACHEDIR.TAG (confirmed by probing `cargo
    // build --target <host-triple>`, which stamped the marker in both
    // `target/` and `target/<triple>/`) — so "parent also has CACHEDIR.TAG"
    // can't tell that inserted level apart from a deliberately nested
    // `--target-dir` (e.g. `--target-dir target/doc-tests`, itself sitting
    // under a marked `target/`), which must NOT be climbed past. Resolve it
    // by asking rustc for its own list of valid target triples: only an
    // actual `--target` subdirectory can be named one.
    //
    // Known, accepted limitation: an operator could name a *custom*
    // `--target-dir` (without ever passing `--target`) after a real triple,
    // e.g. `--target-dir /cache/x86_64-unknown-linux-gnu`. That directory is
    // then byte-for-byte indistinguishable on disk from cargo's own
    // `--target x86_64-unknown-linux-gnu` insertion — same name, same
    // CACHEDIR.TAG placement — so no purely filesystem-based check (this one
    // included) can tell the two apart; only a build-time capture of the
    // real `TARGET` env var via a build script could, at the cost of adding
    // one to this whole library crate for a case no CI job here exercises.
    // We resolve the ambiguity in cargo's favor, since a triple-named
    // `--target` insertion is the far more common real-world source of it.
    if let Some(name) = dir.file_name().and_then(std::ffi::OsStr::to_str)
        && is_rustc_target_triple(name)
        && let Some(parent) = dir.parent()
    {
        dir = parent.to_path_buf();
    }
    dir
}

/// Whether `name` is one of rustc's own recognized `--target` triples,
/// distinguishing a `--target <triple>`-inserted artifact directory from an
/// arbitrary user-chosen `--target-dir` path component of the same shape.
fn is_rustc_target_triple(name: &str) -> bool {
    let Ok(output) = std::process::Command::new("rustc")
        .args(["--print", "target-list"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == name)
}

/// Recursively removes the wrapped directory when dropped, pass or fail
/// (including on an `assert!` panic) — used to keep `doc_getting_started_
/// snippets_compile`'s scratch crate from surviving its own test run.
struct RemoveDirOnDrop(std::path::PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pulls the body of every ` ```rust,no_run ` fence out of a markdown
/// document, paired with the 1-indexed source line its body starts on (used
/// only to name the extracted fixture — a compile failure is reported against
/// that copy, not the original file, so tracking it down still means
/// `grep -n 'rust,no_run'` in the doc).
fn extract_rust_no_run_fences(markdown: &str) -> Vec<(usize, String)> {
    let mut fences = Vec::new();
    let mut lines = markdown.lines().enumerate();
    while let Some((i, line)) = lines.next() {
        if line.trim() != "```rust,no_run" {
            continue;
        }
        let start_line = i + 2;
        let mut body = String::new();
        for (_, body_line) in lines.by_ref() {
            if body_line.trim() == "```" {
                break;
            }
            body.push_str(body_line);
            body.push('\n');
        }
        fences.push((start_line, body));
    }
    fences
}
