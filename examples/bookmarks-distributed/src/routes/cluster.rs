// ── 0.7.0 Feature: the self-clustering substrate ────────────────
//
// Two replicas of this binary find each other, agree on who is running,
// and share one cluster-wide counter with **no external coordination
// service** — no Redis, no Postgres, no etcd. The compose stack already
// runs two web replicas behind nginx (`docker/nginx/nginx.conf`), so it
// only takes a `[cluster]` section and a shared secret to make them a
// cluster; see `autumn-docker.toml` and `docker-compose.yml`.
//
// See `docs/guide/clustering.md`. Note what this is *not*: the counter
// is eventually consistent and cannot fence anything. Work that must run
// on exactly one replica belongs to `docs/guide/distributed-locks.md` or
// the multi-replica scheduler, not here.

use autumn_web::cluster::ClusterHandle;
use autumn_web::prelude::*;

/// Name of the shared grow-only counter. Both replicas must use the same
/// string; a cell key is `<counter name>#<node id>`, which is why `#` is
/// rejected in node ids and cluster names.
pub const BOOKMARKS_CREATED: &str = "bookmarks_created";

/// Add one to this node's own entry in the cluster-wide bookmark counter.
///
/// Synchronous, never fails, and applied locally right away; peers see it
/// on the next push interval. A disabled `[cluster]` section installs no
/// extension at all, so this is a no-op under a plain `cargo run` — which
/// is why the create path calls it unconditionally instead of erroring.
pub fn record_bookmark_created(state: &AppState) {
    if let Some(cluster) = state.extension::<ClusterHandle>() {
        cluster.counter(BOOKMARKS_CREATED).increment();
    }
}

/// What this replica currently believes about the cluster.
///
/// Hit it through nginx and you land on whichever replica the load
/// balancer picked, so repeated calls show the two `node` values
/// alternating while `bookmarks_created` converges to the same total on
/// both — that convergence, with nothing coordinating it, is the whole
/// point of the subsystem.
#[get("/cluster")]
pub async fn status(State(state): State<AppState>) -> AutumnResult<Json<serde_json::Value>> {
    let Some(cluster) = state.extension::<ClusterHandle>() else {
        // Not an error: `[cluster] enabled` is false by default, and the
        // dev profile of this example does not turn it on.
        return Ok(Json(serde_json::json!({
            "enabled": false,
            "hint": "set [cluster] enabled = true — see docs/guide/clustering.md",
        })));
    };

    let members: Vec<serde_json::Value> = cluster
        .members()
        .into_iter()
        .map(|member| {
            serde_json::json!({
                "id": member.id,
                "addr": member.addr.to_string(),
                "status": format!("{:?}", member.status).to_lowercase(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "enabled": true,
        "node": cluster.node_id(),
        "members": members,
        // A lower bound on the cluster-wide total, never a limit to
        // enforce: `get()` may jump upward as a peer's state merges in,
        // and never moves downward.
        "bookmarks_created": cluster.counter(BOOKMARKS_CREATED).get(),
    })))
}
