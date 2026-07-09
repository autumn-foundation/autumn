use autumn_web::error::{AutumnError, AutumnResult};
use autumn_web::pagination::{Page, PageRequest};
use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};
use validator::Validate;

use crate::schema::todos;

/// A todo item loaded from the database.
#[derive(Queryable, Selectable, Serialize)]
#[diesel(table_name = todos)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Todo {
    pub id: i64,
    pub title: String,
    pub completed: bool,
    pub created_at: chrono::NaiveDateTime,
}

impl Todo {
    /// Load all todos ordered by creation date (newest first).
    pub async fn all(db: &mut AsyncPgConnection) -> AutumnResult<Vec<Self>> {
        Ok(todos::table
            .order(todos::created_at.desc())
            .select(Self::as_select())
            .load(db)
            .await?)
    }

    /// Load a page of todos ordered by creation date (newest first).
    ///
    /// Accepts a [`PageRequest`] and returns a [`Page`] containing the items
    /// together with total-elements / total-pages metadata.
    pub async fn page(req: &PageRequest, db: &mut AsyncPgConnection) -> AutumnResult<Page<Self>> {
        let total: i64 = todos::table.count().get_result(db).await?;
        let items = todos::table
            .order((todos::created_at.desc(), todos::id.desc()))
            .limit(req.limit())
            .offset(req.offset())
            .select(Self::as_select())
            .load(db)
            .await?;
        Ok(Page::new(items, total, req))
    }

    /// Find a single todo by ID, returning 404 if not found.
    pub async fn find(id: i64, db: &mut AsyncPgConnection) -> AutumnResult<Self> {
        todos::table
            .find(id)
            .select(Self::as_select())
            .first(db)
            .await
            .map_err(AutumnError::not_found)
    }
}

impl autumn_web::data::csv::CsvSchema for Todo {
    fn csv_columns() -> &'static [&'static str] {
        &["id", "title", "completed", "created_at"]
    }

    fn to_csv_record(&self) -> Vec<String> {
        vec![
            self.id.to_string(),
            self.title.clone(),
            self.completed.to_string(),
            self.created_at.to_string(),
        ]
    }
}

/// Data needed to insert a new todo.
///
/// Derives [`Validate`] so it can be used directly with
/// [`ChangesetForm<NewTodo>`](autumn_web::form::ChangesetForm) when
/// the form shape matches the model shape — no separate form struct needed.
///
/// When the form requires extra fields, different validation rules, or
/// UI-specific concerns (e.g. a `confirm_password` field), define a
/// dedicated form struct instead and convert it to `NewTodo` on success.
#[derive(Insertable, Deserialize, Serialize, Validate)]
#[diesel(table_name = todos)]
pub struct NewTodo {
    #[validate(
        length(min = 1, max = 255, message = "Title must be 1–255 characters"),
        custom(function = "title_not_blank")
    )]
    pub title: String,
}

/// Validate that a title has at least one non-whitespace character.
pub(crate) fn title_not_blank(s: &str) -> Result<(), validator::ValidationError> {
    if s.trim().is_empty() {
        let mut e = validator::ValidationError::new("blank");
        e.message = Some("Title must not be blank or whitespace-only".into());
        return Err(e);
    }
    Ok(())
}

impl NewTodo {
    /// Validate and normalize the title. Returns 422 if the title is empty.
    pub fn validated(self) -> AutumnResult<Self> {
        let title = self.title.trim().to_owned();
        if title.is_empty() {
            return Err(AutumnError::unprocessable_msg("Title must not be empty"));
        }
        Ok(Self { title })
    }
}
