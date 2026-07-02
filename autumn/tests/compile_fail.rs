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

    // Static route macro failures
    t.compile_fail("tests/compile-fail/static_get_path_params.rs");
    t.compile_fail("tests/compile-fail/static_get_non_async.rs");
    t.compile_fail("tests/compile-fail/static_get_params_no_placeholders.rs");

    // Model macro failures (require db feature)
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_on_enum.rs");
    #[cfg(feature = "db")]
    t.compile_fail("tests/compile-fail/model_shard_key_unknown.rs");

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

    // Maud + form/json handlers (require maud feature)
    #[cfg(feature = "maud")]
    t.pass("tests/compile-pass/json_form_handlers.rs");

    // Model derive (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_derive.rs");

    // Model field enum (requires db feature)
    #[cfg(feature = "db")]
    t.pass("tests/compile-pass/model_field_enum.rs");

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
