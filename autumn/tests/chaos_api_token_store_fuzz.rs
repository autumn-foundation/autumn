use autumn_web::auth::{ApiTokenStore, InMemoryApiTokenStore, IssueTokenSpec};
use proptest::prelude::*;
use proptest::test_runner::Config;
use std::sync::Arc;
use tokio::runtime::Runtime;

proptest! {
    #![proptest_config(Config::with_cases(1000))]
    #[test]
    fn test_fuzz_api_token_store(
        principal_id in "\\PC*",
        name in "\\PC*",
    ) {
        let store = Arc::new(InMemoryApiTokenStore::default());
        let rt = Runtime::new().unwrap();

        let spec = IssueTokenSpec {
            principal_id: &principal_id,
            name: &name,
            scopes: &[],
            expires_at: None,
        };
        let token = rt.block_on(store.issue_scoped(spec)).unwrap();

        // Revoke token
        rt.block_on(store.revoke(&token)).unwrap();

        // Ensure verify returns None
        let verified = rt.block_on(store.verify(&token)).unwrap();
        assert!(verified.is_none());

        // Revoke again should not panic or fail
        rt.block_on(store.revoke(&token)).unwrap();
    }
}
