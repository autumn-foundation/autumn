#![cfg(feature = "db")]

use autumn_web::tenancy::with_tenant;
use diesel_async::RunQueryDsl;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

mod schema {
    autumn_web::reexports::diesel::table! {
        tenant_posts (id) {
            id -> Int8,
            title -> Text,
            tenant_id -> Text,
        }
    }
}

use schema::tenant_posts;

#[autumn_web::model(table = "tenant_posts")]
pub struct TenantPost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(TenantPost, table = "tenant_posts", tenant_scoped)]
pub trait TenantPostRepository {
    fn find_by_title(title: String) -> Vec<TenantPost>;
}

mod manual_schema {
    ::autumn_web::reexports::diesel::table! {
        manual_tenant_posts (id) {
            id -> Int8,
            title -> Text,
            tenant_id -> Text,
        }
    }
}

use manual_schema::manual_tenant_posts;

#[autumn_web::model(table = "manual_tenant_posts")]
pub struct ManualTenantPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub tenant_id: String,
}

#[autumn_web::repository(ManualTenantPost, table = "manual_tenant_posts", tenant_scoped)]
pub trait ManualTenantPostRepository {}

#[test]
fn test_manual_tenant_id_insertable() {
    use autumn_web::tenancy::TenantInsertable;
    let post = NewManualTenantPost {
        title: "Hello".to_string(),
        tenant_id: String::new(),
    };
    let with_tenant = post.tenant_values("my-tenant");
    assert_eq!(with_tenant.tenant_id, "my-tenant");
}

// Helper to set up the DB table.
async fn setup_db(
    pool: &autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
        autumn_web::reexports::diesel_async::AsyncPgConnection,
    >,
) {
    let mut conn = pool.get().await.unwrap();
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS tenant_posts (
            id BIGSERIAL PRIMARY KEY,
            title TEXT NOT NULL,
            tenant_id TEXT NOT NULL
        )",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    diesel::sql_query("TRUNCATE tenant_posts RESTART IDENTITY")
        .execute(&mut *conn)
        .await
        .unwrap();
}

// 1. Test standard/derived CRUD operations under a specific tenant context
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_tenant_scoping_isolation() {
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
        diesel_async::AsyncPgConnection,
    >::new(&url);
    let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
        .max_size(5)
        .build()
        .unwrap();

    setup_db(&pool).await;

    let repo = PgTenantPostRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: ::std::time::Duration::from_millis(100),
        __autumn_route: ::core::option::Option::None,
    };

    // Save record for tenant A
    let post_a = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewTenantPost {
            title: "Post A".to_string(),
        })
        .await
        .unwrap()
    })
    .await;
    assert_eq!(post_a.tenant_id, "tenant-a");

    // Save record for tenant B
    let post_b = with_tenant("tenant-b".to_string(), async {
        repo.save(&NewTenantPost {
            title: "Post B".to_string(),
        })
        .await
        .unwrap()
    })
    .await;
    assert_eq!(post_b.tenant_id, "tenant-b");

    // Assert tenant A can only read tenant A's post
    with_tenant("tenant-a".to_string(), async {
        let all = repo.find_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Post A");

        let found = repo.find_by_id(post_a.id).await.unwrap().unwrap();
        assert_eq!(found.title, "Post A");

        let found_by_title = repo.find_by_title("Post A".to_string()).await.unwrap();
        assert_eq!(found_by_title.len(), 1);

        let not_found = repo.find_by_id(post_b.id).await.unwrap();
        assert!(not_found.is_none());

        let exists = repo.exists_by_id(post_a.id).await.unwrap();
        assert!(exists);
        let exists_b = repo.exists_by_id(post_b.id).await.unwrap();
        assert!(!exists_b);
    })
    .await;
}

// 2. Test that calling repository methods without a tenant context throws an error
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_unscoped_query_without_context_fails() {
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
        diesel_async::AsyncPgConnection,
    >::new(&url);
    let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
        .max_size(5)
        .build()
        .unwrap();

    setup_db(&pool).await;
    let repo = PgTenantPostRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: ::std::time::Duration::from_millis(100),
        __autumn_route: ::core::option::Option::None,
    };

    // Scoped methods should fail when run unscoped without context
    let result = repo.find_all().await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("no tenant context was established"),
        "Expected tenant context error, got: {err}"
    );
}

// 3. Test that the across_tenants() escape hatch works and bypasses scoping
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_escape_hatch_across_tenants() {
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
        diesel_async::AsyncPgConnection,
    >::new(&url);
    let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
        .max_size(5)
        .build()
        .unwrap();

    setup_db(&pool).await;
    let repo = PgTenantPostRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: ::std::time::Duration::from_millis(100),
        __autumn_route: ::core::option::Option::None,
    };

    with_tenant("tenant-a".to_string(), async {
        repo.save(&NewTenantPost {
            title: "Post A".to_string(),
        })
        .await
        .unwrap();
    })
    .await;
    with_tenant("tenant-b".to_string(), async {
        repo.save(&NewTenantPost {
            title: "Post B".to_string(),
        })
        .await
        .unwrap();
    })
    .await;

    // Now read across all tenants
    let all = repo.across_tenants().find_all().await.unwrap();
    assert_eq!(all.len(), 2);
}

// 4. Test that client attempts to update tenant_id in scoped updates are overridden/blocked
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_immutable_tenant_id_on_update() {
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
        diesel_async::AsyncPgConnection,
    >::new(&url);
    let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
        .max_size(5)
        .build()
        .unwrap();

    setup_db(&pool).await;

    let repo = PgTenantPostRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: ::std::time::Duration::from_millis(100),
        __autumn_route: ::core::option::Option::None,
    };

    // Save record for tenant A
    let post_a = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewTenantPost {
            title: "Post A".to_string(),
        })
        .await
        .unwrap()
    })
    .await;
    assert_eq!(post_a.tenant_id, "tenant-a");

    // Try to update post_a's title
    let updated = with_tenant("tenant-a".to_string(), async {
        let changes = UpdateTenantPost {
            title: ::autumn_web::hooks::Patch::Set("Post A Updated".to_string()),
        };
        repo.update(post_a.id, &changes).await.unwrap()
    })
    .await;

    // The returned record should still have tenant_id "tenant-a", not "tenant-b"
    assert_eq!(updated.tenant_id, "tenant-a");
    assert_eq!(updated.title, "Post A Updated");

    // Assert that the record still belongs to tenant-a, not tenant-b
    with_tenant("tenant-b".to_string(), async {
        let not_found = repo.find_by_id(post_a.id).await.unwrap();
        assert!(not_found.is_none());
    })
    .await;

    with_tenant("tenant-a".to_string(), async {
        let found = repo.find_by_id(post_a.id).await.unwrap().unwrap();
        assert_eq!(found.tenant_id, "tenant-a");
        assert_eq!(found.title, "Post A Updated");
    })
    .await;
}

// 5. Test that across_tenants().save(...) works with a framework-managed tenant_id (omitted from New*)
// when a CURRENT_TENANT is established, resolving and inserting the correct tenant_id.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_across_tenants_save_without_tenant_id_on_new_struct_works() {
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
        diesel_async::AsyncPgConnection,
    >::new(&url);
    let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
        .max_size(5)
        .build()
        .unwrap();

    setup_db(&pool).await;

    let repo = PgTenantPostRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: ::std::time::Duration::from_millis(100),
        __autumn_route: ::core::option::Option::None,
    };

    // Under `across_tenants()`, we can save a `NewTenantPost` (which does NOT have a tenant_id field)
    // if a CURRENT_TENANT is established, it should correctly resolve and use that tenant_id.
    let saved = with_tenant("tenant-c".to_string(), async {
        repo.across_tenants()
            .save(&NewTenantPost {
                title: "Across Tenant Save".to_string(),
            })
            .await
            .unwrap()
    })
    .await;

    assert_eq!(saved.title, "Across Tenant Save");
    assert_eq!(saved.tenant_id, "tenant-c");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines, clippy::similar_names)]
async fn test_bulk_ops_tenant_scoping() {
    let container = testcontainers_modules::postgres::Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
        diesel_async::AsyncPgConnection,
    >::new(&url);
    let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
        .max_size(5)
        .build()
        .unwrap();

    setup_db(&pool).await;

    let repo = PgTenantPostRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: ::std::time::Duration::from_millis(100),
        __autumn_route: ::core::option::Option::None,
    };

    // 1. save_many under tenant-a context
    let posts_a = with_tenant("tenant-a".to_string(), async {
        let new_posts = vec![
            NewTenantPost {
                title: "Post A1".to_string(),
            },
            NewTenantPost {
                title: "Post A2".to_string(),
            },
        ];
        repo.save_many(&new_posts).await.unwrap()
    })
    .await;
    assert_eq!(posts_a.len(), 2);
    assert_eq!(posts_a[0].tenant_id, "tenant-a");
    assert_eq!(posts_a[1].tenant_id, "tenant-a");

    // 2. save_many under tenant-b context
    let posts_b = with_tenant("tenant-b".to_string(), async {
        let new_posts = vec![
            NewTenantPost {
                title: "Post B1".to_string(),
            },
            NewTenantPost {
                title: "Post B2".to_string(),
            },
        ];
        repo.save_many(&new_posts).await.unwrap()
    })
    .await;
    assert_eq!(posts_b.len(), 2);
    assert_eq!(posts_b[0].tenant_id, "tenant-b");
    assert_eq!(posts_b[1].tenant_id, "tenant-b");

    // 3. update_many under tenant-a context
    // We try to update both tenant-a and tenant-b posts, but only tenant-a posts should change!
    let all_ids = vec![posts_a[0].id, posts_b[0].id];
    let changes = UpdateTenantPost {
        title: autumn_web::hooks::Patch::Set("Scoped Update".to_string()),
    };

    let updated = with_tenant("tenant-a".to_string(), async {
        repo.update_many(&all_ids, &changes).await.unwrap()
    })
    .await;
    // PgTenantPostRepository update_many returns the records updated. Only post A1 should be returned!
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].id, posts_a[0].id);
    assert_eq!(updated[0].title, "Scoped Update");

    // 4. Verify DB state using across_tenants()
    let all_posts = repo.across_tenants().find_all().await.unwrap();
    assert_eq!(all_posts.len(), 4);

    let post_a1 = all_posts.iter().find(|p| p.id == posts_a[0].id).unwrap();
    assert_eq!(post_a1.title, "Scoped Update");
    assert_eq!(post_a1.tenant_id, "tenant-a");

    let post_b1 = all_posts.iter().find(|p| p.id == posts_b[0].id).unwrap();
    assert_eq!(post_b1.title, "Post B1"); // Remains unchanged because it's tenant-b!
    assert_eq!(post_b1.tenant_id, "tenant-b");

    // 5. upsert_many under tenant-a context
    // We try to upsert:
    // - One existing tenant-a post (update its title)
    // - One new post (gets created under tenant-a)
    // - One existing tenant-b post (which should be blocked/ignored)
    let upsert_records = vec![
        TenantPost {
            id: posts_a[1].id,
            title: "Upserted A2".to_string(),
            tenant_id: "tenant-a".to_string(),
        },
        TenantPost {
            id: posts_b[1].id, // Try to hijack/update tenant-b post
            title: "Hijacked B2".to_string(),
            tenant_id: "tenant-a".to_string(),
        },
        TenantPost {
            id: 9999, // A new record
            title: "New Scoped Post".to_string(),
            tenant_id: String::new(), // Will be overridden to tenant-a
        },
    ];

    let upserted = with_tenant("tenant-a".to_string(), async {
        repo.upsert_many(&upsert_records).await.unwrap()
    })
    .await;

    // The returned upserted list should only contain:
    // - The updated post A2
    // - The newly created post 9999
    // And NOT post B2 (which is ignored by conflict filter)
    assert_eq!(upserted.len(), 2);
    assert!(
        upserted
            .iter()
            .any(|p| p.id == posts_a[1].id && p.title == "Upserted A2")
    );
    assert!(
        upserted
            .iter()
            .any(|p| p.id == 9999 && p.title == "New Scoped Post" && p.tenant_id == "tenant-a")
    );

    // Verify DB state across all tenants
    let all_posts_final = repo.across_tenants().find_all().await.unwrap();
    let post_b2_final = all_posts_final
        .iter()
        .find(|p| p.id == posts_b[1].id)
        .unwrap();
    assert_eq!(post_b2_final.title, "Post B2"); // Unchanged! No hijack.
    assert_eq!(post_b2_final.tenant_id, "tenant-b");

    // 6. delete_many under tenant-a context
    // Try to delete one tenant-a and one tenant-b post. Only tenant-a post should be deleted.
    with_tenant("tenant-a".to_string(), async {
        repo.delete_many(&[posts_a[1].id, posts_b[1].id])
            .await
            .unwrap();
    })
    .await;

    let all_posts_after_delete = repo.across_tenants().find_all().await.unwrap();
    assert!(all_posts_after_delete.iter().any(|p| p.id == posts_b[1].id)); // tenant-b post remains!
    assert!(!all_posts_after_delete.iter().any(|p| p.id == posts_a[1].id)); // tenant-a post is deleted!
}
