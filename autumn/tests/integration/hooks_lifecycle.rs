//! Integration tests for mutation hook lifecycle types.

use autumn_web::hooks::*;

#[test]
fn mutation_context_has_uuid_request_id() {
    let ctx = MutationContext::new(MutationOp::Create);
    let rid = ctx.request_id.as_ref().expect("should have request_id");
    assert_eq!(rid.len(), 36); // UUID v4 format
    assert_eq!(&rid[8..9], "-");
}

#[test]
fn mutation_op_roundtrip() {
    for op in [MutationOp::Create, MutationOp::Update, MutationOp::Delete] {
        let s = op.as_str();
        assert!(!s.is_empty());
        assert_eq!(format!("{op}"), s);
    }
}

#[test]
fn field_diff_set_rewrite() {
    let mut diff = FieldDiff::new("old".to_string(), "old".to_string());
    assert!(diff.unchanged());
    diff.set("new".to_string());
    assert!(diff.changed());
    assert!(diff.changed_from(&"old".to_string()));
    assert!(diff.changed_to(&"new".to_string()));
}

#[test]
fn patch_default_is_unchanged() {
    let p: Patch<String> = Patch::default();
    assert!(p.is_unchanged());
    assert_eq!(p.into_option(), None);
}

#[test]
fn patch_tristate_coverage() {
    // Set
    let s = Patch::Set(42);
    assert!(s.is_set());
    assert_eq!(s.as_set(), Some(&42));
    assert_eq!(s.into_option(), Some(Some(42)));

    // Clear
    let c: Patch<i32> = Patch::Clear;
    assert!(c.is_clear());
    assert_eq!(c.into_option(), Some(None));

    // Unchanged
    let u: Patch<i32> = Patch::Unchanged;
    assert!(u.is_unchanged());
    assert_eq!(u.into_option(), None);
}

#[tokio::test]
async fn no_hooks_methods_are_all_ok() {
    use autumn_web::hooks::UpdateDraft;

    let hooks: NoHooks<String, String, String> = NoHooks::default();
    let mut ctx = MutationContext::new(MutationOp::Create);
    let mut new = String::new();
    let model = String::new();
    let mut draft = UpdateDraft::new(String::new());

    assert!(hooks.before_create(&mut ctx, &mut new).await.is_ok());
    assert!(hooks.before_update(&mut ctx, &mut draft).await.is_ok());
    assert!(hooks.before_delete(&mut ctx, &model).await.is_ok());
}
