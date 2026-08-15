//! An in-process `PostgreSQL` server that answers a real client from a capsule.
//!
//! Replay cannot fabricate result rows: diesel's `PgRow` wraps a
//! `tokio_postgres::Row`, which has no public constructor. The only way to hand
//! a replayed handler real rows is to hand a real `tokio-postgres` client the
//! bytes the real server sent — so [`pool_from_capsule`] builds an ordinary
//! `Pool<AsyncPgConnection>` whose connections are wired to a
//! [`tokio::io::duplex`] pipe with a [`StubServer`] on the far end, speaking
//! just enough of the backend protocol to satisfy the driver.
//!
//! **No socket is opened.** The connection URL handed to the pool manager is
//! never dialled; it exists only because the manager's constructor wants one.
//!
//! # How a statement is answered
//!
//! Every `Sync`-terminated batch of frontend messages is resolved in this
//! order, which is what makes a *warm* pooled connection replayable on a *cold*
//! stub (F3):
//!
//! 1. the housekeeping allowlist — `SET TIME ZONE`, `SET CLIENT_ENCODING`,
//!    `SET statement_timeout`, `SET autumn.capsule_request` — answered
//!    synthetically, because those are the framework's own per-connection and
//!    per-checkout statements and never carry application meaning;
//! 2. the tape's `prologue`, by SQL — what the connection did between birth and
//!    the captured request;
//! 3. the tape's `statements`, by SQL — the `Parse`/`Describe` metadata a
//!    connection that had already prepared this statement would not re-fetch;
//! 4. the tape's `catalog`, by SQL and binds — driver type-info probes;
//! 5. the next unconsumed entry of the tape's `exchanges`, checked for the same
//!    SQL and the same *unmasked* binds;
//! 6. otherwise a divergence: the client gets `ErrorResponse` with `SQLSTATE`
//!    `58000` naming the unexpected statement, and the run's
//!    [`DivergenceLog`] records what happened.
//!
//! # Wire encoding of recorded rows
//!
//! `tokio-postgres` always binds with result format code `1`, so every recorded
//! `DataRow` field is **binary**-encoded. A capsule's frames are written back
//! verbatim, so this only matters when hand-building a fixture.

// Replay-time module (offline `autumn replay` runs, never the serving path);
// kept panic-averse with the same deny set, but deliberately outside the
// request-path panic-gate manifest — see CONTRIBUTING.md.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use diesel::ConnectionError;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::pooled_connection::{
    AsyncDieselConnectionManager, ManagerConfig, RecyclingMethod,
};
use futures::FutureExt as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::capsule::replay::{Divergence, DivergenceKind, DivergenceLog, TapeProgress};
use crate::capsule::schema::{BindValue, Capsule, ConnectionTape, Exchange};
use crate::capsule::wire::{
    self, FrameSplitter, FrontendMessage, build, is_catalog_sql, parse_frontend,
};
use crate::db::PoolError;

/// `SQLSTATE` reported to the client when the tape cannot answer a statement.
///
/// `58000` is `system_error`, the class Postgres itself uses for "something
/// outside the database went wrong" — the honest classification for "this is
/// not a database".
const DIVERGENCE_SQLSTATE: &str = "58000";

/// Buffer size of each half of the in-process connection.
///
/// Large enough that a recorded response is written in one go for anything but
/// a very large result set, and the stub writes promptly after every `Sync`,
/// so neither half can wedge the other (risk R4).
const DUPLEX_CAPACITY: usize = 64 * 1024;

/// Read chunk size for the frontend half.
const READ_CHUNK: usize = 8 * 1024;

/// How long a replayed checkout waits for a pool slot before failing.
///
/// Nothing here dials a socket, so any wait at all means the replayed code is
/// holding more connections at once than the recording did. A bounded wait
/// turns that into a reported divergence; an unbounded one (deadpool's default)
/// turns it into a hung `autumn replay`.
const REPLAY_CHECKOUT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Placeholder connection URL. Never dialled — [`pool_from_capsule`] replaces
/// the manager's establish step wholesale, and the port is one nothing can
/// bind, so a regression that *did* dial would fail loudly.
const REPLAY_URL: &str = "postgres://replay@127.0.0.1:1/replay";

/// `CloseComplete` — the one backend frame the wire module has no builder for,
/// because only the stub server ever needs to emit it.
fn close_complete() -> Vec<u8> {
    vec![b'3', 0, 0, 0, 4]
}

/// Statements the framework itself issues on every connection and every
/// checkout. They are answered synthetically rather than from the tape: the
/// recording's values (a statement timeout, a capsule id) are not the values
/// this run uses, and neither carries application meaning.
///
/// Deliberately the recorder's own predicate
/// ([`wire::is_session_housekeeping`]), not a second spelling of it: a
/// statement the recorder classifies as housekeeping is *absent* from the tape,
/// so a stub that disagreed would answer it from the tape it is not on. That
/// includes the empty batch, which is housekeeping to neither: an empty simple
/// `Query` gets its own `EmptyQueryResponse` (see [`StubServer::simple_query`])
/// rather than being acknowledged as a `SET` nobody sent.
fn is_housekeeping(sql: &str) -> bool {
    wire::is_session_housekeeping(sql)
}

/// Split a possibly multi-statement simple-protocol query at top-level `;`,
/// dropping empty fragments — the quote-aware split the recorder uses.
fn split_statements(sql: &str) -> Vec<&str> {
    wire::split_statements(sql)
        .into_iter()
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .collect()
}

/// A single replayed connection: answers one client from one recorded tape.
pub struct StubServer {
    tape: ConnectionTape,
    divergences: Arc<DivergenceLog>,
    /// Position in `tape.exchanges`. Shared with the driver, which reads it
    /// after the response has resolved to check that nothing recorded was left
    /// unasked — these tasks are detached, so the cursor cannot be returned.
    progress: Arc<TapeProgress>,
    /// Prepared-statement name (`s0`, `s1`, …) to SQL. The names are minted by
    /// a process-global counter in `tokio-postgres`, so they are never stable
    /// across runs and must be learnt from the `Parse` messages.
    statements: HashMap<String, String>,
    /// Portal name to SQL.
    portals: HashMap<String, String>,
}

/// What the resolver decided for one batch.
enum Resolution {
    /// Recorded frames to write back verbatim.
    Recorded(Vec<u8>),
    /// Nothing on the tape answers this.
    Diverged(Divergence),
}

impl StubServer {
    /// A server bound to one recorded tape, registering that tape's
    /// consumption cursor with `divergences`.
    #[must_use]
    pub fn new(tape: ConnectionTape, divergences: Arc<DivergenceLog>) -> Self {
        let progress = divergences.register_tape(&tape);
        Self::with_progress(tape, divergences, progress)
    }

    /// A server bound to a tape whose cursor is already registered — the shape
    /// [`pool_from_capsule`] needs, because it registers *every* tape up front
    /// so unclaimed ones are accounted for too.
    fn with_progress(
        tape: ConnectionTape,
        divergences: Arc<DivergenceLog>,
        progress: Arc<TapeProgress>,
    ) -> Self {
        Self {
            tape,
            divergences,
            progress,
            statements: HashMap::new(),
            portals: HashMap::new(),
        }
    }

    /// Serve one client until it disconnects.
    ///
    /// Returns when the client sends `Terminate`, closes its half of the pipe,
    /// or sends something the framing refuses to model. I/O errors end the
    /// conversation quietly: the client is a pool connection being torn down,
    /// and there is nobody to report to.
    pub async fn serve<S>(
        stream: S,
        tape: ConnectionTape,
        divergences: Arc<DivergenceLog>,
        progress: Arc<TapeProgress>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut server = Self::with_progress(tape, divergences, progress);
        let _ = server.run(stream).await;
    }

    async fn run<S>(&mut self, mut stream: S) -> std::io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut splitter = FrameSplitter::new_frontend();
        let mut batch: Vec<FrontendMessage> = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];

        loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                return Ok(());
            }
            let Some(bytes) = chunk.get(..read) else {
                return Ok(());
            };
            for frame in splitter.push(bytes) {
                let message = parse_frontend(&frame);
                let reply = match message {
                    // SSL is always refused: the pool connects with
                    // `sslmode=disable`, but answer anyway rather than hang.
                    FrontendMessage::SslRequest => Some(vec![b'N']),
                    FrontendMessage::Startup => Some(self.handshake()),
                    FrontendMessage::Terminate => return Ok(()),
                    FrontendMessage::Query(sql) => Some(self.simple_query(&sql)),
                    FrontendMessage::Sync => {
                        batch.push(FrontendMessage::Sync);
                        let reply = self.extended_batch(&batch);
                        batch.clear();
                        Some(reply)
                    }
                    // `Flush` asks for whatever is already produced; the stub
                    // produces nothing before `Sync`, so there is nothing to
                    // push. Every path Autumn's driver takes ends in `Sync`.
                    FrontendMessage::Flush => None,
                    other => {
                        batch.push(other);
                        None
                    }
                };
                if let Some(reply) = reply {
                    stream.write_all(&reply).await?;
                    stream.flush().await?;
                }
            }
            if splitter.is_unrecordable() {
                return Ok(());
            }
        }
    }

    /// `AuthenticationOk` … `ReadyForQuery`.
    ///
    /// A recorder may store the real server's startup stream as a leading
    /// `prologue` entry with empty SQL, in which case it is replayed verbatim
    /// (so `server_version` and friends match the recording exactly, R5).
    /// Otherwise a canned handshake is synthesized.
    fn handshake(&self) -> Vec<u8> {
        if let Some(recorded) = self
            .tape
            .prologue
            .first()
            .filter(|exchange| exchange.sql.is_empty() && !exchange.response.is_empty())
        {
            return recorded.response.clone();
        }
        let mut reply = build::authentication_ok();
        for (key, value) in [
            ("server_version", "16.0"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("integer_datetimes", "on"),
            ("TimeZone", "UTC"),
            ("standard_conforming_strings", "on"),
        ] {
            reply.extend_from_slice(&build::parameter_status(key, value));
        }
        reply.extend_from_slice(&build::backend_key_data(1, 1));
        reply.extend_from_slice(&build::ready_for_query(b'I'));
        reply
    }

    /// Simple-protocol `Query` — one statement (or a `;`-joined batch),
    /// answered as one blob up to `ReadyForQuery` (F8).
    fn simple_query(&self, sql: &str) -> Vec<u8> {
        if split_statements(sql).is_empty() {
            // A blank simple `Query` is not housekeeping and not on any tape:
            // a real backend answers it with `EmptyQueryResponse`, and that is
            // what the driver is waiting for. Acknowledging it as a `SET`
            // would put a `CommandComplete` on the wire that never happened.
            let mut reply = build::empty_query_response();
            reply.extend_from_slice(&build::ready_for_query(b'I'));
            return reply;
        }
        if is_housekeeping(sql) {
            let mut reply = Vec::new();
            for _ in split_statements(sql) {
                reply.extend_from_slice(&build::command_complete("SET"));
            }
            reply.extend_from_slice(&build::ready_for_query(b'I'));
            return reply;
        }
        match self.resolve_execute(sql, &[]) {
            Resolution::Recorded(bytes) => bytes,
            Resolution::Diverged(divergence) => self.diverge(divergence),
        }
    }

    /// One `Sync`-terminated extended-protocol batch.
    fn extended_batch(&mut self, batch: &[FrontendMessage]) -> Vec<u8> {
        let mut parse_sql = None;
        let mut bind: Option<(String, Vec<Option<Vec<u8>>>)> = None;
        let mut describe_sql = None;
        let mut has_describe = false;
        let mut has_execute = false;
        let mut unknown_statement: Option<String> = None;

        for message in batch {
            match message {
                FrontendMessage::Parse { name, sql, .. } => {
                    self.statements.insert(name.clone(), sql.clone());
                    parse_sql = Some(sql.clone());
                }
                FrontendMessage::Bind {
                    portal,
                    statement,
                    params,
                } => {
                    // A `Bind` naming a statement this connection never
                    // `Parse`d is not something the tape can answer. Treating
                    // the missing SQL as the empty string used to make the
                    // batch look like housekeeping (`is_housekeeping("")` was
                    // true) and it was acknowledged as a `SET` — the client
                    // then decoded whatever came next against a statement the
                    // stub had never resolved.
                    let Some(sql) = self.statements.get(statement).cloned() else {
                        unknown_statement = Some(statement.clone());
                        continue;
                    };
                    self.portals.insert(portal.clone(), sql.clone());
                    bind = Some((sql, params.clone()));
                }
                FrontendMessage::Describe { kind, name } => {
                    has_describe = true;
                    describe_sql = if *kind == b'S' {
                        self.statements.get(name).cloned()
                    } else {
                        self.portals.get(name).cloned()
                    };
                }
                FrontendMessage::Execute => has_execute = true,
                _ => {}
            }
        }

        if let Some(name) = unknown_statement {
            return self.diverge(self.divergence(
                DivergenceKind::UnknownStatement,
                None,
                "",
                format!(
                    "the code bound prepared statement {name:?}, which this connection never                      parsed during the replay; the capsule cannot say what it was"
                ),
            ));
        }

        let sql = bind
            .as_ref()
            .map(|(sql, _)| sql.clone())
            .or_else(|| parse_sql.clone())
            .or_else(|| describe_sql.clone());

        // A batch that names no statement at all (`Close` + `Sync`, which
        // `tokio_postgres::Statement`'s destructor sends) needs only its
        // acknowledgements.
        let Some(sql) = sql else {
            return synthesize(batch, None);
        };

        if is_housekeeping(&sql) {
            return synthesize(batch, Some("SET"));
        }

        let resolution = if bind.is_some() || has_execute {
            let params = bind.map(|(_, params)| params).unwrap_or_default();
            self.resolve_execute(&sql, &params)
        } else if has_describe {
            self.resolve_prepare(&sql)
        } else {
            // `Parse` + `Sync` with no `Describe`: acknowledge and move on.
            return synthesize(batch, None);
        };

        match resolution {
            Resolution::Recorded(bytes) => bytes,
            Resolution::Diverged(divergence) => self.diverge(divergence),
        }
    }

    /// Answer a `Parse`/`Describe` from the connection's recorded statement
    /// metadata (F3: the capture ran on a warm connection that had prepared
    /// this statement long before the captured request).
    fn resolve_prepare(&self, sql: &str) -> Resolution {
        if let Some(exchange) = find_by_sql(&self.tape.statements, sql) {
            return Resolution::Recorded(exchange.response.clone());
        }
        if let Some(exchange) = find_by_sql(&self.tape.catalog, sql) {
            return Resolution::Recorded(exchange.response.clone());
        }
        if self.tape_mentions(sql) {
            return Resolution::Diverged(self.divergence(
                DivergenceKind::UnknownStatement,
                None,
                sql,
                format!(
                    "the capsule records executions of {sql:?} but no Parse/Describe metadata \
                     for it, so the replayed driver cannot prepare the statement"
                ),
            ));
        }
        Resolution::Diverged(self.divergence(
            DivergenceKind::UnrecordedQuery,
            None,
            sql,
            unrecorded_detail(sql),
        ))
    }

    /// Answer a `Bind`/`Execute` (or a simple `Query`) from the tape.
    ///
    /// The **ordered cursor is consulted first**, and the keyed buckets
    /// (`prologue`, `catalog`) only answer statements the cursor is not
    /// expecting. The other way round, any statement that appears in both — a
    /// `BEGIN` in the connection's prologue and again as the request's own
    /// first exchange is the ordinary case — would be answered from the keyed
    /// bucket without advancing the cursor, and the tape would then be one
    /// behind for the rest of the run: an `SqlMismatch` on the next statement
    /// and `UnconsumedExchanges` at the end, on a capsule that recorded
    /// everything perfectly.
    ///
    /// Takes `&self`: the tape cursor moved into the shared [`TapeProgress`],
    /// which the driver reads after the run, so advancing it no longer needs
    /// exclusive access to the server.
    fn resolve_execute(&self, sql: &str, params: &[Option<Vec<u8>>]) -> Resolution {
        let expected = self.tape.exchanges.get(self.progress.consumed());
        if let Some(expected) = expected.filter(|expected| expected.sql == sql) {
            if !binds_match(&expected.binds, params) {
                let expected_binds = describe_binds(&expected.binds);
                let actual_binds = describe_params(params);
                let expected_sql = expected.sql.clone();
                return Resolution::Diverged(self.divergence(
                    DivergenceKind::BindMismatch,
                    Some(expected_sql),
                    sql,
                    format!(
                        "{sql:?} was recorded with binds {expected_binds} but the code bound \
                         {actual_binds}"
                    ),
                ));
            }
            let response = expected.response.clone();
            self.progress.advance();
            return Resolution::Recorded(response);
        }

        // Not what the tape expects next: it may still be something the
        // connection had already done before the request began.
        if let Some(exchange) = find_by_sql(&self.tape.prologue, sql) {
            return Resolution::Recorded(exchange.response.clone());
        }
        if let Some(exchange) = self
            .tape
            .catalog
            .iter()
            .find(|exchange| exchange.sql == sql && binds_match(&exchange.binds, params))
        {
            return Resolution::Recorded(exchange.response.clone());
        }

        let Some(expected) = expected else {
            // A driver catalog probe is never an *ordering* problem: it is a
            // type-info lookup the recorded connection's cache already had, so
            // it is reported as unrecorded with the F4 hint rather than as a
            // tape that ran out.
            if is_catalog_sql(sql) {
                return Resolution::Diverged(self.divergence(
                    DivergenceKind::UnrecordedQuery,
                    None,
                    sql,
                    unrecorded_detail(sql),
                ));
            }
            return Resolution::Diverged(self.divergence(
                DivergenceKind::TapeExhausted,
                None,
                sql,
                format!(
                    "the connection's tape holds {} exchange(s) and they have all been replayed, \
                     but the code asked for {sql:?}",
                    self.tape.exchanges.len()
                ),
            ));
        };

        let kind = if self.tape_mentions(sql) {
            DivergenceKind::SqlMismatch
        } else {
            DivergenceKind::UnrecordedQuery
        };
        let expected_sql = expected.sql.clone();
        let detail = if kind == DivergenceKind::SqlMismatch {
            format!(
                "the tape expected {expected_sql:?} next but the code sent {sql:?}; the \
                 statements have been reordered since the recording"
            )
        } else {
            unrecorded_detail(sql)
        };
        Resolution::Diverged(self.divergence(kind, Some(expected_sql), sql, detail))
    }

    /// Whether the tape mentions this SQL anywhere at all.
    fn tape_mentions(&self, sql: &str) -> bool {
        [
            &self.tape.exchanges,
            &self.tape.prologue,
            &self.tape.statements,
            &self.tape.catalog,
        ]
        .into_iter()
        .any(|list| list.iter().any(|exchange| exchange.sql == sql))
    }

    /// Build a divergence for the current tape position.
    fn divergence(
        &self,
        kind: DivergenceKind,
        expected_sql: Option<String>,
        actual_sql: &str,
        detail: String,
    ) -> Divergence {
        Divergence {
            kind,
            connection: self.tape.id,
            exchange_index: self.progress.consumed(),
            expected_sql,
            actual_sql: actual_sql.to_owned(),
            detail,
        }
    }

    /// Record a divergence and produce the `ErrorResponse` the client sees.
    fn diverge(&self, divergence: Divergence) -> Vec<u8> {
        let mut reply = build::error_response(
            DIVERGENCE_SQLSTATE,
            &format!("autumn_replay_divergence: {}", divergence.detail),
        );
        // `I` (idle) rather than `E`: the stub is not in a transaction, and an
        // `E` would make diesel's transaction manager believe a transaction
        // needs rolling back.
        reply.extend_from_slice(&build::ready_for_query(b'I'));
        self.divergences.record(divergence);
        reply
    }
}

/// Acknowledge every message in a batch without consulting the tape.
///
/// Used for the housekeeping allowlist and for bookkeeping-only batches. The
/// acknowledgements are emitted in message order, exactly as a real backend
/// would. `command_tag` is the `CommandComplete` tag for an `Execute`, when the
/// batch has one.
fn synthesize(batch: &[FrontendMessage], command_tag: Option<&str>) -> Vec<u8> {
    let mut reply = Vec::new();
    for message in batch {
        match message {
            FrontendMessage::Parse { .. } => reply.extend_from_slice(&build::parse_complete()),
            FrontendMessage::Bind { .. } => reply.extend_from_slice(&build::bind_complete()),
            FrontendMessage::Describe { kind, .. } => {
                if *kind == b'S' {
                    reply.extend_from_slice(&build::parameter_description(&[]));
                }
                reply.extend_from_slice(&build::no_data());
            }
            FrontendMessage::Execute => {
                reply.extend_from_slice(&build::command_complete(command_tag.unwrap_or("SET")));
            }
            FrontendMessage::Close { .. } => reply.extend_from_slice(&close_complete()),
            FrontendMessage::Sync => reply.extend_from_slice(&build::ready_for_query(b'I')),
            _ => {}
        }
    }
    reply
}

/// Explanation for SQL the capsule never saw, with the type-info hint (F4)
/// when the statement is a driver catalog probe.
fn unrecorded_detail(sql: &str) -> String {
    if is_catalog_sql(sql) {
        format!(
            "the code sent the driver catalog probe {sql:?}, which the capsule did not record; a \
             custom or extension type whose OID was not in the recorded connection's type cache \
             cannot be resolved offline"
        )
    } else {
        format!(
            "the code sent {sql:?}, which the capsule never recorded on this connection; the \
             query path has changed since the capsule was captured"
        )
    }
}

/// First recorded exchange whose SQL matches.
fn find_by_sql<'a>(exchanges: &'a [Exchange], sql: &str) -> Option<&'a Exchange> {
    exchanges.iter().find(|exchange| exchange.sql == sql)
}

/// Whether the binds the code sent are the binds the capsule recorded.
///
/// [`BindValue::Masked`] always matches: redaction blanked the value because it
/// echoed something masked out of the request, so the capsule does not carry
/// the bytes to compare against (F1).
fn binds_match(recorded: &[BindValue], actual: &[Option<Vec<u8>>]) -> bool {
    recorded.len() == actual.len()
        && recorded
            .iter()
            .zip(actual)
            .all(|(recorded, actual)| match recorded {
                BindValue::Masked => true,
                BindValue::Null => actual.is_none(),
                BindValue::Value(bytes) => actual.as_deref() == Some(bytes.as_slice()),
            })
}

/// Render recorded binds for a divergence message.
fn describe_binds(binds: &[BindValue]) -> String {
    let rendered: Vec<String> = binds
        .iter()
        .map(|bind| match bind {
            BindValue::Null => "NULL".to_owned(),
            BindValue::Masked => "[FILTERED]".to_owned(),
            BindValue::Value(bytes) => hex_preview(bytes),
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

/// Render the binds the code actually sent.
fn describe_params(params: &[Option<Vec<u8>>]) -> String {
    let rendered: Vec<String> = params
        .iter()
        .map(|param| {
            param
                .as_deref()
                .map_or_else(|| "NULL".to_owned(), hex_preview)
        })
        .collect();
    format!("[{}]", rendered.join(", "))
}

/// Hex preview of a bind value, bounded so a large parameter cannot flood the
/// verdict. Bind values are wire-format bytes, not necessarily text.
fn hex_preview(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    const MAX: usize = 16;
    let head = bytes.get(..MAX.min(bytes.len())).unwrap_or_default();
    let mut rendered = String::with_capacity(head.len().saturating_mul(2).saturating_add(16));
    rendered.push_str("0x");
    for byte in head {
        let _ = write!(rendered, "{byte:02x}");
    }
    if bytes.len() > MAX {
        let _ = write!(rendered, "… ({} bytes)", bytes.len());
    }
    rendered
}

/// Build a connection pool that serves `capsule`'s recorded database traffic.
///
/// The pool is sized one slot *above* the number of connections the capsule
/// recorded, and each connection the pool establishes claims the next
/// unclaimed tape. This is why
/// [`DbBuffer`](crate::capsule::DbBuffer) records tapes in the order the
/// request *first used* each connection rather than by connection id: the
/// *i*-th tape answers the *i*-th connection the replayed run opens, so any
/// other ordering swaps the tapes and diverges on traffic that was recorded
/// perfectly. A request that opens *more* connections than the recording did
/// gets an empty tape, on which every statement is a divergence — the honest
/// answer, since nothing was recorded for it (F12). That path is only
/// reachable because of the spare slot and the wait timeout below: sized to the
/// recording exactly and with deadpool's default (unbounded) wait, a replayed
/// handler that held two connections at once would block forever on the second
/// checkout and `autumn replay` would hang instead of reporting the
/// divergence. Oversubscription past the spare slot fails the checkout after
/// [`REPLAY_CHECKOUT_TIMEOUT`], which the handler surfaces as an error and the
/// verdict as a mismatch.
///
/// Recycling is set to [`RecyclingMethod::Fast`] so returning a connection to
/// the pool does not issue a `SELECT 1` ping the tape never recorded.
///
/// Every tape is registered with `divergences` here rather than when a
/// connection claims it, so a run that opens *fewer* connections than the
/// recording did — up to and including a run that never touches the database —
/// is still held to the whole recording.
///
/// # Errors
///
/// Returns [`PoolError::Build`] when the underlying deadpool builder rejects
/// the configuration.
pub fn pool_from_capsule(
    capsule: &Capsule,
    divergences: Arc<DivergenceLog>,
) -> Result<Pool<AsyncPgConnection>, PoolError> {
    // Everything that is not explicitly a replica tape — including tapes from
    // capsules that predate roles — is the primary's.
    let tapes = capsule
        .db
        .as_ref()
        .map(|db| {
            db.connections
                .iter()
                .filter(|tape| tape.role != crate::capsule::schema::TAPE_ROLE_REPLICA)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    pool_from_tapes(tapes, divergences)
}

/// The replica-role sibling of [`pool_from_capsule`], or `None` when the
/// capsule recorded no replica traffic.
///
/// Replay rebuilds the topology with the same shape the recording had: a
/// write-then-read request claims each tape from the pool role it was
/// recorded on, instead of funnelling reads into the primary stub and
/// mismatching its tape while the replica's sits unconsumed.
///
/// # Errors
///
/// Returns [`PoolError::Build`] when the underlying deadpool builder rejects
/// the configuration.
pub fn replica_pool_from_capsule(
    capsule: &Capsule,
    divergences: Arc<DivergenceLog>,
) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
    let tapes: Vec<ConnectionTape> = capsule
        .db
        .as_ref()
        .map(|db| {
            db.connections
                .iter()
                .filter(|tape| tape.role == crate::capsule::schema::TAPE_ROLE_REPLICA)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if tapes.is_empty() {
        return Ok(None);
    }
    pool_from_tapes(tapes, divergences).map(Some)
}

/// Shared constructor: a stub pool that serves `tapes` in first-use order.
fn pool_from_tapes(
    tapes: Vec<ConnectionTape>,
    divergences: Arc<DivergenceLog>,
) -> Result<Pool<AsyncPgConnection>, PoolError> {
    let tapes: Arc<Vec<ConnectionTape>> = Arc::new(tapes);
    // One spare slot beyond the recording: see the note above on why replay
    // must never be able to block on its own pool.
    let max_size = tapes.len().saturating_add(1);
    let next_tape = Arc::new(AtomicUsize::new(0));
    let progress: Arc<Vec<Arc<TapeProgress>>> = Arc::new(
        tapes
            .iter()
            .map(|tape| divergences.register_tape(tape))
            .collect(),
    );

    let mut config = ManagerConfig::<AsyncPgConnection>::default();
    config.recycling_method = RecyclingMethod::Fast;
    config.custom_setup = Box::new(move |_url: &str| {
        let tapes = Arc::clone(&tapes);
        let next_tape = Arc::clone(&next_tape);
        let divergences = Arc::clone(&divergences);
        let progress = Arc::clone(&progress);
        async move {
            let index = next_tape.fetch_add(1, Ordering::SeqCst);
            let tape = tapes.get(index).cloned().unwrap_or_default();
            // A connection past the end of the recording claims an empty tape,
            // and an unregistered cursor with it: there is nothing recorded for
            // it to leave unconsumed.
            let progress = progress.get(index).map_or_else(
                || Arc::new(TapeProgress::new(tape.id, Vec::new())),
                Arc::clone,
            );

            let (client_half, server_half) = tokio::io::duplex(DUPLEX_CAPACITY);
            tokio::spawn(StubServer::serve(server_half, tape, divergences, progress));

            let mut pg = tokio_postgres::Config::new();
            pg.ssl_mode(tokio_postgres::config::SslMode::Disable)
                .user("replay")
                .dbname("replay");
            let (client, connection) = pg
                .connect_raw(client_half, tokio_postgres::NoTls)
                .await
                .map_err(|error| {
                    ConnectionError::BadConnection(format!(
                        "the replay stub server refused the handshake: {error}"
                    ))
                })?;
            // We own the connection future because `try_from_client_and_connection`
            // is hard-wired to `tokio_postgres::Socket` and cannot take a duplex
            // half. The cost is no notification stream on replay pools, which
            // replay does not use.
            tokio::spawn(async move {
                let _ = connection.await;
            });
            AsyncPgConnection::try_from(client).await
        }
        .boxed()
    });

    let manager =
        AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(REPLAY_URL, config);
    Ok(Pool::builder(manager)
        .max_size(max_size)
        .wait_timeout(Some(REPLAY_CHECKOUT_TIMEOUT))
        .create_timeout(Some(REPLAY_CHECKOUT_TIMEOUT))
        .runtime(deadpool::Runtime::Tokio1)
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capsule::schema::ExchangeProtocol;

    fn exchange(sql: &str, binds: Vec<BindValue>) -> Exchange {
        Exchange {
            protocol: ExchangeProtocol::Extended,
            sql: sql.to_owned(),
            binds,
            response: build::ready_for_query(b'I'),
            row_count: 0,
            error: None,
        }
    }

    /// Tapes split by recorded role: the primary stub pool serves everything
    /// that is not explicitly replica (pre-role capsules included), and a
    /// replica pool exists exactly when replica tapes were recorded — so a
    /// write-then-read request claims each tape from the pool it was recorded
    /// on.
    #[tokio::test]
    async fn tapes_are_split_by_recorded_role() {
        use crate::capsule::schema::{CapsuleOutcome, ConnectionTape, test_support};

        let mut capsule = test_support::capsule(
            test_support::request("GET", "/split"),
            CapsuleOutcome::Status {
                code: 500,
                message: String::new(),
                problem_type: None,
            },
        );
        capsule.db = Some(crate::capsule::schema::CapsuleDb {
            connections: vec![
                ConnectionTape {
                    id: 1,
                    role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
                    ..ConnectionTape::default()
                },
                ConnectionTape {
                    id: 2,
                    role: crate::capsule::schema::TAPE_ROLE_REPLICA.to_owned(),
                    ..ConnectionTape::default()
                },
            ],
        });

        let divergences = Arc::new(DivergenceLog::new());
        let replica = replica_pool_from_capsule(&capsule, Arc::clone(&divergences))
            .expect("replica pool builds");
        assert!(
            replica.is_some(),
            "a capsule with replica-recorded tapes must get a replica stub pool"
        );

        capsule
            .db
            .as_mut()
            .expect("db present")
            .connections
            .retain(|tape| tape.role != crate::capsule::schema::TAPE_ROLE_REPLICA);
        let no_replica =
            replica_pool_from_capsule(&capsule, divergences).expect("replica pool builds");
        assert!(
            no_replica.is_none(),
            "no replica tapes — including a pre-role capsule — means no replica pool"
        );
    }

    #[test]
    fn framework_housekeeping_is_recognized() {
        assert!(is_housekeeping("SET TIME ZONE 'UTC'"));
        assert!(is_housekeeping("SET CLIENT_ENCODING TO 'UTF8'"));
        assert!(is_housekeeping("SET statement_timeout = 5000"));
        assert!(is_housekeeping(
            "SET statement_timeout = 0; SET autumn.capsule_request = 'req-1'"
        ));
        assert!(!is_housekeeping("SELECT 1"));
        assert!(
            !is_housekeeping("SET statement_timeout = 0; SELECT 1"),
            "a batch is only housekeeping when every statement in it is"
        );
    }

    #[test]
    fn masked_binds_are_excluded_from_comparison() {
        assert!(binds_match(
            &[BindValue::Masked],
            &[Some(b"anything".to_vec())]
        ));
        assert!(binds_match(&[BindValue::Null], &[None]));
        assert!(binds_match(
            &[BindValue::Value(vec![1, 2])],
            &[Some(vec![1, 2])]
        ));
        assert!(!binds_match(
            &[BindValue::Value(vec![1, 2])],
            &[Some(vec![3, 4])]
        ));
        assert!(!binds_match(&[BindValue::Null], &[Some(vec![1])]));
        assert!(
            !binds_match(&[BindValue::Masked], &[]),
            "a different arity is still a mismatch"
        );
    }

    #[test]
    fn the_tape_is_consumed_in_order_and_then_exhausted() {
        let tape = ConnectionTape {
            id: 3,
            role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
            prologue: Vec::new(),
            statements: Vec::new(),
            catalog: Vec::new(),
            exchanges: vec![exchange("SELECT 1", Vec::new())],
        };
        let log = Arc::new(DivergenceLog::new());
        let server = StubServer::new(tape, Arc::clone(&log));

        assert!(matches!(
            server.resolve_execute("SELECT 1", &[]),
            Resolution::Recorded(_)
        ));
        match server.resolve_execute("SELECT 1", &[]) {
            Resolution::Diverged(divergence) => {
                assert_eq!(divergence.kind, DivergenceKind::TapeExhausted);
                assert_eq!(divergence.connection, 3);
            }
            Resolution::Recorded(_) => panic!("the tape held only one exchange"),
        }
    }

    #[test]
    fn serving_an_exchange_advances_the_shared_consumption_cursor() {
        let tape = ConnectionTape {
            id: 5,
            role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
            prologue: Vec::new(),
            statements: Vec::new(),
            catalog: Vec::new(),
            exchanges: vec![
                exchange("SELECT 1", Vec::new()),
                exchange("SELECT 2", vec![]),
            ],
        };
        let log = Arc::new(DivergenceLog::new());
        let server = StubServer::new(tape, Arc::clone(&log));

        // Nothing served yet: the whole tape is outstanding.
        assert_eq!(log.unconsumed().len(), 1);

        let _ = server.resolve_execute("SELECT 1", &[]);
        let outstanding = log.unconsumed();
        assert_eq!(
            outstanding.first().map(|entry| entry.exchange_index),
            Some(1),
            "one exchange served must leave the cursor on the second: {outstanding:?}"
        );

        let _ = server.resolve_execute("SELECT 2", &[]);
        assert!(
            log.unconsumed().is_empty(),
            "a fully replayed tape must leave nothing outstanding"
        );
    }

    /// The ordered cursor is consulted before the keyed buckets. A statement
    /// that appears in both — `BEGIN` in the connection prologue and again as
    /// the request's own first exchange — would otherwise be answered from the
    /// prologue without advancing the cursor, and the tape would be one behind
    /// for the rest of the run: a mismatch and a pile of unconsumed exchanges
    /// on a capsule that recorded everything perfectly.
    #[test]
    fn the_ordered_cursor_is_consulted_before_the_keyed_buckets() {
        let tape = ConnectionTape {
            id: 11,
            role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
            prologue: vec![exchange("BEGIN", Vec::new())],
            statements: Vec::new(),
            catalog: Vec::new(),
            exchanges: vec![
                exchange("BEGIN", Vec::new()),
                exchange("SELECT 1", Vec::new()),
            ],
        };
        let log = Arc::new(DivergenceLog::new());
        let server = StubServer::new(tape, Arc::clone(&log));

        assert!(matches!(
            server.resolve_execute("BEGIN", &[]),
            Resolution::Recorded(_)
        ));
        assert_eq!(
            server.progress.consumed(),
            1,
            "answering the exchange the tape expects must consume it, even when the same \
             SQL is also in the prologue"
        );
        assert!(matches!(
            server.resolve_execute("SELECT 1", &[]),
            Resolution::Recorded(_)
        ));
        assert!(
            log.unconsumed().is_empty(),
            "the whole tape must be consumable: {:?}",
            log.unconsumed()
        );
        assert!(
            log.is_empty(),
            "and nothing may diverge: {:?}",
            log.entries()
        );

        // The prologue still answers a statement the cursor is *not* expecting.
        assert!(matches!(
            server.resolve_execute("BEGIN", &[]),
            Resolution::Recorded(_)
        ));
    }

    /// A `Bind` naming a statement the replay never parsed is a divergence, not
    /// a synthesized `SET`: the empty SQL it used to fall back to looked like
    /// housekeeping to the allowlist and was acknowledged as one.
    #[test]
    fn a_bind_for_an_unknown_statement_diverges() {
        let log = Arc::new(DivergenceLog::new());
        let mut server = StubServer::new(ConnectionTape::default(), Arc::clone(&log));
        let reply = server.extended_batch(&[
            FrontendMessage::Bind {
                portal: String::new(),
                statement: "s7".to_owned(),
                params: Vec::new(),
            },
            FrontendMessage::Execute,
            FrontendMessage::Sync,
        ]);

        assert_eq!(
            log.entries().first().map(|entry| entry.kind),
            Some(DivergenceKind::UnknownStatement),
            "an unparsed statement name must be reported, got {:?}",
            log.entries()
        );
        assert!(
            reply.starts_with(b"E"),
            "the client must get an ErrorResponse rather than a fabricated CommandComplete"
        );
    }

    /// An empty simple `Query` is what a real backend answers with
    /// `EmptyQueryResponse`. It is not housekeeping — nothing is there to be
    /// the framework's own — so it must not be acknowledged as a `SET`.
    #[test]
    fn an_empty_simple_query_gets_an_empty_query_response() {
        let server = StubServer::new(ConnectionTape::default(), Arc::new(DivergenceLog::new()));
        let reply = server.simple_query("   ");
        assert_eq!(
            reply.first(),
            Some(&b'I'),
            "an empty query is answered with EmptyQueryResponse, got {reply:?}"
        );
        assert!(!is_housekeeping(""), "an empty batch is not housekeeping");
        assert!(
            !is_housekeeping("SET statement_timeout = 0; SELECT 1"),
            "a batch is only housekeeping when every statement in it is"
        );
    }

    /// A capsule that recorded one connection must still answer a handler that
    /// holds two at once: the pool is sized one above the recording and waits
    /// with a timeout, so oversubscription is reported rather than deadlocked.
    /// Sized to the recording exactly, this test never returned.
    #[tokio::test]
    async fn a_replay_pool_never_blocks_on_itself() {
        let mut capsule = crate::capsule::schema::test_support::capsule(
            crate::capsule::schema::test_support::request("GET", "/boom"),
            crate::capsule::schema::CapsuleOutcome::Status {
                code: 500,
                message: "boom".to_owned(),
                problem_type: None,
            },
        );
        capsule.db = Some(crate::capsule::schema::CapsuleDb {
            connections: vec![ConnectionTape {
                id: 1,
                role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
                ..ConnectionTape::default()
            }],
        });

        let pool = pool_from_capsule(&capsule, Arc::new(DivergenceLog::new()))
            .expect("the replay pool builds");
        assert_eq!(
            pool.status().max_size,
            2,
            "one spare slot beyond the recording keeps a two-connection handler moving"
        );

        let (first, second) = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            futures::future::join(pool.get(), pool.get()),
        )
        .await
        .expect("two concurrent checkouts must not deadlock the replay pool");
        assert!(
            first.is_ok() && second.is_ok(),
            "both checkouts must resolve"
        );
    }

    #[test]
    fn an_unrecorded_statement_names_itself() {
        let tape = ConnectionTape {
            id: 1,
            role: crate::capsule::schema::TAPE_ROLE_PRIMARY.to_owned(),
            prologue: Vec::new(),
            statements: Vec::new(),
            catalog: Vec::new(),
            exchanges: vec![exchange("SELECT 1", Vec::new())],
        };
        let server = StubServer::new(tape, Arc::new(DivergenceLog::new()));
        match server.resolve_execute("SELECT * FROM gadgets", &[]) {
            Resolution::Diverged(divergence) => {
                assert_eq!(divergence.kind, DivergenceKind::UnrecordedQuery);
                assert!(divergence.detail.contains("gadgets"));
                assert_eq!(divergence.expected_sql.as_deref(), Some("SELECT 1"));
            }
            Resolution::Recorded(_) => panic!("nothing recorded should have matched"),
        }
    }

    #[test]
    fn a_catalog_probe_diverges_with_a_type_hint() {
        let server = StubServer::new(ConnectionTape::default(), Arc::new(DivergenceLog::new()));
        // The driver's real enum type-info probe, verbatim — hand-written
        // catalog reads are application work and no longer classify.
        let probe = "SELECT enumlabel\nFROM pg_catalog.pg_enum\nWHERE enumtypid = $1\nORDER BY enumsortorder\n";
        match server.resolve_execute(probe, &[]) {
            Resolution::Diverged(divergence) => {
                assert!(
                    divergence.detail.contains("catalog probe"),
                    "expected the type-info hint, got {:?}",
                    divergence.detail
                );
            }
            Resolution::Recorded(_) => panic!("nothing recorded should have matched"),
        }
    }
}
