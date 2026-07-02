//! Async CSV export as a tracked background job (issue #1373).
//!
//! Demonstrates the enqueue → progress → download-link flow end to end: the
//! initiating request returns immediately with a poll token instead of
//! blocking on the export, and the browser polls `handle.status_path()` for
//! progress and the final result.

use autumn_web::data::csv::export_csv;
use autumn_web::job::JobContext;
use autumn_web::prelude::*;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::models::Todo;

/// No input needed today — the export always covers every todo. A real app
/// might carry a filter here (e.g. a date range or `completed` flag).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportTodosArgs {}

/// Stream every todo to CSV off the request thread, reporting progress and
/// handing back a downloadable link when done.
///
/// Real apps typically upload the CSV to blob storage (see the `storage`
/// feature) and set `download_url` to a presigned link; this example embeds
/// the CSV directly as a `data:` URL so the demo needs no extra
/// infrastructure to run.
#[job(name = "export_todos_csv")]
pub async fn export_todos_csv(
    state: AppState,
    _args: ExportTodosArgs,
    ctx: JobContext,
) -> AutumnResult<()> {
    ctx.set_progress(0, Some("Loading todos")).await?;

    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::internal_server_error_msg("database not configured"))?;
    let mut conn = pool.get().await.map_err(|error| {
        AutumnError::internal_server_error_msg(format!("database pool error: {error}"))
    })?;
    let todos = Todo::all(&mut conn).await?;

    ctx.set_progress(50, Some(&format!("Encoding {} todos", todos.len())))
        .await?;

    let mut csv = Vec::new();
    export_csv(todos, &mut csv).map_err(|error| {
        AutumnError::internal_server_error_msg(format!("csv export failed: {error}"))
    })?;
    let download_url = format!(
        "data:text/csv;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&csv)
    );

    ctx.set_progress(100, Some("Done")).await?;
    ctx.set_result(serde_json::json!({ "download_url": download_url }));
    Ok(())
}
