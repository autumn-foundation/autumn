//! Seed binary for todo-app.
//!
//! Populates the database with representative todo items so the app looks
//! like a working product on first run. Safe to run multiple times — if any
//! todos already exist, the seed is a no-op.
//!
//! Run with:
//!   autumn migrate && autumn seed
//!
//! Or directly:
//!   cargo run --bin todo-app-seed

use autumn_web::seed::SeedContext;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

diesel::table! {
    todos (id) {
        id -> Int8,
        title -> Text,
        completed -> Bool,
        created_at -> Timestamp,
    }
}

#[derive(Insertable)]
#[diesel(table_name = todos)]
struct NewTodo<'a> {
    title: &'a str,
    completed: bool,
}

#[tokio::main]
async fn main() {
    let ctx = SeedContext::build().expect(
        "failed to build seed context — is the database running and AUTUMN_DATABASE__URL set?",
    );

    println!("Seeding todo-app (profile: {})...", ctx.profile());

    let mut db = ctx
        .conn()
        .await
        .expect("failed to acquire database connection");

    // Idempotency guard: skip if the table is already populated.
    // Match the error explicitly so "relation does not exist" (unapplied
    // migrations) produces a clear message instead of panicking at the INSERT.
    let existing_count: i64 = match todos::table.count().get_result(&mut *db).await {
        Ok(n) => n,
        Err(diesel::result::Error::DatabaseError(_, ref info))
            if info.message().contains("does not exist") =>
        {
            eprintln!(
                "error: table 'todos' does not exist — migrations have not been applied.\n\
                 Run:\n\
                 \n    autumn migrate && autumn seed\n\
                 \nIf you are using `autumn dev`, wait for the server to finish\n\
                 starting (it applies migrations on boot) then run `autumn seed`."
            );
            std::process::exit(1);
        }
        Err(e) => panic!("failed to count todos: {e}"),
    };

    if existing_count > 0 {
        println!(
            "Database already has {existing_count} todo(s); skipping seed. \
             Delete all rows to re-seed."
        );
        return;
    }

    let seed_todos = vec![
        NewTodo {
            title: "Buy groceries",
            completed: false,
        },
        NewTodo {
            title: "Write unit tests",
            completed: true,
        },
        NewTodo {
            title: "Read the Autumn docs",
            completed: false,
        },
        NewTodo {
            title: "Ship the feature",
            completed: false,
        },
        NewTodo {
            title: "Review pull requests",
            completed: true,
        },
    ];

    diesel::insert_into(todos::table)
        .values(&seed_todos)
        .execute(&mut *db)
        .await
        .expect("failed to insert seed todos");

    println!("Seeded {} todo(s) successfully.", seed_todos.len());
}
