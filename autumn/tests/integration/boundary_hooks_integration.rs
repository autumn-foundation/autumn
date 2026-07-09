use autumn_web::app::app;
use std::sync::Arc;

// We expect these traits to be exposed under autumn_web::interceptor
use autumn_web::interceptor::JobInterceptor;

#[cfg(feature = "mail")]
use autumn_web::interceptor::MailInterceptor;

#[cfg(feature = "db")]
use autumn_web::interceptor::DbConnectionInterceptor;

#[cfg(feature = "ws")]
use autumn_web::interceptor::ChannelsInterceptor;

#[cfg(feature = "oauth2")]
use autumn_web::interceptor::HttpInterceptor;

#[cfg(feature = "mail")]
struct DummyMailInterceptor;
#[cfg(feature = "mail")]
impl MailInterceptor for DummyMailInterceptor {
    fn intercept<'a>(
        &'a self,
        _mail: &'a autumn_web::mail::Mail,
        next: std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<(), autumn_web::mail::MailError>>
                    + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), autumn_web::mail::MailError>> + Send + 'a>,
    > {
        next
    }
}

struct DummyJobInterceptor;
impl JobInterceptor for DummyJobInterceptor {
    fn intercept_enqueue<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
    > {
        next
    }

    fn intercept_execute<'a>(
        &'a self,
        _name: &'a str,
        _payload: &'a serde_json::Value,
        next: std::pin::Pin<
            Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
        >,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
    > {
        next
    }
}

#[cfg(feature = "db")]
struct DummyDbInterceptor;
#[cfg(feature = "db")]
impl DbConnectionInterceptor for DummyDbInterceptor {
    fn intercept_checkout<'a>(
        &'a self,
        _ctx: autumn_web::interceptor::DbCheckoutContext,
        next: std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<autumn_web::db::PooledConnection, autumn_web::AutumnError>,
                    > + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<autumn_web::db::PooledConnection, autumn_web::AutumnError>,
                > + Send
                + 'a,
        >,
    > {
        next
    }
}

#[cfg(feature = "ws")]
struct DummyChannelsInterceptor;
#[cfg(feature = "ws")]
impl ChannelsInterceptor for DummyChannelsInterceptor {
    fn intercept_publish(
        &self,
        topic: &str,
        msg: &autumn_web::channels::ChannelMessage,
        next: &dyn Fn(
            &str,
            &autumn_web::channels::ChannelMessage,
        ) -> Result<usize, autumn_web::channels::ChannelPublishError>,
    ) -> Result<usize, autumn_web::channels::ChannelPublishError> {
        next(topic, msg)
    }
}

#[cfg(feature = "oauth2")]
struct DummyHttpInterceptor;
#[cfg(feature = "oauth2")]
impl HttpInterceptor for DummyHttpInterceptor {
    fn intercept<'a>(
        &'a self,
        req: reqwest::Request,
        next: &'a dyn Fn(
            reqwest::Request,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>
                    + Send
                    + 'a,
            >,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<reqwest::Response, reqwest::Error>> + Send + 'a,
        >,
    > {
        next(req)
    }
}

#[test]
fn app_builder_registers_interceptors() {
    let mut builder = app();

    #[cfg(feature = "mail")]
    {
        builder = builder.with_mail_interceptor(DummyMailInterceptor);
    }

    builder = builder.with_job_interceptor(DummyJobInterceptor);

    #[cfg(feature = "db")]
    {
        builder = builder.with_db_interceptor(DummyDbInterceptor);
    }

    #[cfg(feature = "ws")]
    {
        builder = builder.with_channels_interceptor(DummyChannelsInterceptor);
    }

    #[cfg(feature = "oauth2")]
    {
        builder = builder.with_http_interceptor(DummyHttpInterceptor);
    }

    let _ = builder;
}

#[cfg(feature = "mail")]
#[tokio::test]
async fn mail_interceptor_intercepts_sends() {
    // Converted from hand-rolled RecordingMailInterceptor to the built-in helpers.
    use autumn_web::get;
    use autumn_web::mail::{Mail, Mailer, Transport};
    use autumn_web::test::TestApp;

    #[get("/send-test-mail")]
    async fn send_test_mail(mailer: Mailer) -> &'static str {
        let mail = Mail::builder()
            .to("test@example.com")
            .subject("Interception Test")
            .text("This should be intercepted")
            .build()
            .unwrap();
        mailer.send(mail).await.unwrap();
        "sent"
    }

    let mut config = autumn_web::config::AutumnConfig::default();
    config.mail.transport = Transport::Log;
    config.mail.from = Some("noreply@example.com".to_string());

    let client = TestApp::new()
        .config(config)
        .routes(autumn_web::routes![send_test_mail])
        .build();

    client.get("/send-test-mail").send().await.assert_ok();

    // ~40-line hand-rolled interceptor replaced by one assertion:
    client.assert_email_count(1);
}

#[tokio::test]
async fn job_interceptor_intercepts_enqueue_and_execute() {
    use autumn_web::AppState;
    use autumn_web::job::{self, JobInfo, clear_global_job_client, global_job_runtime_test_lock};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static ENQUEUES: AtomicUsize = AtomicUsize::new(0);
    static EXECUTES: AtomicUsize = AtomicUsize::new(0);

    struct RecordingJobInterceptor;
    impl JobInterceptor for RecordingJobInterceptor {
        fn intercept_enqueue<'a>(
            &'a self,
            name: &'a str,
            payload: &'a serde_json::Value,
            next: std::pin::Pin<
                Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
            >,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
        > {
            Box::pin(async move {
                assert_eq!(name, "test-job");
                assert_eq!(payload["data"], "hello");
                ENQUEUES.fetch_add(1, Ordering::SeqCst);
                next.await
            })
        }

        fn intercept_execute<'a>(
            &'a self,
            name: &'a str,
            payload: &'a serde_json::Value,
            next: std::pin::Pin<
                Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
            >,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'a>,
        > {
            Box::pin(async move {
                assert_eq!(name, "test-job");
                assert_eq!(payload["data"], "hello");
                EXECUTES.fetch_add(1, Ordering::SeqCst);
                next.await
            })
        }
    }

    fn test_job_handler(
        _state: AppState,
        _payload: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = autumn_web::AutumnResult<()>> + Send + 'static>,
    > {
        Box::pin(async move { Ok(()) })
    }

    let _guard = global_job_runtime_test_lock().lock().await;
    clear_global_job_client();

    let state = AppState::for_test();
    state.insert_extension(Arc::new(RecordingJobInterceptor) as Arc<dyn JobInterceptor>);

    let shutdown = tokio_util::sync::CancellationToken::new();
    let config = autumn_web::config::JobConfig::default();

    let job_info = JobInfo {
        name: "test-job".to_string(),
        max_attempts: 1,
        initial_backoff_ms: 1,
        queue: "default".to_string(),
        uniqueness: None,
        concurrency: None,
        handler: test_job_handler,
    };

    job::start_runtime(vec![job_info], &state, &shutdown, &config).unwrap();

    job::enqueue("test-job", json!({ "data": "hello" }))
        .await
        .unwrap();

    // Sleep briefly to let the local worker process the job
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    shutdown.cancel();

    assert_eq!(ENQUEUES.load(Ordering::SeqCst), 1);
    assert_eq!(EXECUTES.load(Ordering::SeqCst), 1);

    clear_global_job_client();
}

#[cfg(feature = "db")]
#[tokio::test]
async fn db_connection_interceptor_intercepts_checkout() {
    use autumn_web::db::Db;
    use autumn_web::get;
    use autumn_web::interceptor::DbCheckoutContext;
    use autumn_web::test::TestApp;
    use diesel_async::AsyncPgConnection;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CHECKOUTS: AtomicUsize = AtomicUsize::new(0);

    struct RecordingDbInterceptor;
    impl DbConnectionInterceptor for RecordingDbInterceptor {
        fn intercept_checkout<'a>(
            &'a self,
            ctx: DbCheckoutContext,
            next: std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                autumn_web::db::PooledConnection,
                                autumn_web::AutumnError,
                            >,
                        > + Send
                        + 'a,
                >,
            >,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<autumn_web::db::PooledConnection, autumn_web::AutumnError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                assert_eq!(ctx.pool_name, "primary");
                CHECKOUTS.fetch_add(1, Ordering::SeqCst);
                next.await
            })
        }
    }

    #[get("/db-test")]
    async fn db_test(_db: Db) -> &'static str {
        "ok"
    }

    let manager =
        AsyncDieselConnectionManager::<AsyncPgConnection>::new("postgres://127.0.0.1:54321/dummy");
    let pool = Pool::builder(manager).build().unwrap();

    let client = TestApp::new()
        .routes(autumn_web::routes![db_test])
        .with_db(pool)
        .with_db_interceptor(RecordingDbInterceptor)
        .build();

    let response = client.get("/db-test").send().await;
    assert_eq!(response.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(CHECKOUTS.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "ws")]
#[tokio::test]
async fn channels_interceptor_intercepts_publish() {
    use autumn_web::channels::{ChannelMessage, ChannelPublishError};
    use autumn_web::get;
    use autumn_web::test::TestApp;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static PUBLISH_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct RecordingChannelsInterceptor;
    impl ChannelsInterceptor for RecordingChannelsInterceptor {
        fn intercept_publish(
            &self,
            topic: &str,
            msg: &ChannelMessage,
            next: &dyn Fn(&str, &ChannelMessage) -> Result<usize, ChannelPublishError>,
        ) -> Result<usize, ChannelPublishError> {
            assert_eq!(topic, "test-topic");
            assert_eq!(msg.as_str(), "test-message");
            PUBLISH_CALLS.fetch_add(1, Ordering::SeqCst);
            next(topic, msg)
        }
    }

    #[get("/publish-test")]
    async fn publish_test(
        autumn_web::prelude::State(state): autumn_web::prelude::State<autumn_web::AppState>,
    ) -> &'static str {
        state
            .channels()
            .publish("test-topic", "test-message")
            .unwrap();
        "ok"
    }

    let client = TestApp::new()
        .routes(autumn_web::routes![publish_test])
        .with_channels_interceptor(RecordingChannelsInterceptor)
        .build();

    let response = client.get("/publish-test").send().await;
    response.assert_ok();

    assert_eq!(PUBLISH_CALLS.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "oauth2")]
static HTTP_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "oauth2")]
struct RecordingHttpInterceptor;

#[cfg(feature = "oauth2")]
#[cfg(feature = "oauth2")]
impl HttpInterceptor for RecordingHttpInterceptor {
    fn intercept<'a>(
        &'a self,
        req: reqwest::Request,
        next: &'a dyn Fn(reqwest::Request) -> autumn_web::interceptor::HttpInterceptorFuture<'a>,
    ) -> autumn_web::interceptor::HttpInterceptorFuture<'a> {
        assert_eq!(req.url().as_str(), "http://127.0.0.1:54321/token");
        HTTP_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        next(req)
    }
}

#[cfg(feature = "oauth2")]
#[autumn_web::get("/test-oauth-client")]
async fn test_oauth_client(session: autumn_web::session::Session) -> &'static str {
    use autumn_web::auth::{OAuth2Callback, OAuth2ProviderConfig, oauth2_finish_login};

    let state_val = "test-state-val";
    session
        .insert("oauth2:github:state", state_val.to_string())
        .await;
    session
        .insert("oauth2:github:code_verifier", "test-verifier".to_string())
        .await;

    let provider = OAuth2ProviderConfig {
        client_id: "client-id".to_string(),
        client_secret: "client-secret".to_string(),
        authorize_url: "https://example.com/auth".to_string(),
        token_url: "http://127.0.0.1:54321/token".to_string(),
        userinfo_url: None,
        redirect_uri: "http://127.0.0.1/callback".to_string(),
        scope: "user".to_string(),
        issuer: None,
        jwks_url: None,
        discovery_url: None,
    };

    let callback = OAuth2Callback {
        code: "code".to_string(),
        state: state_val.to_string(),
    };

    let _ = oauth2_finish_login(&session, "github", &provider, &callback).await;

    "ok"
}

#[cfg(feature = "oauth2")]
#[tokio::test]
async fn http_interceptor_intercepts_calls() {
    use autumn_web::test::TestApp;
    use std::sync::atomic::Ordering;

    HTTP_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .routes(autumn_web::routes![test_oauth_client])
        .with_http_interceptor(RecordingHttpInterceptor)
        .build();

    let response = client.get("/test-oauth-client").send().await;
    response.assert_ok();

    assert_eq!(HTTP_CALLS.load(Ordering::SeqCst), 1);
}
