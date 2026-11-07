//! The Postgres slot-ownership store (`PgSlotOwnership`): claim, renew,
//! fencing and takeover on a real Postgres.
//!
//! These tests need Docker (testcontainers). They are `#[ignore]`d, and the CI
//! Docker sweep runs them.

#![cfg(all(feature = "postgres", not(feature = "sqlite")))]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::db::fleet_ownership::{
    FleetOwnership, OwnerId, OwnershipError, PgSlotOwnership, SlotOwnershipStore as _, SlotRange,
};
use autumn_web::lock::FencingToken;
use autumn_web::time::TickingClock;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const TTL: Duration = Duration::from_secs(30);

async fn start_postgres() -> (String, testcontainers::ContainerAsync<Postgres>) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        container,
    )
}

fn pool_for(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager)
        .max_size(8)
        .wait_timeout(Some(Duration::from_secs(5)))
        .create_timeout(Some(Duration::from_secs(5)))
        .runtime(deadpool::Runtime::Tokio1)
        .build()
        .expect("pool")
}

fn clock() -> TickingClock {
    TickingClock::starting_at(chrono::DateTime::from_timestamp_millis(1_000_000).unwrap())
}

/// A store on its own pool: one host.
async fn store(url: &str, clock: &TickingClock) -> PgSlotOwnership {
    let store = PgSlotOwnership::new(pool_for(url), Arc::new(clock.clone()));
    store.ensure_schema().await.expect("schema");
    store
}

fn owner(id: &str) -> OwnerId {
    OwnerId::new(id).unwrap()
}

fn range(start: u16, end: u16) -> SlotRange {
    SlotRange::new(start, end).unwrap()
}

fn token(n: i64) -> FencingToken {
    FencingToken::try_from(n).unwrap()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ownership_claims_renews_and_releases() {
    let (url, _pg) = start_postgres().await;
    let clock = clock();
    let s = store(&url, &clock).await;
    let grant = s.claim(SlotRange::ALL, &owner("a"), TTL, 0).await.unwrap();
    assert_eq!(grant.epoch, token(1));
    assert!(grant.fresh);
    // A restart of the same owner keeps the epoch.
    let again = s.claim(SlotRange::ALL, &owner("a"), TTL, 0).await.unwrap();
    assert_eq!(again.epoch, token(1));
    assert!(!again.fresh);
    // Another owner waits for the live lease.
    assert!(matches!(
        s.claim(SlotRange::ALL, &owner("b"), TTL, 0).await,
        Err(OwnershipError::Held { .. })
    ));
    clock.advance(Duration::from_secs(10));
    let renewed = s
        .renew(SlotRange::ALL, &owner("a"), grant.epoch, TTL)
        .await
        .unwrap();
    assert!(renewed.lease_expires_ms > grant.lease_expires_ms);
    let rows = s.rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].lease_expires_ms, renewed.lease_expires_ms);
    // A release lets the next owner claim at once, at a new epoch.
    assert!(
        s.release(SlotRange::ALL, &owner("a"), grant.epoch)
            .await
            .unwrap()
    );
    let next = s.claim(SlotRange::ALL, &owner("b"), TTL, 0).await.unwrap();
    assert_eq!(next.epoch, token(2));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ownership_fences_the_old_owner_after_a_lapse() {
    let (url, _pg) = start_postgres().await;
    let clock = clock();
    let s = store(&url, &clock).await;
    let old = s.claim(range(0, 99), &owner("a"), TTL, 0).await.unwrap();
    clock.advance(TTL);
    let new = s.claim(range(0, 99), &owner("b"), TTL, 0).await.unwrap();
    assert_eq!(new.epoch, token(2));
    assert_eq!(
        s.renew(range(0, 99), &owner("a"), old.epoch, TTL).await,
        Err(OwnershipError::Lost {
            range: range(0, 99)
        })
    );
    assert!(
        !s.release(range(0, 99), &owner("a"), old.epoch)
            .await
            .unwrap()
    );
    // A new shape removes the old row. The counter keeps the epochs unique.
    assert!(
        s.release(range(0, 99), &owner("b"), new.epoch)
            .await
            .unwrap()
    );
    let reshaped = s.claim(range(0, 49), &owner("c"), TTL, 0).await.unwrap();
    assert_eq!(reshaped.epoch, token(3));
    let rows = s.rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].range, range(0, 49));
    // A floor from the replicas lifts the next epoch above it.
    let floored = s.claim(range(50, 99), &owner("c"), TTL, 10).await.unwrap();
    assert_eq!(floored.epoch, token(11));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ownership_two_hosts_racing_a_claim_get_one_owner() {
    let (url, _pg) = start_postgres().await;
    let clock = clock();
    // Two pools: two hosts. The schema calls race too.
    let (a, b) = tokio::join!(store(&url, &clock), store(&url, &clock));
    for round in 0..8_u16 {
        let slots = range(round * 100, round * 100 + 99);
        // A third session holds the table lock, so both claims wait on it
        // and then run one after the other.
        let mut holder = pool_for(&url).get().await.unwrap();
        holder
            .batch_execute("BEGIN; LOCK TABLE _autumn_fleet_slot_owners IN EXCLUSIVE MODE")
            .await
            .unwrap();
        let (ha, hb) = (owner("a"), owner("b"));
        let claims =
            async { tokio::join!(a.claim(slots, &ha, TTL, 0), b.claim(slots, &hb, TTL, 0)) };
        let release = async {
            waiters(&url, 2).await;
            holder.batch_execute("COMMIT").await.unwrap();
        };
        let ((ra, rb), ()) = tokio::join!(claims, release);
        assert!(ra.is_ok() != rb.is_ok(), "round {round}: {ra:?} {rb:?}");
        let loser = if ra.is_ok() { rb } else { ra };
        assert!(
            matches!(loser, Err(OwnershipError::Held { .. })),
            "the loser is refused, not failed: {loser:?}"
        );
    }
    let rows = a.rows().await.unwrap();
    assert_eq!(rows.len(), 8);
    let mut epochs: Vec<i64> = rows.iter().map(|row| row.epoch.as_i64()).collect();
    epochs.sort_unstable();
    assert_eq!(epochs, (1..=8).collect::<Vec<_>>(), "unique epochs");
}

/// Waits until `n` sessions wait for a lock.
async fn waiters(url: &str, n: i64) {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = pool_for(url).get().await.unwrap();
    for _ in 0..500 {
        let waiting: Count =
            diesel::sql_query("SELECT COUNT(*) AS n FROM pg_locks WHERE NOT granted")
                .get_result(&mut *conn)
                .await
                .unwrap();
        if waiting.n >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{n} sessions did not wait for the lock");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ownership_schema_is_safe_when_many_hosts_boot_at_once() {
    let (url, _pg) = start_postgres().await;
    let clock = clock();
    let stores: Vec<PgSlotOwnership> = (0..4)
        .map(|_| PgSlotOwnership::new(pool_for(&url), Arc::new(clock.clone())))
        .collect();
    let results =
        futures::future::join_all(stores.iter().map(PgSlotOwnership::ensure_schema)).await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    stores[0].ensure_schema().await.expect("again");
}

/// Takeover through `FleetOwnership`: host b waits while a holds the range,
/// claims it at a higher epoch after a's lease lapses, and a sees the loss.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ownership_a_new_host_takes_over_a_lapsed_range() {
    let (url, _pg) = start_postgres().await;
    let clock = clock();
    let host = |id: &str, store: PgSlotOwnership| {
        Arc::new(FleetOwnership::new(Arc::new(store), owner(id), vec![range(0, 999)], TTL).unwrap())
    };
    let a = host("a", store(&url, &clock).await);
    let b = host("b", store(&url, &clock).await);

    let first = a.refresh().await;
    assert_eq!(first.claimed, [(range(0, 999), token(1))]);
    assert_eq!(a.epoch_for(5), Some(token(1)));
    let waiting = b.refresh().await;
    assert_eq!(waiting.waiting, [range(0, 999)]);
    assert_eq!(b.epoch_for(5), None);

    // Host a stops renewing. Its lease lapses on the store's clock.
    clock.advance(TTL + Duration::from_secs(1));
    let taken = b.refresh().await;
    assert_eq!(taken.claimed, [(range(0, 999), token(2))]);
    assert_eq!(b.epoch_for(5), Some(token(2)));

    // Host a still serves on its local lease until it renews. Its epoch is
    // older, so the replicas fence its writes.
    assert_eq!(a.epoch_for(5), Some(token(1)));
    let lost = a.refresh().await;
    assert_eq!(lost.lost, [range(0, 999)]);
    assert_eq!(a.epoch_for(5), None, "the old owner is fenced");
}

/// Without `fleet_ownership_store`, the app's store is `PgSlotOwnership` on
/// the control pool.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ownership_is_the_default_store_of_a_postgres_app() {
    let (url, _pg) = start_postgres().await;
    let state = autumn_web::AppState::for_test().with_pool(pool_for(&url));
    let store = autumn_web::fleet::ownership_store(&state).expect("a control pool");
    store.ensure_schema().await.unwrap();
    let grant = store
        .claim(SlotRange::ALL, &owner("a"), TTL, 0)
        .await
        .unwrap();
    assert_eq!(grant.epoch, token(1));
    let direct = PgSlotOwnership::new(pool_for(&url), Arc::new(clock()));
    assert_eq!(
        direct.rows().await.unwrap().len(),
        1,
        "the row is in Postgres"
    );
}
