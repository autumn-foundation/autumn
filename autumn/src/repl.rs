//! Interactive Rhai prompt for `autumn console --repl` (issue #2148).
//!
//! Enabled by the `repl` feature (off by default). `autumn console --repl`
//! turns it on for the playground run only, so Rhai never reaches a normal
//! build.
//!
//! `#[model]` and `#[repository]` register themselves here through
//! `inventory`. Each repository becomes a Rhai module with three reads:
//!
//! ```text
//! autumn> PostRepository::count()
//! 3
//! autumn> PostRepository::find_by_id(1)
//! { "id": 1, "title": "Hello" }
//! ```
//!
//! Rows reach the prompt as JSON: the same projection `Json(model)` sends.
//! A model with `#[classified]` columns shows its other columns only.

use std::fmt::Write as _;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use diesel_async::pooled_connection::deadpool::Pool;
use futures::future::BoxFuture;
use rhai::{Dynamic, Engine, EvalAltResult, Module, Scope};
use serde_json::Value;
use tokio::runtime::Handle;

use crate::db::RuntimeConnection;

/// The pool the prompt runs repository calls on.
pub type ReplPool = Pool<RuntimeConnection>;

/// A boxed repository call. The error is the message the prompt shows.
pub type ReplFuture<'a, T> = BoxFuture<'a, Result<T, String>>;

/// The prompt text.
pub const PROMPT: &str = "autumn> ";

/// The time limit for one repository call.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The history file, relative to the project directory.
pub const HISTORY_FILE: &str = "target/autumn/repl_history.txt";

/// A `#[model]` the prompt knows. One is registered per model.
pub struct ReplModel {
    /// The model type name, e.g. `"Post"`.
    pub name: &'static str,
    /// The database table.
    pub table: &'static str,
    /// The fields a row shows at the prompt.
    pub fields: &'static [&'static str],
}

inventory::collect!(ReplModel);

/// A `#[repository]` the prompt can call. One is registered per repository.
pub struct ReplRepository {
    /// The repository trait name, e.g. `"PostRepository"`. It is also the
    /// Rhai module name.
    pub name: &'static str,
    /// The model type name.
    pub model: &'static str,
    /// Read all rows.
    pub find_all: for<'a> fn(&'a ReplPool) -> ReplFuture<'a, Vec<Value>>,
    /// Read one row by id.
    pub find_by_id: for<'a> fn(&'a ReplPool, i64) -> ReplFuture<'a, Option<Value>>,
    /// Count the rows.
    pub count: for<'a> fn(&'a ReplPool) -> ReplFuture<'a, i64>,
}

inventory::collect!(ReplRepository);

/// Converts a row to the value the prompt shows.
///
/// `#[model]` implements it. Do not implement it by hand.
pub trait ReplRow {
    /// Returns the row as JSON.
    ///
    /// # Errors
    ///
    /// Returns the serializer message when a field does not serialize.
    fn to_repl_value(&self) -> Result<Value, String>;
}

/// Error from [`run`].
#[derive(Debug, thiserror::Error)]
pub enum ReplError {
    /// The line editor failed.
    #[error("line editor failed: {0}")]
    Editor(String),
    /// The prompt thread stopped unexpectedly.
    #[error("prompt thread stopped: {0}")]
    Thread(String),
}

/// All registered models, sorted by name.
#[must_use]
pub fn registered_models() -> Vec<&'static ReplModel> {
    let mut models: Vec<_> = inventory::iter::<ReplModel>.into_iter().collect();
    models.sort_unstable_by_key(|m| m.name);
    models.dedup_by_key(|m| m.name);
    models
}

/// All registered repositories, sorted by name.
#[must_use]
pub fn registered_repositories() -> Vec<&'static ReplRepository> {
    let mut repositories: Vec<_> = inventory::iter::<ReplRepository>.into_iter().collect();
    repositories.sort_unstable_by_key(|r| r.name);
    repositories.dedup_by_key(|r| r.name);
    repositories
}

/// Runs async repository calls from the synchronous Rhai engine.
///
/// Each call blocks on a runtime handle, with a time limit. A failure, a
/// time-out, or a panic becomes an error message. It never unwinds into the
/// engine.
pub struct Bridge {
    handle: Handle,
    pool: ReplPool,
    timeout: Duration,
}

impl Bridge {
    /// Makes a bridge with [`DEFAULT_CALL_TIMEOUT`].
    #[must_use]
    pub const fn new(handle: Handle, pool: ReplPool) -> Self {
        Self {
            handle,
            pool,
            timeout: DEFAULT_CALL_TIMEOUT,
        }
    }

    /// Sets the time limit for one call.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The pool calls run on.
    #[must_use]
    pub const fn pool(&self) -> &ReplPool {
        &self.pool
    }

    /// Blocks on `call` and returns its result.
    ///
    /// # Errors
    ///
    /// Returns a message when the call fails, times out, or panics.
    pub fn call<T>(&self, call: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        let timeout = self.timeout;
        // `block_on` panics inside an async context. `catch_unwind` turns that,
        // and a panic in the call, into a message.
        let blocked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.handle
                .block_on(async move { tokio::time::timeout(timeout, call).await })
        }));
        match blocked {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("call timed out after {}", humanize(timeout))),
            Err(payload) => Err(format!(
                "call panicked: {}",
                panic_message(payload.as_ref())
            )),
        }
    }
}

/// The result of one prompt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The rendered value.
    Value(String),
    /// The script error.
    Error(String),
    /// The user asked to stop.
    Exit,
    /// The line was blank.
    Empty,
}

/// The Rhai engine and its variables.
pub struct Repl {
    engine: Engine,
    scope: Scope<'static>,
}

impl Repl {
    /// Makes an engine with every registered repository.
    #[must_use]
    pub fn new(bridge: Bridge) -> Self {
        let bridge = Rc::new(bridge);
        let mut engine = Engine::new();
        for repository in registered_repositories() {
            engine.register_static_module(
                repository.name,
                repository_module(repository, &bridge).into(),
            );
        }
        engine.register_fn("help", help_text);
        engine.register_fn("models", models_value);
        engine.register_fn("repositories", repositories_value);
        Self {
            engine,
            scope: Scope::new(),
        }
    }

    /// The text shown when the prompt opens.
    #[must_use]
    pub fn banner(&self) -> String {
        let names: Vec<_> = registered_repositories().iter().map(|r| r.name).collect();
        let names = if names.is_empty() {
            "(none registered)".to_owned()
        } else {
            names.join(", ")
        };
        format!(
            "Autumn REPL (Rhai). Repositories: {names}.\n\
             Type help() for the commands, exit to stop."
        )
    }

    /// Evaluates one line. Variables stay for later lines.
    pub fn eval_line(&mut self, line: &str) -> Outcome {
        let line = line.trim();
        if line.is_empty() {
            return Outcome::Empty;
        }
        if matches!(line, "exit" | "quit" | ":q") {
            return Outcome::Exit;
        }
        match self
            .engine
            .eval_with_scope::<Dynamic>(&mut self.scope, line)
        {
            Ok(value) => Outcome::Value(render(&value)),
            Err(err) => Outcome::Error(err.to_string()),
        }
    }
}

/// Renders a value for the prompt: text as is, other values as JSON.
#[must_use]
pub fn render(value: &Dynamic) -> String {
    if value.is_unit() {
        return "()".to_owned();
    }
    if let Some(text) = value.read_lock::<rhai::ImmutableString>() {
        return text.to_string();
    }
    rhai::serde::from_dynamic::<Value>(value)
        .ok()
        .and_then(|json| serde_json::to_string_pretty(&json).ok())
        .unwrap_or_else(|| value.to_string())
}

/// Reads lines from `next_line` until it returns `None` or the user exits.
///
/// Values go to `out`; errors go to `err`.
///
/// # Errors
///
/// Returns the error from `next_line`.
pub fn drive(
    repl: &mut Repl,
    mut next_line: impl FnMut() -> Result<Option<String>, ReplError>,
    out: &mut impl std::io::Write,
    err: &mut impl std::io::Write,
) -> Result<(), ReplError> {
    while let Some(line) = next_line()? {
        // A closed pipe must not stop the prompt; the write result is dropped.
        match repl.eval_line(&line) {
            Outcome::Value(text) => {
                let _ = writeln!(out, "{text}");
            }
            Outcome::Error(text) => {
                let _ = writeln!(err, "error: {text}");
            }
            Outcome::Exit => break,
            Outcome::Empty => {}
        }
    }
    Ok(())
}

/// Opens the prompt on `pool` and returns when the user exits.
///
/// The prompt runs on a blocking thread. Repository calls block on the
/// current runtime handle.
///
/// # Errors
///
/// Returns [`ReplError`] when the line editor or the prompt thread fails.
pub async fn run(pool: &ReplPool) -> Result<(), ReplError> {
    let bridge = Bridge::new(Handle::current(), pool.clone());
    tokio::task::spawn_blocking(move || run_blocking(bridge))
        .await
        .map_err(|err| ReplError::Thread(err.to_string()))?
}

fn run_blocking(bridge: Bridge) -> Result<(), ReplError> {
    use rustyline::error::ReadlineError;

    let mut repl = Repl::new(bridge);
    let mut editor =
        rustyline::DefaultEditor::new().map_err(|err| ReplError::Editor(err.to_string()))?;
    let history = PathBuf::from(HISTORY_FILE);
    // History is a convenience: a missing or unreadable file is not an error.
    let _ = editor.load_history(&history);
    println!("{}", repl.banner());

    let result = drive(
        &mut repl,
        || loop {
            match editor.readline(PROMPT) {
                Ok(line) => {
                    let _ = editor.add_history_entry(line.as_str());
                    return Ok(Some(line));
                }
                // Ctrl-C clears the line; Ctrl-D stops.
                Err(ReadlineError::Interrupted) => {}
                Err(ReadlineError::Eof) => return Ok(None),
                Err(err) => return Err(ReplError::Editor(err.to_string())),
            }
        },
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );

    if let Some(dir) = history.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = editor.save_history(&history);
    result
}

/// Ends the process with the result of [`run`]. Used by
/// [`console_repl!`](crate::console_repl).
pub fn exit_with(result: Result<(), ReplError>) -> ! {
    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("autumn console: {err}");
            std::process::exit(1);
        }
    }
}

/// Converts one row. Picked by [`__Probe`].
#[doc(hidden)]
pub type __Projector<M> = fn(&M) -> Result<Value, String>;

// Autoref selection of the row projection for `#[repository]`. Call it on
// `&&&__Probe`. The methods take `&self`, so lookup tries the impl for
// `&&__Probe` first, then `&__Probe`, then `__Probe`: `ReplRow` (a `#[model]`)
// wins, then `Serialize` (a hand-written model), then an error.

#[doc(hidden)]
pub struct __Probe<M>(std::marker::PhantomData<M>);

impl<M> __Probe<M> {
    #[doc(hidden)]
    #[must_use]
    pub const fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

#[doc(hidden)]
pub trait __ViaRow<M> {
    fn __projector(&self) -> __Projector<M>;
}

impl<M: ReplRow> __ViaRow<M> for &&__Probe<M> {
    fn __projector(&self) -> __Projector<M> {
        M::to_repl_value
    }
}

#[doc(hidden)]
pub trait __ViaSerde<M> {
    fn __projector(&self) -> __Projector<M>;
}

impl<M: serde::Serialize> __ViaSerde<M> for &__Probe<M> {
    fn __projector(&self) -> __Projector<M> {
        |row| serde_json::to_value(row).map_err(|err| err.to_string())
    }
}

#[doc(hidden)]
pub trait __ViaNone<M> {
    fn __projector(&self) -> __Projector<M>;
}

impl<M> __ViaNone<M> for __Probe<M> {
    fn __projector(&self) -> __Projector<M> {
        |_| {
            Err(format!(
                "`{}` is not a `#[model]` and has no `Serialize` impl, so the REPL cannot show it",
                std::any::type_name::<M>()
            ))
        }
    }
}

#[doc(hidden)]
pub fn __project_all<M>(
    rows: crate::AutumnResult<Vec<M>>,
    project: __Projector<M>,
) -> Result<Vec<Value>, String> {
    rows.map_err(|err| err.to_string())?
        .iter()
        .map(project)
        .collect()
}

#[doc(hidden)]
pub fn __project_one<M>(
    row: crate::AutumnResult<Option<M>>,
    project: __Projector<M>,
) -> Result<Option<Value>, String> {
    row.map_err(|err| err.to_string())?
        .as_ref()
        .map(project)
        .transpose()
}

#[doc(hidden)]
pub fn __count(count: crate::AutumnResult<i64>) -> Result<i64, String> {
    count.map_err(|err| err.to_string())
}

/// Builds the Rhai module for one repository.
fn repository_module(repository: &'static ReplRepository, bridge: &Rc<Bridge>) -> Module {
    let mut module = Module::new();

    let b = Rc::clone(bridge);
    module.set_native_fn("find_all", move || {
        let rows = b
            .call((repository.find_all)(b.pool()))
            .map_err(script_error)?;
        to_dynamic(&rows)
    });

    let b = Rc::clone(bridge);
    module.set_native_fn("find_by_id", move |id: rhai::INT| {
        b.call((repository.find_by_id)(b.pool(), id))
            .map_err(script_error)?
            .map_or(Ok(Dynamic::UNIT), |row| to_dynamic(&row))
    });

    let b = Rc::clone(bridge);
    module.set_native_fn("count", move || {
        b.call((repository.count)(b.pool())).map_err(script_error)
    });

    module
}

fn to_dynamic(value: &impl serde::Serialize) -> Result<Dynamic, Box<EvalAltResult>> {
    rhai::serde::to_dynamic(value)
}

#[allow(
    clippy::unnecessary_box_returns,
    reason = "Rhai native functions return `Box<EvalAltResult>` errors"
)]
fn script_error(message: String) -> Box<EvalAltResult> {
    message.into()
}

fn help_text() -> String {
    let mut text = String::from(
        "Reads, one Rhai module per repository:\n\
         \x20 <Repository>::find_all()       all rows\n\
         \x20 <Repository>::find_by_id(id)   one row, or () when none\n\
         \x20 <Repository>::count()          the row count\n\
         Built-ins: help(), models(), repositories()\n\
         Type exit (or quit, :q, Ctrl-D) to stop.\n\
         Repositories:",
    );
    let repositories = registered_repositories();
    if repositories.is_empty() {
        text.push_str("\n  (none registered)");
    }
    for repository in repositories {
        let _ = write!(
            text,
            "\n  {name}::find_all()  {name}::find_by_id(id)  {name}::count()  ({model})",
            name = repository.name,
            model = repository.model,
        );
    }
    text
}

fn models_value() -> Result<Dynamic, Box<EvalAltResult>> {
    let models: Vec<Value> = registered_models()
        .into_iter()
        .map(|m| serde_json::json!({ "name": m.name, "table": m.table, "fields": m.fields }))
        .collect();
    to_dynamic(&models)
}

fn repositories_value() -> Result<Dynamic, Box<EvalAltResult>> {
    let repositories: Vec<Value> = registered_repositories()
        .into_iter()
        .map(|r| serde_json::json!({ "name": r.name, "model": r.model }))
        .collect();
    to_dynamic(&repositories)
}

fn humanize(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ── fixtures: registered without a database ────────────────────────
    fn fixture_rows() -> Vec<Value> {
        vec![
            serde_json::json!({ "id": 1, "title": "Hello" }),
            serde_json::json!({ "id": 2, "title": "World" }),
            serde_json::json!({ "id": 3, "title": "Again" }),
        ]
    }

    inventory::submit! {
        ReplModel { name: "ReplFixture", table: "repl_fixtures", fields: &["id", "title"] }
    }

    inventory::submit! {
        ReplRepository {
            name: "ReplFixtureRepository",
            model: "ReplFixture",
            find_all: |_pool| Box::pin(async { Ok(fixture_rows()) }),
            find_by_id: |_pool, id| Box::pin(async move {
                Ok(fixture_rows().into_iter().find(|r| r["id"] == id))
            }),
            count: |_pool| Box::pin(async { Ok(3) }),
        }
    }

    inventory::submit! {
        ReplRepository {
            name: "FailingFixtureRepository",
            model: "ReplFixture",
            find_all: |_pool| Box::pin(async { Err("connection refused".to_owned()) }),
            find_by_id: |_pool, _id| Box::pin(async { Err("connection refused".to_owned()) }),
            count: |_pool| Box::pin(async { Err("connection refused".to_owned()) }),
        }
    }

    inventory::submit! {
        ReplRepository {
            name: "PanickingFixtureRepository",
            model: "ReplFixture",
            find_all: |_pool| Box::pin(async { panic!("fixture panic") }),
            find_by_id: |_pool, _id| Box::pin(async { panic!("fixture panic") }),
            count: |_pool| Box::pin(async { panic!("fixture panic") }),
        }
    }

    inventory::submit! {
        ReplRepository {
            name: "SlowFixtureRepository",
            model: "ReplFixture",
            find_all: |_pool| Box::pin(async { std::future::pending().await }),
            find_by_id: |_pool, _id| Box::pin(async { std::future::pending().await }),
            count: |_pool| Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(0)
            }),
        }
    }

    fn lazy_pool() -> ReplPool {
        crate::db::create_pool(&crate::config::DatabaseConfig {
            primary_url: Some(crate::test_urls::primary("unused")),
            ..crate::config::DatabaseConfig::default()
        })
        .expect("pool builds")
        .expect("url present => Some(pool)")
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn with_repl<T>(timeout: Duration, f: impl FnOnce(&mut Repl) -> T) -> T {
        let rt = runtime();
        let bridge = Bridge::new(rt.handle().clone(), lazy_pool()).with_timeout(timeout);
        let mut repl = Repl::new(bridge);
        f(&mut repl)
    }

    fn eval(line: &str) -> Outcome {
        with_repl(DEFAULT_CALL_TIMEOUT, |repl| repl.eval_line(line))
    }

    fn error_text(outcome: Outcome) -> String {
        match outcome {
            Outcome::Error(text) => text,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    // ── registry ───────────────────────────────────────────────────────
    #[test]
    fn registry_collects_models_and_repositories() {
        assert!(registered_models().iter().any(|m| m.name == "ReplFixture"));
        let names: Vec<_> = registered_repositories().iter().map(|r| r.name).collect();
        assert!(names.contains(&"ReplFixtureRepository"), "{names:?}");
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "repositories are sorted by name");
    }

    // ── reads at the prompt ────────────────────────────────────────────
    #[test]
    fn count_is_callable_from_the_prompt() {
        assert_eq!(
            eval("ReplFixtureRepository::count()"),
            Outcome::Value("3".into())
        );
    }

    #[test]
    fn find_all_renders_rows_as_json() {
        let Outcome::Value(text) = eval("ReplFixtureRepository::find_all()") else {
            panic!("expected a value");
        };
        let rows: Value = serde_json::from_str(&text).expect("JSON output");
        assert_eq!(rows, Value::Array(fixture_rows()));
        assert!(text.contains('\n'), "output is pretty-printed: {text}");
    }

    #[test]
    fn find_by_id_returns_the_row_or_unit() {
        let Outcome::Value(text) = eval("ReplFixtureRepository::find_by_id(2)") else {
            panic!("expected a value");
        };
        let row: Value = serde_json::from_str(&text).expect("JSON output");
        assert_eq!(row["title"], "World");
        assert_eq!(
            eval("ReplFixtureRepository::find_by_id(99)"),
            Outcome::Value("()".into())
        );
    }

    #[test]
    fn rows_are_scriptable_values() {
        assert_eq!(
            eval("ReplFixtureRepository::find_all().len()"),
            Outcome::Value("3".into())
        );
        assert_eq!(
            eval("ReplFixtureRepository::find_by_id(1).title"),
            Outcome::Value("Hello".into())
        );
    }

    #[test]
    fn variables_stay_between_lines() {
        with_repl(DEFAULT_CALL_TIMEOUT, |repl| {
            assert_eq!(
                repl.eval_line("let n = ReplFixtureRepository::count();"),
                Outcome::Value("()".into())
            );
            assert_eq!(repl.eval_line("n + 1"), Outcome::Value("4".into()));
        });
    }

    // ── errors are script errors, never panics ─────────────────────────
    #[test]
    fn a_failed_call_is_a_script_error() {
        let text = error_text(eval("FailingFixtureRepository::count()"));
        assert!(text.contains("connection refused"), "{text}");
    }

    #[test]
    fn a_panicking_call_is_a_script_error() {
        let text = error_text(eval("PanickingFixtureRepository::find_all()"));
        assert!(
            text.contains("panicked") && text.contains("fixture panic"),
            "{text}"
        );
    }

    #[test]
    fn a_slow_call_times_out_as_a_script_error() {
        let outcome = with_repl(Duration::from_millis(50), |repl| {
            repl.eval_line("SlowFixtureRepository::count()")
        });
        assert!(error_text(outcome).contains("timed out"));
    }

    #[test]
    fn a_call_from_inside_the_runtime_is_a_script_error() {
        let rt = runtime();
        let handle = rt.handle().clone();
        let outcome = rt.block_on(async move {
            let mut repl = Repl::new(Bridge::new(handle, lazy_pool()));
            repl.eval_line("ReplFixtureRepository::count()")
        });
        assert!(matches!(outcome, Outcome::Error(_)), "{outcome:?}");
    }

    #[test]
    fn a_syntax_error_is_a_script_error() {
        assert!(matches!(eval("let = ;"), Outcome::Error(_)));
        assert!(matches!(
            eval("NoSuchRepository::count()"),
            Outcome::Error(_)
        ));
    }

    #[test]
    fn a_bad_id_type_is_a_script_error() {
        assert!(matches!(
            eval("ReplFixtureRepository::find_by_id(\"one\")"),
            Outcome::Error(_)
        ));
    }

    #[test]
    fn bridge_runs_a_future_to_completion() {
        let rt = runtime();
        let bridge = Bridge::new(rt.handle().clone(), lazy_pool());
        let ran = AtomicBool::new(false);
        let value = bridge.call(async {
            tokio::task::yield_now().await;
            ran.store(true, Ordering::SeqCst);
            Ok::<_, String>(7)
        });
        assert_eq!(value, Ok(7));
        assert!(ran.load(Ordering::SeqCst));
    }

    // ── built-ins and control lines ────────────────────────────────────
    #[test]
    fn exit_and_blank_lines() {
        for line in ["exit", "quit", " exit ", ":q"] {
            assert_eq!(eval(line), Outcome::Exit, "{line:?}");
        }
        assert_eq!(eval("   "), Outcome::Empty);
    }

    #[test]
    fn help_lists_the_repositories() {
        let Outcome::Value(text) = eval("help()") else {
            panic!("expected a value");
        };
        assert!(text.contains("ReplFixtureRepository::find_all()"), "{text}");
        assert!(text.contains("exit"), "{text}");
    }

    #[test]
    fn models_and_repositories_built_ins() {
        let Outcome::Value(models) = eval("models()") else {
            panic!("expected a value");
        };
        let models: Value = serde_json::from_str(&models).expect("JSON");
        assert!(
            models
                .as_array()
                .expect("array")
                .iter()
                .any(|m| m["name"] == "ReplFixture" && m["table"] == "repl_fixtures")
        );
        let Outcome::Value(repos) = eval("repositories()") else {
            panic!("expected a value");
        };
        assert!(repos.contains("\"ReplFixtureRepository\""), "{repos}");
    }

    #[test]
    fn banner_names_the_repositories() {
        let banner = with_repl(DEFAULT_CALL_TIMEOUT, |repl| repl.banner());
        assert!(banner.contains("ReplFixtureRepository"), "{banner}");
        assert!(banner.contains("help()"), "{banner}");
    }

    // ── rendering ──────────────────────────────────────────────────────
    #[test]
    fn render_shows_text_raw_and_the_rest_as_json() {
        assert_eq!(render(&Dynamic::from("plain")), "plain");
        assert_eq!(render(&Dynamic::UNIT), "()");
        assert_eq!(render(&Dynamic::from(42_i64)), "42");
        let map = rhai::serde::to_dynamic(serde_json::json!({ "a": 1 })).expect("map");
        assert_eq!(render(&map), "{\n  \"a\": 1\n}");
    }

    // ── the line loop ──────────────────────────────────────────────────
    #[test]
    fn drive_prints_values_and_errors_and_stops_at_exit() {
        let mut lines = vec!["40 + 2", "nope(", "", "exit", "7 * 6 + 1"]
            .into_iter()
            .map(str::to_owned);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        with_repl(DEFAULT_CALL_TIMEOUT, |repl| {
            drive(repl, || Ok(lines.next()), &mut out, &mut err).expect("drive");
        });
        let (out, err) = (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        );
        assert_eq!(out, "42\n");
        assert!(err.starts_with("error: "), "{err}");
        assert_eq!(
            lines.next().as_deref(),
            Some("7 * 6 + 1"),
            "stopped at exit"
        );
    }

    #[test]
    fn drive_stops_at_end_of_input() {
        let mut lines = vec!["1"].into_iter().map(str::to_owned);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        with_repl(DEFAULT_CALL_TIMEOUT, |repl| {
            drive(repl, || Ok(lines.next()), &mut out, &mut err).expect("drive");
        });
        assert_eq!(String::from_utf8(out).unwrap(), "1\n");
    }

    // ── projection helpers used by the macros ──────────────────────────
    struct Row(i64);
    impl ReplRow for Row {
        fn to_repl_value(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "id": self.0 }))
        }
    }

    #[derive(serde::Serialize)]
    struct SerdeOnly {
        id: i64,
    }

    struct Opaque;

    // Prefers `ReplRow` even when the type also has `Serialize`.
    #[derive(serde::Serialize)]
    struct Both {
        hidden: i64,
    }
    impl ReplRow for Both {
        fn to_repl_value(&self) -> Result<Value, String> {
            Ok(serde_json::json!({ "shown": true }))
        }
    }

    #[test]
    fn probe_picks_row_then_serde_then_an_error() {
        let row = (&&&__Probe::<Row>::new()).__projector();
        assert_eq!(row(&Row(1)), Ok(serde_json::json!({ "id": 1 })));
        let both = (&&&__Probe::<Both>::new()).__projector();
        assert_eq!(
            both(&Both { hidden: 1 }),
            Ok(serde_json::json!({ "shown": true }))
        );
        let serde_only = (&&&__Probe::<SerdeOnly>::new()).__projector();
        assert_eq!(
            serde_only(&SerdeOnly { id: 2 }),
            Ok(serde_json::json!({ "id": 2 }))
        );
        let opaque = (&&&__Probe::<Opaque>::new()).__projector();
        assert!(opaque(&Opaque).unwrap_err().contains("Opaque"));
    }

    #[test]
    fn projection_helpers_map_rows_and_errors() {
        let project: __Projector<Row> = Row::to_repl_value;
        assert_eq!(
            __project_all(Ok(vec![Row(1), Row(2)]), project),
            Ok(vec![
                serde_json::json!({"id": 1}),
                serde_json::json!({"id": 2})
            ])
        );
        assert_eq!(__project_one::<Row>(Ok(None), project), Ok(None));
        assert_eq!(
            __project_one(Ok(Some(Row(5))), project),
            Ok(Some(serde_json::json!({"id": 5})))
        );
        assert_eq!(__count(Ok(9)), Ok(9));
        let err = __count(Err(crate::AutumnError::not_found_msg("gone"))).unwrap_err();
        assert!(err.contains("gone"), "{err}");
    }
}
