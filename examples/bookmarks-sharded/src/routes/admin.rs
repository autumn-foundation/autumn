//! Cross-shard admin/aggregate endpoints.
//!
//! `Shards::each_shard` fans out concurrently and collects per-shard
//! results instead of short-circuiting, so a single down shard degrades
//! this endpoint to a partial answer rather than a 500. There are no
//! cross-shard transactions: the counts below are independent snapshots
//! and can observe writes that land mid-fan-out.

use std::collections::BTreeMap;

use autumn_web::prelude::*;
use autumn_web::reexports::diesel::prelude::*;
use autumn_web::reexports::diesel_async::RunQueryDsl;

use crate::schema::bookmarks;

#[derive(serde::Serialize)]
pub struct ShardStat {
    pub slots: usize,
    pub bookmarks: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[get("/api/stats")]
pub async fn stats(shards: Shards) -> AutumnResult<Json<BTreeMap<String, ShardStat>>> {
    let results = shards
        .each_shard(|shard, mut db| {
            let name = shard.name().to_owned();
            let slots = shard.slots().len();
            async move {
                let count = bookmarks::table
                    .count()
                    .get_result::<i64>(&mut *db)
                    .await
                    .map_err(AutumnError::from)?;
                Ok((name, slots, count))
            }
        })
        .await;

    let mut stats = BTreeMap::new();
    for (shard_id, result) in results {
        match result {
            Ok((name, slots, count)) => {
                stats.insert(
                    name,
                    ShardStat {
                        slots,
                        bookmarks: Some(count),
                        error: None,
                    },
                );
            }
            Err(error) => {
                let shard = shards
                    .set()
                    .get(shard_id)
                    .map_or_else(|| format!("shard {}", shard_id.0), |s| s.name().to_owned());
                let slots = shards.set().get(shard_id).map_or(0, |s| s.slots().len());
                stats.insert(
                    shard,
                    ShardStat {
                        slots,
                        bookmarks: None,
                        error: Some(error.to_string()),
                    },
                );
            }
        }
    }
    Ok(Json(stats))
}
