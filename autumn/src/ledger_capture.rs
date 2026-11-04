//! Ledger capture: record every SQL statement the framework issues.
//!
//! Compiled only with the `ledger-capture` feature, and active only when
//! `AUTUMN_LEDGER_CAPTURE_DIR` names a directory. CI's Docker sweep runs with
//! both, and `scripts/check-perf-profiles.py ledger` compares the capture
//! against the per-query catalog in `docs/perf/ledger/queries/`. A statement
//! with no catalog entry fails that gate. See `docs/perf/README.md`.
//!
//! # What is recorded
//!
//! A diesel default [`Instrumentation`] sees every statement on every
//! connection established after it is installed. For each statement it
//! records:
//!
//! * the normalized text and its fingerprint (see [`normalize`]),
//! * the production call site: the innermost stack frame in a workspace
//!   `src/` file that is not test code (see [`Site`]),
//! * how many times the statement ran from that site.
//!
//! A statement with no production frame is a test fixture. It is not
//! recorded. Transaction control and `SET` are not recorded either.
//!
//! For a Postgres statement, the capture also opens a side connection to the
//! same database and runs `EXPLAIN (GENERIC_PLAN, COSTS OFF)` twice: once as
//! planned, and once with `enable_seqscan = off`. The second plan shows
//! whether any index can serve the statement. The statement itself never
//! runs twice. `GENERIC_PLAN` needs Postgres 16. When the test database is
//! older, the capture copies its schema with `pg_dump --schema-only` into a
//! database on the Postgres 16 server named by `AUTUMN_LEDGER_PLAN_URL`, and
//! plans there. Without that variable, such a statement has no plan.
//!
//! # Output
//!
//! Each process appends JSON lines to `capture-<pid>.jsonl` in the capture
//! directory. Record types are `site`, `plan` and `calls`. Call counts are
//! kept per thread and written when the thread exits. libtest runs each test
//! on its own thread named after the test, so a `calls` record names the test
//! that issued the statement.
//!
//! # Installation
//!
//! [`install`] makes the capture diesel's default instrumentation. Only
//! connections established after it are covered. The pool builders and the
//! test harness call it. The workspace forbids `unsafe`, so nothing can run
//! before `main`. The consolidated integration binary therefore carries a
//! test that sorts first and installs the capture, see
//! `autumn/tests/integration/mod.rs`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use diesel::connection::{Instrumentation, InstrumentationEvent};

/// The environment variable that turns the capture on.
pub const CAPTURE_DIR_ENV: &str = "AUTUMN_LEDGER_CAPTURE_DIR";

/// The workspace root, from this crate's manifest directory.
const WORKSPACE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

/// How often the capture retries the plan of one fingerprint.
///
/// A plan fails when the statement names a table that an open transaction
/// created, or when a lock blocks it. A later run of the same statement can
/// succeed.
const PLAN_ATTEMPTS: u32 = 3;

/// A production call site.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Site {
    /// The path of the source file, relative to the workspace root.
    pub file: String,
    /// The function, without its hash suffix and closure markers. Line
    /// tables give a closure no path, so this can be a bare name or empty.
    pub function: String,
    /// The line of the call. With `function` unqualified, the catalog reads
    /// the enclosing `fn` from the source at this line.
    pub line: u32,
}

struct State {
    out: File,
    /// Sites of each fingerprint that are already written.
    seen: HashSet<(String, Site)>,
    /// Plan attempts per fingerprint, and whether one succeeded.
    plans: HashMap<String, (u32, bool)>,
    /// Resolved stacks, keyed by their instruction pointers.
    stacks: HashMap<Vec<usize>, Option<Site>>,
    /// Server version of each test database, by URL.
    versions: HashMap<String, u32>,
    /// Schema snapshots taken of each old test database, by URL.
    mirrors: HashMap<String, u32>,
    /// Fingerprints already reported as unplannable on an old server.
    too_old: HashSet<String>,
}

static STATE: OnceLock<Option<Mutex<State>>> = OnceLock::new();

thread_local! {
    /// Set while the capture runs its own statements, so it does not record them.
    static BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn state() -> Option<&'static Mutex<State>> {
    STATE
        .get_or_init(|| {
            let dir = PathBuf::from(std::env::var_os(CAPTURE_DIR_ENV)?);
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join(format!("capture-{}.jsonl", std::process::id()));
            let out = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(State {
                out,
                seen: HashSet::new(),
                plans: HashMap::new(),
                stacks: HashMap::new(),
                versions: HashMap::new(),
                mirrors: HashMap::new(),
                too_old: HashSet::new(),
            }))
        })
        .as_ref()
}

/// Whether the capture is on in this process.
#[must_use]
pub fn enabled() -> bool {
    state().is_some()
}

/// Install the capture as diesel's default instrumentation.
///
/// Does nothing unless [`CAPTURE_DIR_ENV`] is set. Connections established
/// before this call are not covered.
pub fn install() {
    if enabled() {
        let _ = diesel::connection::set_default_instrumentation(default_instrumentation);
    }
}

#[allow(clippy::unnecessary_wraps)] // diesel fixes this signature.
fn default_instrumentation() -> Option<Box<dyn Instrumentation>> {
    Some(Box::new(CaptureInstrumentation::default()))
}

/// Call counts of one thread, written when the thread exits.
#[derive(Default)]
struct ThreadCalls(HashMap<(String, Site), u64>);

impl Drop for ThreadCalls {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let Some(state) = state() else { return };
        let Ok(mut s) = state.lock() else { return };
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("");
        let mut calls: Vec<_> = self.0.drain().collect();
        calls.sort();
        let mut lines = String::new();
        for ((fp, site), n) in calls {
            let line = serde_json::json!({
                "type": "calls",
                "fingerprint": fp,
                "file": site.file,
                "function": site.function,
                "line": site.line,
                "thread": test,
                "calls": n,
            });
            lines.push_str(&line.to_string());
            lines.push('\n');
        }
        let _ = s.out.write_all(lines.as_bytes());
        let _ = s.out.flush();
    }
}

thread_local! {
    static CALLS: RefCell<ThreadCalls> = RefCell::new(ThreadCalls::default());
}

/// The per-connection instrumentation. It remembers the connection URL so a
/// plan can use a side connection to the same database.
#[derive(Default)]
struct CaptureInstrumentation {
    url: Option<String>,
}

impl Instrumentation for CaptureInstrumentation {
    fn on_connection_event(&mut self, event: InstrumentationEvent<'_>) {
        match event {
            InstrumentationEvent::StartEstablishConnection { url, .. } => {
                self.url = Some(url.to_owned());
            }
            InstrumentationEvent::StartQuery { query, .. } => {
                observe(&query.to_string(), self.url.as_deref());
            }
            _ => {}
        }
    }
}

/// Record one statement. Instrumentation that replaces the capture on a
/// connection forwards to this, with no URL.
pub(crate) fn observe(sql: &str, url: Option<&str>) {
    if BUSY.with(std::cell::Cell::get) {
        return;
    }
    let Some(state) = state() else { return };
    let Some(normalized) = normalize(sql) else {
        return;
    };
    BUSY.with(|b| b.set(true));
    let _reset = BusyReset;
    let mut ips = Vec::with_capacity(64);
    backtrace::trace(|frame| {
        ips.push(frame.ip() as usize);
        true
    });
    let Ok(mut s) = state.lock() else { return };
    let site = s
        .stacks
        .entry(ips.clone())
        .or_insert_with(|| resolve_site(&ips))
        .clone();
    let Some(site) = site else { return };
    let fp = fingerprint(&normalized);
    let _ = CALLS.try_with(|c| {
        *c.borrow_mut()
            .0
            .entry((fp.clone(), site.clone()))
            .or_insert(0) += 1;
    });
    if s.seen.insert((fp.clone(), site.clone())) {
        let line = serde_json::json!({
            "type": "site",
            "fingerprint": fp,
            "statement": normalized,
            "kind": kind(&normalized),
            "backend": backend(url),
            "file": site.file,
            "function": site.function,
            "line": site.line,
        });
        let _ = writeln!(s.out, "{line}");
    }
    if let Some(url) = url
        && backend(Some(url)) == "postgres"
        && plannable(&normalized)
    {
        plan(&mut s, &fp, sql, url);
    }
}

struct BusyReset;

impl Drop for BusyReset {
    fn drop(&mut self) {
        BUSY.with(|b| b.set(false));
    }
}

fn backend(url: Option<&str>) -> &'static str {
    match url {
        Some(u) if u.starts_with("postgres://") || u.starts_with("postgresql://") => "postgres",
        Some(u) if u.starts_with("mysql://") => "mysql",
        Some(_) => "sqlite",
        None => "unknown",
    }
}

/// Find the innermost production frame of a stack.
///
/// One instruction pointer can resolve to several frames when calls were
/// inlined. Each one counts, innermost first.
fn resolve_site(ips: &[usize]) -> Option<Site> {
    let root = std::fs::canonicalize(WORKSPACE_ROOT).ok()?;
    let mut frames: Vec<(String, String, u32)> = Vec::new();
    for &ip in ips {
        backtrace::resolve(ip as *mut std::ffi::c_void, |symbol| {
            let (Some(file), Some(name)) = (symbol.filename(), symbol.name()) else {
                return;
            };
            let Ok(rel) = file.strip_prefix(&root) else {
                return;
            };
            // `{:#}` drops the crate hashes of v0 mangling.
            frames.push((
                rel.to_string_lossy().into_owned(),
                clean_symbol(&format!("{name:#}")),
                symbol.lineno().unwrap_or(0),
            ));
        });
    }
    let (i, (file, function, line)) = frames
        .iter()
        .enumerate()
        .find(|(_, (file, function, _))| !is_capture_plumbing(file, function))?;
    // The innermost workspace frame decides. Test code means a fixture.
    if is_test_code(file, function) {
        return None;
    }
    // Line tables name a closure or an inlined call without its path. The
    // next qualified frame in the same file is the function that holds it.
    let function = if function.contains("::") {
        function.clone()
    } else {
        frames[i..]
            .iter()
            .find(|(f, name, _)| f == file && name.contains("::"))
            .map_or_else(|| function.clone(), |(_, name, _)| name.clone())
    };
    Some(Site {
        file: file.clone(),
        function,
        line: *line,
    })
}

fn is_capture_plumbing(file: &str, function: &str) -> bool {
    file.ends_with("src/ledger_capture.rs")
        || function.contains("on_connection_event")
        || function.contains("RequestQueryTimer")
        || function.ends_with("::on_start")
}

fn is_test_code(file: &str, function: &str) -> bool {
    let in_dir = |d: &str| file.starts_with(d) || file.contains(&format!("/{d}"));
    in_dir("tests/")
        || in_dir("benches/")
        || !file.contains("src/")
        || file.ends_with("src/test.rs")
        || file.contains("/test_support")
        || file.contains("/testing/")
        || file.ends_with("/tests.rs")
        || function.contains("::tests::")
        || function.contains("::test::")
        || function.contains("::test_support")
        || function.contains("::testing::")
        || function.starts_with("autumn_web::test::")
}

/// Drop the hash suffix, generic arguments and closure markers from a symbol.
///
/// `<Type as Trait>::method` becomes `Type::method`.
fn clean_symbol(raw: &str) -> String {
    let mut s = raw.to_owned();
    if let Some(i) = s.rfind("::h")
        && s[i + 3..].len() == 16
        && s[i + 3..].chars().all(|c| c.is_ascii_hexdigit())
    {
        s.truncate(i);
    }
    let mut s = s.replace("::{{closure}}", "");
    if s.starts_with('<')
        && let Some(close) = matching_angle(s.as_bytes())
    {
        let inner = &s[1..close];
        let self_ty = inner.split(" as ").next().unwrap_or(inner);
        s = format!("{self_ty}{}", &s[close + 1..]);
    }
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // v0 mangling marks closures and async bodies as `{closure#0}` segments.
    out.split("::")
        .filter(|seg| !seg.is_empty() && !seg.starts_with('{'))
        .collect::<Vec<_>>()
        .join("::")
}

/// The index of the `>` that closes the `<` at index 0.
fn matching_angle(b: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    for (k, &c) in b.iter().enumerate() {
        match c {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// Normalize a statement the way `pg_stat_statements` would, and more.
///
/// * Whitespace collapses to one space.
/// * String and number literals become `?`. Bind parameters become `$?`.
/// * A run of two or more `$?` becomes `$?, ...`, and a run of equal tuples
///   becomes one tuple and `...`. A batch insert of 3 rows and one of 30 rows
///   are then one statement.
///
/// Returns `None` for transaction control, `SET` and empty text, which are
/// not statements a catalog profiles.
#[must_use]
pub fn normalize(sql: &str) -> Option<String> {
    let text = strip_comments(sql);
    let mut tokens = text.split_whitespace();
    let first = tokens.next()?.to_ascii_uppercase();
    let second = tokens.next().map(str::to_ascii_uppercase);
    let skip = matches!(
        first.as_str(),
        "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "SET" | "RESET" | "END"
    ) || (first == "START" && second.as_deref() == Some("TRANSACTION"));
    if skip {
        return None;
    }
    let lexed = lex(&text);
    Some(collapse_tuples(&collapse_params(&lexed)))
}

/// Remove `--` and `/* */` comments outside of string literals.
fn strip_comments(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                let end = skip_string(b, i);
                out.push_str(&sql[i..end]);
                i = end;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out.push(' ');
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out.push(' ');
            }
            _ => {
                let ch = sql[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// The index just past the string literal that starts at `start`.
fn skip_string(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// Replace literals and parameters, and collapse whitespace.
fn lex(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut space = false;
    let push = |out: &mut String, s: &str, space: &mut bool| {
        if *space && !out.is_empty() {
            out.push(' ');
        }
        *space = false;
        out.push_str(s);
    };
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            space = true;
            i += 1;
        } else if c == b',' {
            // One spelling of every separator, so `(1,2)` and `(1, 2)` lex
            // alike and the list collapse sees both.
            out.push(',');
            space = true;
            i += 1;
        } else if c == b'\'' {
            i = skip_string(b, i);
            push(&mut out, "?", &mut space);
        } else if c == b'$' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            push(&mut out, "$?", &mut space);
        } else if c.is_ascii_digit() && !prev_is_ident(&out, space) {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            push(&mut out, "?", &mut space);
        } else if c == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            push(&mut out, &sql[start..i], &mut space);
        } else {
            let ch = sql[i..].chars().next().unwrap_or(' ');
            let mut buf = [0u8; 4];
            push(&mut out, ch.encode_utf8(&mut buf), &mut space);
            i += ch.len_utf8();
        }
    }
    out
}

/// Whether a digit here continues an identifier, as in `t0` or `int8`.
fn prev_is_ident(out: &str, space: bool) -> bool {
    !space
        && out
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Collapse a run of two or more `$?` or `?` items into `$?, ...`.
fn collapse_params(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        let starts_item = i == 0 || matches!(b[i - 1], b'(' | b' ' | b',');
        if let (true, Some(len)) = (starts_item, param_at(b, i)) {
            let mut end = i + len;
            let mut items = 1;
            while b[end..].starts_with(b", ") {
                let Some(next) = param_at(b, end + 2) else {
                    break;
                };
                end += 2 + next;
                items += 1;
            }
            if items >= 2 {
                out.push_str(&s[i..i + len]);
                out.push_str(", ...");
                i = end;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The length of the `$?` or `?` item at `i`, when one ends there.
fn param_at(b: &[u8], i: usize) -> Option<usize> {
    let len = if b[i..].starts_with(b"$?") {
        2
    } else if b.get(i) == Some(&b'?') {
        1
    } else {
        return None;
    };
    match b.get(i + len) {
        None | Some(b',' | b')' | b' ') => Some(len),
        _ => None,
    }
}

/// Collapse `(a), (a), (a)` into `(a), ...`.
fn collapse_tuples(s: &str) -> String {
    let mut out = s.to_owned();
    loop {
        let Some(next) = collapse_one_tuple_run(&out) else {
            return out;
        };
        out = next;
    }
}

fn collapse_one_tuple_run(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'(' {
            let end = matching_paren(b, i)?;
            let tuple = &s[i..=end];
            let sep = format!(", {tuple}");
            let mut j = end + 1;
            let mut repeats = 0;
            while s[j..].starts_with(&sep) {
                j += sep.len();
                repeats += 1;
            }
            if repeats > 0 && !s[j..].starts_with(", ...") {
                return Some(format!("{}{tuple}, ...{}", &s[..i], &s[j..]));
            }
        }
        i += 1;
    }
    None
}

fn matching_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, &c) in b.iter().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// The first 16 hex digits of the SHA-256 of a normalized statement.
#[must_use]
pub fn fingerprint(normalized: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(normalized.as_bytes());
    hex::encode(&digest[..8])
}

/// A coarse statement kind, from the first keyword.
///
/// A multi-statement batch is `batch`, unless it starts with DDL: a migration
/// is `ddl`.
#[must_use]
pub fn kind(normalized: &str) -> &'static str {
    let first = normalized
        .trim_start_matches('(')
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let single = match first.as_str() {
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" => "read",
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY" | "TRUNCATE" => "write",
        "CREATE" | "ALTER" | "DROP" | "COMMENT" | "GRANT" | "REVOKE" | "REINDEX" | "CLUSTER" => {
            "ddl"
        }
        "LISTEN" | "UNLISTEN" | "NOTIFY" | "LOCK" | "VACUUM" | "ANALYZE" | "DO" | "CALL"
        | "DISCARD" | "PREPARE" | "EXECUTE" | "DEALLOCATE" => "utility",
        _ => "other",
    };
    if single != "ddl" && normalized.trim_end_matches(';').contains("; ") {
        return "batch";
    }
    single
}

fn plannable(normalized: &str) -> bool {
    matches!(kind(normalized), "read" | "write")
        && !normalized.starts_with("COPY")
        && !normalized.starts_with("TRUNCATE")
}

/// Explain a statement on a side connection, once per fingerprint.
fn plan(s: &mut State, fp: &str, sql: &str, url: &str) {
    let entry = s.plans.entry(fp.to_owned()).or_insert((0, false));
    if entry.1 || entry.0 >= PLAN_ATTEMPTS {
        return;
    }
    let target = match plan_target(s, fp, url, false) {
        Ok(Some(target)) => Ok(target),
        Ok(None) => return,
        Err(e) => Err(e),
    };
    if let Some(e) = s.plans.get_mut(fp) {
        e.0 += 1;
    }
    let (mut planned, mut forced) = match &target {
        Ok(t) => explain_both(&t.url, sql),
        Err(e) => (Err(e.clone()), Err(e.clone())),
    };
    // A mirror is a snapshot of the schema. A table the test created after
    // the snapshot is missing from it, so take a new one and try again.
    if target.as_ref().is_ok_and(|t| t.mirrored)
        && missing_relation(&planned)
        && let Ok(Some(fresh)) = plan_target(s, fp, url, true)
    {
        (planned, forced) = explain_both(&fresh.url, sql);
    }
    let ok = planned.is_ok() && forced.is_ok();
    if let Some(e) = s.plans.get_mut(fp) {
        e.1 = ok;
    }
    if !ok && s.plans.get(fp).is_some_and(|e| e.0 < PLAN_ATTEMPTS) {
        return;
    }
    let (plan, plan_error) = split(planned);
    let (forced, forced_error) = split(forced);
    let line = serde_json::json!({
        "type": "plan",
        "fingerprint": fp,
        "planned_on": target.ok().map(|t| t.label),
        "plan": plan,
        "plan_error": plan_error,
        "plan_index_forced": forced,
        "plan_index_forced_error": forced_error,
    });
    let _ = writeln!(s.out, "{line}");
}

/// Where a plan runs: the test database itself, or a schema mirror of it.
struct PlanTarget {
    url: String,
    mirrored: bool,
    label: String,
}

/// The environment variable naming a Postgres 16+ server for plans of
/// statements whose test database is older.
pub const PLAN_URL_ENV: &str = "AUTUMN_LEDGER_PLAN_URL";

/// How many schema snapshots one test database may take.
const MIRROR_REFRESHES: u32 = 8;

/// Pick the server to plan on. `GENERIC_PLAN` needs Postgres 16. An older
/// test database is mirrored into the server named by [`PLAN_URL_ENV`].
///
/// `Ok(None)` means no plan and nothing more to record. `Err` is a failed
/// attempt, with its reason.
fn plan_target(
    s: &mut State,
    fp: &str,
    url: &str,
    refresh: bool,
) -> Result<Option<PlanTarget>, String> {
    let version = if let Some(v) = s.versions.get(url) {
        *v
    } else {
        let v = server_version(url).ok_or("could not read the test server's version")?;
        s.versions.insert(url.to_owned(), v);
        v
    };
    if version >= 160_000 {
        return Ok(Some(PlanTarget {
            url: url.to_owned(),
            mirrored: false,
            label: format!("test database, Postgres {}", version / 10_000),
        }));
    }
    let Some(plan_url) = std::env::var(PLAN_URL_ENV).ok() else {
        if s.too_old.insert(fp.to_owned()) {
            let line = serde_json::json!({
                "type": "plan",
                "fingerprint": fp,
                "plan": null,
                "plan_error": format!(
                    "the test database is Postgres {}; GENERIC_PLAN needs 16, and {PLAN_URL_ENV} is not set",
                    version / 10_000
                ),
            });
            let _ = writeln!(s.out, "{line}");
        }
        return Ok(None);
    };
    let refreshes = s.mirrors.get(url).copied();
    let mirror =
        mirror_url(&plan_url, url).ok_or_else(|| format!("{PLAN_URL_ENV} is not a URL"))?;
    if refresh || refreshes.is_none() {
        let n = refreshes.unwrap_or(0);
        if n >= MIRROR_REFRESHES {
            return Ok(None);
        }
        s.mirrors.insert(url.to_owned(), n + 1);
        copy_schema(&plan_url, &mirror, url)?;
    }
    Ok(Some(PlanTarget {
        url: mirror,
        mirrored: true,
        label: format!(
            "schema mirror of a Postgres {} test database",
            version / 10_000
        ),
    }))
}

fn server_version(url: &str) -> Option<u32> {
    use diesel::Connection as _;
    use diesel::RunQueryDsl as _;
    let mut conn = diesel::PgConnection::establish(url).ok()?;
    let rows = diesel::sql_query("SELECT current_setting('server_version_num') AS line")
        .load::<PlanLine>(&mut conn)
        .ok()?;
    rows.first()?.0.parse().ok()
}

/// The URL of the mirror database of `test_url` on the plan server.
fn mirror_url(plan_url: &str, test_url: &str) -> Option<String> {
    let mut u = url::Url::parse(plan_url).ok()?;
    u.set_path(&format!("/ledger_mirror_{}", fingerprint(test_url)));
    Some(u.to_string())
}

/// Recreate the mirror database and copy the test database's schema into it.
fn copy_schema(plan_url: &str, mirror: &str, test_url: &str) -> Result<(), String> {
    use diesel::Connection as _;
    use diesel::connection::SimpleConnection as _;
    use std::process::{Command, Stdio};
    let name = mirror.rsplit('/').next().unwrap_or_default().to_owned();
    let mut admin = diesel::PgConnection::establish(plan_url)
        .map_err(|e| format!("could not connect to {PLAN_URL_ENV}: {e}"))?;
    // Two statements: neither may run inside a transaction block, and one
    // multi-statement string is one implicit transaction.
    admin
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .and_then(|()| admin.batch_execute(&format!("CREATE DATABASE \"{name}\"")))
        .map_err(|e| format!("could not create the schema mirror: {e}"))?;
    drop(admin);
    // The dump never waits on a lock the test holds: the test is blocked
    // until this returns.
    let mut dump = Command::new("pg_dump")
        .args([
            "--schema-only",
            "--no-owner",
            "--no-privileges",
            "--lock-wait-timeout=200",
            test_url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run pg_dump: {e}"))?;
    let stdout = dump.stdout.take().ok_or("pg_dump has no stdout")?;
    let loaded = Command::new("psql")
        .args(["-X", "-q", "-v", "ON_ERROR_STOP=0", mirror])
        .stdin(stdout)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("could not run psql: {e}"))?;
    let dumped = dump.wait().map_err(|e| format!("pg_dump: {e}"))?;
    if !dumped.success() {
        return Err(format!("pg_dump of the test database failed: {dumped}"));
    }
    if !loaded.success() {
        return Err(format!("psql could not load the schema mirror: {loaded}"));
    }
    Ok(())
}

fn missing_relation(r: &Result<Vec<String>, String>) -> bool {
    r.as_ref().is_err_and(|e| e.contains("does not exist"))
}

fn explain_both(
    url: &str,
    sql: &str,
) -> (Result<Vec<String>, String>, Result<Vec<String>, String>) {
    // One short-lived connection per plan. A pooled one would hold the test
    // database open, and a test that drops its database would then fail.
    let Some(mut conn) = side_connection(url) else {
        let e = Err("could not open a side connection".to_owned());
        return (e.clone(), e);
    };
    (
        explain(&mut conn, sql, false),
        explain(&mut conn, sql, true),
    )
}

fn split(r: Result<Vec<String>, String>) -> (Option<Vec<String>>, Option<String>) {
    match r {
        Ok(v) => (Some(v), None),
        Err(e) => (None, Some(e)),
    }
}

fn side_connection(url: &str) -> Option<diesel::PgConnection> {
    use diesel::Connection as _;
    use diesel::connection::SimpleConnection as _;
    let mut conn = diesel::PgConnection::establish(url).ok()?;
    // A plan never waits on the transaction it is measuring.
    conn.batch_execute(
        "SET lock_timeout = '100ms'; SET statement_timeout = '5s'; \
         SET application_name = 'autumn-ledger-capture'",
    )
    .ok()?;
    // A test that reads pg_stat_statements must not see the plans. Setting
    // it needs a superuser, which every test database has. Elsewhere the
    // plans are visible, which is harmless.
    let _ = conn.batch_execute("SET pg_stat_statements.track = 'none'");
    // `GENERIC_PLAN` with `$n` placeholders needs the simple query protocol,
    // and diesel's `sql_query` binds through the extended one. A dynamic
    // EXECUTE in plpgsql parses the statement the simple way. The function
    // lives in this session's `pg_temp` schema and goes with the connection.
    conn.batch_execute(
        "CREATE FUNCTION pg_temp.autumn_ledger_explain(q text) RETURNS SETOF text \
         LANGUAGE plpgsql AS $f$ BEGIN RETURN QUERY EXECUTE q; END $f$",
    )
    .ok()?;
    Some(conn)
}

/// One `EXPLAIN` output line.
struct PlanLine(String);

impl diesel::QueryableByName<diesel::pg::Pg> for PlanLine {
    fn build<'a>(
        row: &impl diesel::row::NamedRow<'a, diesel::pg::Pg>,
    ) -> diesel::deserialize::Result<Self> {
        Ok(Self(diesel::row::NamedRow::get::<
            diesel::sql_types::Text,
            String,
        >(row, "line")?))
    }
}

fn explain(
    conn: &mut diesel::PgConnection,
    sql: &str,
    force_index: bool,
) -> Result<Vec<String>, String> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;
    let toggle = if force_index { "off" } else { "on" };
    conn.batch_execute(&format!("SET enable_seqscan = {toggle}"))
        .map_err(|e| e.to_string())?;
    let rows = diesel::sql_query("SELECT pg_temp.autumn_ledger_explain($1) AS line")
        .bind::<diesel::sql_types::Text, _>(format!("EXPLAIN (GENERIC_PLAN, COSTS OFF) {sql}"))
        .load::<PlanLine>(conn)
        .map_err(|e| e.to_string());
    let _ = conn.batch_execute("RESET enable_seqscan");
    Ok(rows?.into_iter().map(|l| l.0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_control_and_set_are_not_statements() {
        for sql in [
            "BEGIN",
            "commit",
            "ROLLBACK TO SAVEPOINT s",
            "SET statement_timeout = 5",
            "START TRANSACTION",
            "  ",
        ] {
            assert_eq!(normalize(sql), None, "{sql}");
        }
    }

    #[test]
    fn literals_and_params_are_replaced() {
        let n = normalize("SELECT  a, 'x''y' FROM t0 WHERE id = $1 AND n > 42\n LIMIT 10").unwrap();
        assert_eq!(n, "SELECT a, ? FROM t0 WHERE id = $? AND n > ? LIMIT ?");
    }

    #[test]
    fn identifiers_keep_their_digits() {
        let n = normalize("SELECT col2::int8 FROM \"t 1\"").unwrap();
        assert_eq!(n, "SELECT col2::int8 FROM \"t 1\"");
    }

    #[test]
    fn batch_inserts_of_any_size_share_a_fingerprint() {
        let three = normalize("INSERT INTO t (a, b) VALUES ($1, $2), ($3, $4), ($5, $6)").unwrap();
        let one = normalize("INSERT INTO t (a, b) VALUES ($1, $2)").unwrap();
        let thirty: String = (0..30)
            .map(|i| format!("(${}, ${})", 2 * i + 1, 2 * i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let thirty = normalize(&format!("INSERT INTO t (a, b) VALUES {thirty}")).unwrap();
        assert_eq!(three, thirty);
        assert_eq!(fingerprint(&three), fingerprint(&thirty));
        assert_eq!(three, "INSERT INTO t (a, b) VALUES ($?, ...), ...");
        assert_eq!(one, "INSERT INTO t (a, b) VALUES ($?, ...)");
    }

    #[test]
    fn in_lists_collapse() {
        let a = normalize("SELECT * FROM t WHERE id IN ($1, $2, $3)").unwrap();
        let b = normalize("SELECT * FROM t WHERE id IN ($1, $2)").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "SELECT * FROM t WHERE id IN ($?, ...)");
    }

    #[test]
    fn lists_without_spaces_collapse_like_spaced_ones() {
        let tight = normalize("SELECT * FROM t WHERE id IN (1,2)").unwrap();
        let three = normalize("SELECT * FROM t WHERE id IN (1,2,3)").unwrap();
        let spaced = normalize("SELECT * FROM t WHERE id IN (1 , 2, 3)").unwrap();
        assert_eq!(tight, three);
        assert_eq!(tight, spaced);
        assert_eq!(tight, "SELECT * FROM t WHERE id IN (?, ...)");
    }

    #[test]
    fn normalizing_twice_changes_nothing() {
        for sql in [
            "SELECT a, 'x' FROM t0 WHERE id IN (1,2,3) AND n = $1",
            "INSERT INTO t (a, b) VALUES ($1, $2), ($3, $4)",
            "UPDATE t SET n = n + 1 WHERE id = ANY($1) -- tail",
        ] {
            let once = normalize(sql).unwrap();
            assert_eq!(normalize(&once).unwrap(), once, "{sql}");
        }
    }

    #[test]
    fn single_param_is_kept() {
        assert_eq!(
            normalize("SELECT * FROM t WHERE id = ANY($1)").unwrap(),
            "SELECT * FROM t WHERE id = ANY($?)"
        );
    }

    #[test]
    fn comments_are_dropped() {
        let n = normalize("-- lead\nSELECT /* x */ 1 -- tail").unwrap();
        assert_eq!(n, "SELECT ?");
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(fingerprint("SELECT ?"), fingerprint("SELECT ?"));
        assert_eq!(fingerprint("SELECT ?").len(), 16);
        assert_ne!(fingerprint("SELECT ?"), fingerprint("SELECT $?"));
    }

    #[test]
    fn kinds() {
        assert_eq!(kind("SELECT ?"), "read");
        assert_eq!(kind("WITH a AS (SELECT ?) UPDATE t SET x = ?"), "read");
        assert_eq!(kind("INSERT INTO t VALUES ($?)"), "write");
        assert_eq!(kind("CREATE TABLE t (id int)"), "ddl");
        assert_eq!(kind("LISTEN chan"), "utility");
        assert_eq!(
            kind("CREATE TABLE a (id int); CREATE TABLE b (id int);"),
            "ddl"
        );
        assert_eq!(kind("DELETE FROM a; DELETE FROM b"), "batch");
    }

    #[test]
    fn symbols_lose_hashes_generics_and_closures() {
        assert_eq!(
            clean_symbol(
                "autumn_web::job::claim::<integration_tests::Foo>::{{closure}}::h0123456789abcdef"
            ),
            "autumn_web::job::claim"
        );
        assert_eq!(
            clean_symbol("<autumn_web::db::PgStore as autumn_web::Store>::load::{{closure}}"),
            "autumn_web::db::PgStore::load"
        );
    }

    /// The plan path against a real server. Needs Postgres 16 or later:
    /// `LEDGER_CAPTURE_TEST_PG_URL=postgres://postgres@127.0.0.1:5432/postgres`.
    #[test]
    #[ignore = "requires Postgres 16 (LEDGER_CAPTURE_TEST_PG_URL)"]
    fn generic_plans_show_whether_an_index_serves_a_statement() {
        use diesel::connection::SimpleConnection as _;
        let url = std::env::var("LEDGER_CAPTURE_TEST_PG_URL").expect("LEDGER_CAPTURE_TEST_PG_URL");
        let mut setup = side_connection(&url).expect("connect");
        setup
            .batch_execute(
                "DROP TABLE IF EXISTS ledger_capture_t; \
                 CREATE TABLE ledger_capture_t (id bigint PRIMARY KEY, tag text)",
            )
            .expect("schema");
        let mut conn = side_connection(&url).expect("connect");
        let by_id = explain(
            &mut conn,
            "SELECT tag FROM ledger_capture_t WHERE id = $1",
            true,
        )
        .expect("plan");
        assert!(by_id.iter().any(|l| l.contains("Index Scan")), "{by_id:?}");
        let by_tag = explain(
            &mut conn,
            "SELECT id FROM ledger_capture_t WHERE tag = $1",
            true,
        )
        .expect("plan");
        assert!(
            by_tag
                .iter()
                .any(|l| l.contains("Seq Scan on ledger_capture_t")),
            "{by_tag:?}"
        );
        let write = explain(
            &mut conn,
            "UPDATE ledger_capture_t SET tag = $1 WHERE id = $2",
            false,
        )
        .expect("plan");
        assert!(
            write
                .iter()
                .any(|l| l.contains("Update on ledger_capture_t")),
            "{write:?}"
        );
        let missing = explain(&mut conn, "SELECT * FROM ledger_capture_nope", false);
        assert!(missing.is_err());
        setup
            .batch_execute("DROP TABLE ledger_capture_t")
            .expect("cleanup");
    }

    #[test]
    fn v0_closure_segments_are_dropped() {
        assert_eq!(
            clean_symbol("<autumn_web::job::Store>::claim::{closure#0}"),
            "autumn_web::job::Store::claim"
        );
        assert_eq!(
            clean_symbol("autumn_web::{impl#3}::run::{async_fn_env#0}"),
            "autumn_web::run"
        );
    }

    #[test]
    fn mirror_urls_are_per_test_database() {
        let a = mirror_url(
            "postgres://u@plan:5432/postgres",
            "postgres://p@127.0.0.1:1111/postgres",
        )
        .unwrap();
        let b = mirror_url(
            "postgres://u@plan:5432/postgres",
            "postgres://p@127.0.0.1:2222/postgres",
        )
        .unwrap();
        assert_ne!(a, b);
        assert!(
            a.starts_with("postgres://u@plan:5432/ledger_mirror_"),
            "{a}"
        );
    }

    #[test]
    fn test_code_is_recognized() {
        assert!(is_test_code(
            "autumn/tests/integration/x.rs",
            "integration_tests::x::run"
        ));
        assert!(is_test_code(
            "autumn/src/job.rs",
            "autumn_web::job::tests::claim"
        ));
        assert!(is_test_code(
            "autumn/src/test.rs",
            "autumn_web::test::TestApp::build"
        ));
        assert!(!is_test_code("autumn/src/job.rs", "autumn_web::job::claim"));
        assert!(is_test_code(
            "autumn/benches/repository_crud.rs",
            "repository_crud::main"
        ));
    }
}
