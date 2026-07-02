//! Issue #835 / PR review: `preload` must apply the association target's own
//! read scoping (tenant isolation + soft-delete), mirroring what the target's
//! repository finders hide — and *only* when the target's repository actually
//! opts into that scoping.
//!
//! Scoping is keyed off the target repository's `#[repository(..., soft_delete,
//! tenant_scoped)]` config (surfaced to the `#[model]`-generated
//! `__autumn_preload_retain` via the `AutumnPreloadScopeExt` inherent-override
//! pattern), **not** off field presence: a model may carry a `deleted_at` /
//! `tenant_id` column (audit history, denormalized tenant) without its
//! repository scoping on it, and then finders — and preload — leave it
//! unfiltered. A tenant-scoped target with no tenant context **fails closed**
//! exactly like its finders. `across_tenants()` is honored via the ambient
//! `PRELOAD_ACROSS_TENANTS` task-local that a repository's `preload` publishes.
//!
//! These tests exercise the retain in-memory — no database required — by
//! calling the generated helper directly on hand-built rows.

#![cfg(feature = "db")]

use autumn_web::preload::PRELOAD_ACROSS_TENANTS;
use autumn_web::tenancy::CURRENT_TENANT;

mod schema {
    autumn_web::reexports::diesel::table! {
        scoped_items (id) {
            id -> Int8,
            name -> Text,
            tenant_id -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        // Same columns as `scoped_items`, but its repository does NOT opt into
        // soft_delete / tenant_scoped — so preload must leave it unfiltered.
        audit_items (id) {
            id -> Int8,
            name -> Text,
            tenant_id -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        // Soft-delete only (no tenant scoping): soft-delete applies regardless
        // of tenant context, and never fails closed.
        soft_items (id) {
            id -> Int8,
            name -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        plain_items (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema::{audit_items, plain_items, scoped_items, soft_items};

#[autumn_web::model(table = "scoped_items")]
pub struct ScopedItem {
    #[id]
    pub id: i64,
    pub name: String,
    pub tenant_id: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

// Repository opts into both scopes → preload scopes `ScopedItem`.
#[autumn_web::repository(ScopedItem, table = "scoped_items", soft_delete, tenant_scoped)]
pub trait ScopedItemRepository {}

#[autumn_web::model(table = "audit_items")]
pub struct AuditItem {
    #[id]
    pub id: i64,
    pub name: String,
    pub tenant_id: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

// Repository does NOT opt into soft_delete / tenant_scoped, even though the
// columns exist (e.g. `deleted_at` is audit history). Finders don't filter, so
// neither should preload.
#[autumn_web::repository(AuditItem, table = "audit_items")]
pub trait AuditItemRepository {}

#[autumn_web::model(table = "soft_items")]
pub struct SoftItem {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(SoftItem, table = "soft_items", soft_delete)]
pub trait SoftItemRepository {}

#[autumn_web::model(table = "plain_items")]
pub struct PlainItem {
    #[id]
    pub id: i64,
    pub name: String,
}

fn item(id: i64, tenant: &str, deleted: bool) -> ScopedItem {
    ScopedItem {
        id,
        name: format!("item-{id}"),
        tenant_id: tenant.to_string(),
        deleted_at: deleted.then(chrono::NaiveDateTime::default),
    }
}

fn audit(id: i64, tenant: &str, deleted: bool) -> AuditItem {
    AuditItem {
        id,
        name: format!("audit-{id}"),
        tenant_id: tenant.to_string(),
        deleted_at: deleted.then(chrono::NaiveDateTime::default),
    }
}

fn ids(rows: &[ScopedItem]) -> Vec<i64> {
    rows.iter().map(|r| r.id).collect()
}

#[tokio::test]
async fn retain_keeps_only_current_tenant_and_drops_soft_deleted() {
    let rows = vec![
        item(1, "acme", false),   // kept
        item(2, "globex", false), // other tenant → dropped
        item(3, "acme", true),    // soft-deleted → dropped
        item(4, "acme", false),   // kept
    ];

    let kept = CURRENT_TENANT
        .scope(Some("acme".to_string()), async move {
            ScopedItem::__autumn_preload_retain(rows)
        })
        .await
        .expect("tenant context present");

    assert_eq!(ids(&kept), vec![1, 4]);
}

#[tokio::test]
async fn retain_fails_closed_when_tenant_scoped_without_context() {
    // Tenant-scoped target, no CURRENT_TENANT, not across_tenants: must error
    // rather than attach cross-tenant rows — same as a tenant-scoped finder.
    let rows = vec![item(1, "acme", false), item(2, "globex", false)];
    let result = ScopedItem::__autumn_preload_retain(rows);
    let err = result.expect_err("must fail closed without tenant context");
    assert!(
        err.to_string().to_lowercase().contains("tenant"),
        "error should mention tenant context, got: {err}"
    );
}

#[tokio::test]
async fn retain_fails_closed_on_empty_rows_without_tenant_context() {
    // Regression guard for the many-to-many (`through =`) fail-closed parity
    // probe: `through` preload loaders call `__autumn_preload_retain` with an
    // *empty* `Vec` specifically to surface this error even when their join
    // returns zero rows (which would otherwise mean the per-row
    // `__autumn_preload_keep` loop never runs, and the fail-closed check
    // never fires). `retain` must not special-case an empty input and skip
    // the tenant check ahead of it.
    let result = ScopedItem::__autumn_preload_retain(Vec::new());
    let err = result.expect_err("must fail closed without tenant context, even for zero rows");
    assert!(
        err.to_string().to_lowercase().contains("tenant"),
        "error should mention tenant context, got: {err}"
    );
}

#[tokio::test]
async fn retain_isolates_each_tenant() {
    let make = || {
        vec![
            item(1, "acme", false),
            item(2, "globex", false),
            item(3, "acme", false),
        ]
    };

    let acme = CURRENT_TENANT
        .scope(Some("acme".to_string()), async {
            ScopedItem::__autumn_preload_retain(make())
        })
        .await
        .expect("tenant context present");
    assert_eq!(ids(&acme), vec![1, 3]);

    let globex = CURRENT_TENANT
        .scope(Some("globex".to_string()), async {
            ScopedItem::__autumn_preload_retain(make())
        })
        .await
        .expect("tenant context present");
    assert_eq!(ids(&globex), vec![2]);
}

#[tokio::test]
async fn across_tenants_skips_tenant_filter_but_keeps_soft_delete() {
    // Under `across_tenants()` (ambient flag set by the repository's preload),
    // the tenant predicate is skipped at every level — but soft-delete still
    // applies, exactly like an `across_tenants()` finder.
    let rows = vec![
        item(1, "acme", false),   // kept (tenant filter skipped)
        item(2, "globex", false), // kept (tenant filter skipped)
        item(3, "acme", true),    // soft-deleted → dropped
    ];

    let kept = CURRENT_TENANT
        .scope(Some("acme".to_string()), async {
            PRELOAD_ACROSS_TENANTS
                .scope(true, async { ScopedItem::__autumn_preload_retain(rows) })
                .await
        })
        .await
        .expect("across_tenants never fails closed");

    assert_eq!(ids(&kept), vec![1, 2]);
}

#[test]
fn soft_delete_only_drops_deleted_without_tenant_context() {
    // `soft_delete` (not `tenant_scoped`): soft-deleted rows are hidden with no
    // tenant context, and there is no fail-closed.
    let rows = vec![
        SoftItem {
            id: 1,
            name: "a".into(),
            deleted_at: None,
        },
        SoftItem {
            id: 2,
            name: "b".into(),
            deleted_at: Some(chrono::NaiveDateTime::default()),
        },
        SoftItem {
            id: 3,
            name: "c".into(),
            deleted_at: None,
        },
    ];
    let kept = SoftItem::__autumn_preload_retain(rows).expect("soft-only never fails closed");
    assert_eq!(kept.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 3]);
}

#[tokio::test]
async fn retain_does_not_scope_when_repository_opts_out() {
    // `AuditItem` has `deleted_at` + `tenant_id` columns, but its repository is
    // not `soft_delete` / `tenant_scoped`. Finders leave these rows unfiltered,
    // so preload must too — even with a tenant in context and soft-deleted rows
    // present, and it must not fail closed. (Regression for the field-presence
    // bug.)
    let rows = vec![
        audit(1, "acme", false),
        audit(2, "globex", false), // other tenant → still kept
        audit(3, "acme", true),    // "deleted" audit row → still kept
    ];

    let kept = CURRENT_TENANT
        .scope(Some("acme".to_string()), async {
            AuditItem::__autumn_preload_retain(rows)
        })
        .await
        .expect("opted-out target never fails closed");

    assert_eq!(kept.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn retain_is_identity_for_models_without_scoping_columns() {
    let rows = vec![
        PlainItem {
            id: 1,
            name: "a".into(),
        },
        PlainItem {
            id: 2,
            name: "b".into(),
        },
    ];
    let kept = PlainItem::__autumn_preload_retain(rows).expect("identity");
    assert_eq!(kept.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2]);
}

// ── `__autumn_preload_keep` (per-row sibling of `__autumn_preload_retain`,
// used by many-to-many `through =` preload loaders which pair each child row
// with its parent key and so can't batch-`retain` a `Vec<Self>`) — #1324 ────

#[tokio::test]
async fn keep_matches_retain_for_scoped_rows() {
    let rows = vec![
        item(1, "acme", false),   // kept
        item(2, "globex", false), // other tenant → dropped
        item(3, "acme", true),    // soft-deleted → dropped
        item(4, "acme", false),   // kept
    ];

    let kept_ids = CURRENT_TENANT
        .scope(Some("acme".to_string()), async {
            let mut kept_ids = Vec::new();
            for row in &rows {
                if ScopedItem::__autumn_preload_keep(row)? {
                    kept_ids.push(row.id);
                }
            }
            autumn_web::AutumnResult::Ok(kept_ids)
        })
        .await
        .expect("tenant context present");

    assert_eq!(kept_ids, vec![1, 4]);
}

#[tokio::test]
async fn keep_fails_closed_without_tenant_context_per_row() {
    let row = item(1, "acme", false);
    let err = ScopedItem::__autumn_preload_keep(&row)
        .expect_err("must fail closed without tenant context");
    assert!(
        err.to_string().to_lowercase().contains("tenant"),
        "error should mention tenant context, got: {err}"
    );
}

#[tokio::test]
async fn keep_identity_for_leaf_models() {
    // A hand-written `Preloadable` target (`impl_preloadable_leaf!`) applies
    // no scoping, mirroring its batch `__autumn_preload_retain`.
    struct LeafItem {
        #[allow(dead_code)]
        id: i64,
    }
    autumn_web::impl_preloadable_leaf!(LeafItem);

    let row = LeafItem { id: 1 };
    assert!(LeafItem::__autumn_preload_keep(&row).expect("identity never fails"));
}
