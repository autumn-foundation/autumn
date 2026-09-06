//! The GraphQL surface over the `Note` model and repository.
//!
//! Nothing here touches SQL. Every resolver builds a [`PgNoteRepository`]
//! from the pool on the request's `AppState` — the same pool, the same
//! generated repository, and the same [`crate::hooks::NoteHooks`] the REST
//! handlers and the startup seed use — so validation, trimming, and
//! transactions behave identically no matter which door a write comes in.

use async_graphql::{
    Context, EmptySubscription, ErrorExtensions, InputObject, Object, Result, Schema,
};
use autumn_web::hooks::Patch;
use autumn_web::{AppState, AutumnError};

use crate::models::{NewNote, Note, UpdateNote};
use crate::repositories::{NoteRepository, PgNoteRepository};

/// The GraphQL `Note` object, resolved straight off the `#[model]` struct.
///
/// An `#[Object]` impl block turns an existing type into a GraphQL object
/// without changing its definition, which keeps `models.rs` free of any
/// GraphQL vocabulary. `id` is exposed as `Int` (the schema's `i32`) for
/// client simplicity; the column is `BIGINT`.
#[Object]
impl Note {
    async fn id(&self) -> i64 {
        self.id
    }
    async fn title(&self) -> &str {
        &self.title
    }
    async fn body(&self) -> &str {
        &self.body
    }
    async fn pinned(&self) -> bool {
        self.pinned
    }
    /// RFC 3339 UTC timestamp, so the client needs no date library.
    async fn created_at(&self) -> String {
        self.created_at.and_utc().to_rfc3339()
    }
}

/// Input for `createNote`. The model's `#[normalize(trim)]` trims both fields
/// and its `#[validate]` rejects an empty title; this type just carries them.
#[derive(Debug, InputObject)]
pub struct NewNoteInput {
    pub title: String,
    #[graphql(default)]
    pub body: String,
}

/// Build the repository for this request from the `AppState` the plugin put
/// on the context. `with_pool_untracked` is the constructor for code that
/// runs outside an axum extractor (a resolver, a task, a seed); in a route
/// handler you would take `repo: PgNoteRepository` as an argument instead.
fn repo(ctx: &Context<'_>) -> Result<PgNoteRepository> {
    let pool = ctx
        .data::<AppState>()?
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("no database pool configured"))
        .map_err(to_gql)?
        .clone();
    Ok(PgNoteRepository::with_pool_untracked(pool))
}

/// Map a repository/framework error onto a GraphQL field error.
///
/// A GraphQL response is an HTTP `200`, so it never passes through the
/// framework's problem-details filter that redacts server-side errors. This
/// does the equivalent: a client error (`4xx` — validation, the pinned-delete
/// rule, an unknown id) keeps its message, which was written for the client;
/// a server error (`5xx` — a failed query, a lost connection, no pool) is
/// logged with its detail and replaced by a generic message, so database
/// diagnostics never reach the wire. Either way the HTTP status the error
/// would have carried travels in `extensions.status`, so a client can still
/// tell a `422` from a `503` without an HTTP status to read.
fn to_gql(err: AutumnError) -> async_graphql::Error {
    let status = err.status();
    let message = if status.is_server_error() {
        tracing::error!(status = %status, error = %err, "GraphQL resolver failed");
        "internal server error".to_owned()
    } else {
        err.to_string()
    };
    async_graphql::Error::new(message).extend_with(|_, e| e.set("status", status.as_u16()))
}

/// Newest first. `find_all`/`find_by_pinned` return primary-key order, and
/// `id` is a `BIGSERIAL`, so reversing is the same as `ORDER BY id DESC`.
fn newest_first(mut notes: Vec<Note>) -> Vec<Note> {
    notes.reverse();
    notes
}

/// GraphQL `Query` root.
pub struct Query;

#[Object]
impl Query {
    /// All notes, newest first. `pinnedOnly: true` narrows to pinned notes.
    async fn notes(
        &self,
        ctx: &Context<'_>,
        #[graphql(default)] pinned_only: bool,
    ) -> Result<Vec<Note>> {
        let repo = repo(ctx)?;
        let notes = if pinned_only {
            repo.find_by_pinned(true).await.map_err(to_gql)?
        } else {
            repo.find_all().await.map_err(to_gql)?
        };
        Ok(newest_first(notes))
    }

    /// One note by id, or `null`.
    async fn note(&self, ctx: &Context<'_>, id: i64) -> Result<Option<Note>> {
        repo(ctx)?.find_by_id(id).await.map_err(to_gql)
    }
}

/// GraphQL `Mutation` root.
pub struct Mutation;

#[Object]
impl Mutation {
    /// Create a note. Trimming and the 1–120 character rule come from the
    /// model, not from this resolver.
    async fn create_note(&self, ctx: &Context<'_>, input: NewNoteInput) -> Result<Note> {
        let new = NewNote {
            title: input.title,
            body: input.body,
            pinned: false,
        };
        repo(ctx)?.save(&new).await.map_err(to_gql)
    }

    /// Flip a note's pinned flag. Errors if the id is unknown.
    async fn toggle_pinned(&self, ctx: &Context<'_>, id: i64) -> Result<Note> {
        let repo = repo(ctx)?;
        let current = repo
            .find_by_id(id)
            .await
            .map_err(to_gql)?
            .ok_or_else(|| format!("no note with id {id}"))?;
        let changes = UpdateNote {
            pinned: Patch::Set(!current.pinned),
            ..Default::default()
        };
        repo.update(id, &changes).await.map_err(to_gql)
    }

    /// Delete a note. Returns whether anything was deleted. A pinned note is
    /// refused by the repository's `before_delete` hook, which surfaces here
    /// as a field error.
    async fn delete_note(&self, ctx: &Context<'_>, id: i64) -> Result<bool> {
        let repo = repo(ctx)?;
        if !repo.exists_by_id(id).await.map_err(to_gql)? {
            return Ok(false);
        }
        repo.delete_by_id(id).await.map_err(to_gql)?;
        Ok(true)
    }
}

/// The executable schema. Nothing is bound into it at build time — the pool
/// arrives per request through `AppState` — so it is a plain value the plugin
/// can clone freely.
#[must_use]
pub fn build_schema() -> Schema<Query, Mutation, EmptySubscription> {
    Schema::build(Query, Mutation, EmptySubscription).finish()
}
