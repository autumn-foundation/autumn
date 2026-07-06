use autumn_web::auth::{ApiTokenStore, InMemoryApiTokenStore, IssueTokenSpec};
use loom::sync::Arc;
use loom::thread;

#[test]
fn api_token_store_concurrent_writes() {
    loom::model(|| {
        let store = Arc::new(InMemoryApiTokenStore::default());

        let s1 = store.clone();
        let t1 = thread::spawn(move || {
            let spec = IssueTokenSpec {
                principal_id: "user1",
                name: "token1",
                scopes: &[],
                expires_at: None,
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let _ = rt.block_on(s1.issue_scoped(spec));
        });

        let s2 = store.clone();
        let t2 = thread::spawn(move || {
            let spec = IssueTokenSpec {
                principal_id: "user2",
                name: "token2",
                scopes: &[],
                expires_at: None,
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let _ = rt.block_on(s2.issue_scoped(spec));
        });

        t1.join().unwrap();
        t2.join().unwrap();
    });
}
