use autumn_web::job;
use autumn_web::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportArgs {
    account_id: i64,
}

#[job(name = "export_orders")]
async fn export_orders(
    _state: AppState,
    args: ExportArgs,
    ctx: job::JobContext,
) -> AutumnResult<()> {
    ctx.set_progress(50, Some("Rows 1200/5000")).await?;
    ctx.set_result(serde_json::json!({ "download_url": format!("/blob/{}.csv", args.account_id) }));
    Ok(())
}

async fn use_companion() -> AutumnResult<()> {
    let handle = ExportOrdersJob::enqueue_tracked(ExportArgs { account_id: 42 }).await?;
    let _path: String = handle.status_path();

    let _handle = ExportOrdersJob::enqueue_tracked_for(
        ExportArgs { account_id: 42 },
        job::TrackedJobOwner::Anonymous,
    )
    .await?;

    Ok(())
}

fn main() {
    let _ = use_companion;
}
