//! Referentially-intact row subsetting for `autumn db scrub` (issue #1636).
//!
//! # Why this exists
//!
//! A scrubbed copy of a multi-hundred-GB production database is still a
//! multi-hundred-GB database: PII-safe, but useless on a laptop. Teams then
//! either work without realistic data or hand-roll `pg_sample` scripts that know
//! nothing about PII and silently break foreign keys.
//!
//! This module selects a small subset of rows that is **referentially correct by
//! construction**, in the same pass and the same transaction as the scrub, so no
//! flag combination can emit sampled-but-unscrubbed rows.
//!
//! # How the subset is chosen
//!
//! The developer names root entities on the command line
//! (`--sample users=1%`). Every other row is pulled in by walking the foreign
//! key graph the database itself reports, to a fixpoint:
//!
//! - **Descend** — rows referencing a selected row are selected, and are
//!   themselves descend-eligible. This is what "1% of users plus all their
//!   related rows" means.
//! - **Ascend** — rows a selected row references are selected, but are **not**
//!   descend-eligible. This is what makes every foreign key resolve, without
//!   letting one shared parent (an org, a plan) drag its whole subtree back in.
//!
//! Two per-table rules refine that: `always_include` tables (reference/lookup
//! data) start with every row selected and are never descended from;
//! `never_include` tables (audit logs) are emptied.
//!
//! # Fail-closed
//!
//! Anything the walk cannot prove is refused before a row is deleted: a table no
//! root can reach (it would be silently emptied), a foreign key into a
//! `never_include` table (it would dangle), a table with no primary key (it has
//! no row identity to select on), a reference cycle between tables (the deletes
//! have no safe order), and a framework-owned table referencing a sampled one.
//! After the deletes, every foreign key is re-verified inside the same
//! transaction, so a violation rolls the whole run back.

use std::collections::{BTreeMap, BTreeSet};

use diesel::{PgConnection, RunQueryDsl as _, sql_query};

use super::super::{quote_ident, quote_literal};
// Every statement is schema-qualified through the scrub's own helper, so a
// role- or database-level `search_path` cannot redirect a delete to a table
// nothing classified.
use super::qualified_ident as qualified;

/// Ceiling on closure passes. The keep-sets only grow and are bounded by the
/// row count, so the walk always converges; this turns a hypothetical
/// non-convergence into an error instead of a hung command.
const MAX_PASSES: usize = 1000;

// ─── Specs ──────────────────────────────────────────────────────────────────

/// How many root rows one `--sample <table>=<spec>` selects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleAmount {
    /// A percentage of the table's rows, rounded up.
    Percent(f64),
    /// An absolute row count, capped at the table's size.
    Count(u64),
}

/// One parsed `--sample <table>=<spec>` argument.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleSpec {
    /// The root table.
    pub table: String,
    /// How much of it to select.
    pub amount: SampleAmount,
}

/// The `[sample]` section of `scrub.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleRules {
    /// Reference/lookup tables copied in full.
    #[serde(default)]
    pub always_include: Vec<String>,
    /// Tables excluded entirely (their rows are deleted).
    #[serde(default)]
    pub never_include: Vec<String>,
}

/// One foreign key constraint, composite keys included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyConstraint {
    /// The constraint name, for error messages.
    pub name: String,
    /// The referencing table.
    pub child_table: String,
    /// The referencing columns, in key order.
    pub child_columns: Vec<String>,
    /// The referenced table.
    pub parent_table: String,
    /// The referenced columns, in key order.
    pub parent_columns: Vec<String>,
    /// True for `MATCH FULL`, whose composite rule differs from the default
    /// `MATCH SIMPLE`: a tuple must be entirely NULL or entirely populated, so
    /// a partially-NULL one violates the constraint instead of satisfying it.
    pub match_full: bool,
}

/// What decides a table's rows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleRole {
    /// A developer-chosen root: this many rows, chosen deterministically.
    Root(SampleAmount),
    /// Copied in full (`[sample] always_include`).
    AlwaysInclude,
    /// Emptied (`[sample] never_include`).
    NeverInclude,
    /// Selected only by the graph walk.
    Related,
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Failure modes for sampling. Every variant refuses **before** a row is
/// deleted, and none embeds a connection URL.
#[derive(Debug, PartialEq, Eq)]
pub enum SampleError {
    /// A `--sample` argument is not `<table>=<count|percent%>`.
    InvalidSpec {
        /// The argument as written.
        spec: String,
        /// A human-readable reason.
        detail: String,
    },
    /// `--sample` named a table the classified universe does not have.
    UnknownRoot {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// The same root table was given twice.
    DuplicateRoot {
        /// The table name.
        table: String,
    },
    /// A root table is also declared `never_include`.
    RootNeverIncluded {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// A root table is also declared `always_include`.
    RootAlwaysIncluded {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// A table is declared both `always_include` and `never_include`.
    RuleContradiction {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// `[sample]` names a table the database does not have.
    StaleRule {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// `[sample]` names a framework-owned table, which sampling never covers.
    FrameworkRule {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// A table no root can reach through the foreign key graph. Sampling it
    /// would empty it silently, so the run refuses instead.
    UncoveredTables {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// A retained table references a `never_include` table, so the reference
    /// would dangle.
    NeverIncludeReferenced {
        /// `child -> parent (constraint)` descriptions, sorted.
        edges: Vec<String>,
    },
    /// A table outside the sampled universe — a framework-owned one — references
    /// a table the sample subsets, so removing those rows would break it.
    OutsideTableReferencesSampled {
        /// `child -> parent (constraint)` descriptions, sorted.
        edges: Vec<String>,
    },
    /// A foreign key declared directly on a leaf partition rather than cloned
    /// from its partitioned parent. The plan is keyed on the parent, whose rows
    /// span every partition, so neither the walk nor the integrity re-check can
    /// represent an edge that binds one partition only.
    PartitionLocalForeignKey {
        /// `child -> parent (constraint)` descriptions, sorted.
        edges: Vec<String>,
    },
    /// A table the sample keeps rows in references a framework-owned table that
    /// `[framework] purge` empties, so no order of the two satisfies the key.
    RetainedReferencesPurged {
        /// `child -> parent (constraint)` descriptions, sorted.
        edges: Vec<String>,
    },
    /// A table in the sampled universe has no primary key, so its rows have no
    /// identity the walk can select on.
    NoRowKey {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// Two or more tables reference each other, so the deletes have no order
    /// that keeps every foreign key satisfied.
    ForeignKeyCycle {
        /// The table names, sorted.
        tables: Vec<String>,
    },
    /// The closure walk did not converge (defensive; unreachable in practice).
    IterationLimit,
    /// The post-sample foreign key verification found unresolved references.
    IntegrityViolation {
        /// `constraint: child -> parent (n row(s))` descriptions, sorted.
        violations: Vec<String>,
    },
}

impl std::fmt::Display for SampleError {
    // One arm per variant, each a single actionable message; splitting the
    // match would scatter the error copy across helpers for no reader benefit.
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSpec { spec, detail } => write!(
                f,
                "--sample {spec:?} is not a valid sample spec: {detail}\n  \
                 Write it as `<table>=<count>` or `<table>=<percent>%`, for example \
                 `--sample users=1%` or `--sample users=500`."
            ),
            Self::UnknownRoot { tables } => write!(
                f,
                "--sample names {} table(s) that are not part of the sampled \
                 universe:\n{}\n  \
                 A root must be one of the app's own tables. Framework-owned tables are \
                 emptied with `[framework] purge` instead, and a partition is sampled \
                 through its parent table.",
                tables.len(),
                bullets(tables),
            ),
            Self::DuplicateRoot { table } => write!(
                f,
                "--sample names {table:?} more than once.\n  \
                 A root table takes exactly one size; give the one you mean."
            ),
            Self::RootNeverIncluded { tables } => write!(
                f,
                "--sample names {} table(s) that {SAMPLE_SECTION} excludes:\n{}\n  \
                 A root is what the sample is built from, so it cannot also be dropped. \
                 Remove it from `never_include`, or sample a different root.",
                tables.len(),
                bullets(tables),
            ),
            Self::RootAlwaysIncluded { tables } => write!(
                f,
                "--sample names {} table(s) that {SAMPLE_SECTION} copies whole:\n{}\n  \
                 A size and \"every row\" are two different answers. Remove it from \
                 `always_include` to subset it, or drop the --sample root to keep it \
                 whole.",
                tables.len(),
                bullets(tables),
            ),
            Self::RuleContradiction { tables } => write!(
                f,
                "{} table(s) are declared both `always_include` and `never_include` in \
                 {SAMPLE_SECTION}:\n{}\n  Pick one.",
                tables.len(),
                bullets(tables),
            ),
            Self::StaleRule { tables } => write!(
                f,
                "{SAMPLE_SECTION} names {} table(s) the database does not have:\n{}\n  \
                 The declaration has drifted from the schema — remove or rename the stale \
                 entries.",
                tables.len(),
                bullets(tables),
            ),
            Self::FrameworkRule { tables } => write!(
                f,
                "{SAMPLE_SECTION} names {} framework-owned table(s):\n{}\n  \
                 Sampling covers the app's own tables only — framework-owned rows are \
                 emptied with `[framework] purge = [...]` instead.",
                tables.len(),
                bullets(tables),
            ),
            Self::UncoveredTables { tables } => write!(
                f,
                "{} table(s) cannot be reached from any --sample root through the foreign \
                 key graph:\n{}\n  \
                 Sampling would empty them without saying so. Name one as a root \
                 (`--sample <table>=<n>`), copy it whole (`[sample] always_include`), or \
                 drop it deliberately (`[sample] never_include`).\n  \
                 Being connected to a root is not enough: the walk descends only out of a \
                 root and out of the tables it descended into. A table hanging off one it \
                 merely ASCENDED into \u{2014} a shared org a kept user points at \u{2014} or off \
                 an `always_include` lookup table, is not reachable: descending from one \
                 such row would drag the whole database back in.",
                tables.len(),
                bullets(tables),
            ),
            Self::NeverIncludeReferenced { edges } => write!(
                f,
                "{} foreign key(s) point at a table {SAMPLE_SECTION} excludes:\n{}\n  \
                 Emptying the parent would leave those references dangling. Drop the \
                 referencing table too (`never_include`), or stop excluding the parent.",
                edges.len(),
                bullets(edges),
            ),
            Self::OutsideTableReferencesSampled { edges } => write!(
                f,
                "{} reference(s) come from a table outside the sample:\n{}\n  \
                 Nothing removes rows from a framework-owned table, so removing the rows \
                 it points at would break it. Empty it in the same run with \
                 `[framework] purge = [...]`.",
                edges.len(),
                bullets(edges),
            ),
            Self::PartitionLocalForeignKey { edges } => write!(
                f,
                "{} foreign key(s) are declared on a partition rather than on its \
                 partitioned parent:\n{}\n  \
                 The sample selects and removes a partition's rows through that parent, \
                 whose rows span every partition, so it cannot honour a key that binds \
                 one partition only. Declare the foreign key on the partitioned parent \
                 so PostgreSQL clones it to each partition, or drop the table with \
                 `never_include` in {SAMPLE_SECTION}.",
                edges.len(),
                bullets(edges),
            ),
            Self::RetainedReferencesPurged { edges } => write!(
                f,
                "{} reference(s) point from rows the sample keeps into a table \
                 `[framework] purge` empties:\n{}\n  \
                 Emptying the parent would leave the kept rows dangling, and no order of \
                 the two removals avoids it. Stop purging that table, or drop the \
                 referencing table with `never_include` in {SAMPLE_SECTION}.",
                edges.len(),
                bullets(edges),
            ),
            Self::NoRowKey { tables } => write!(
                f,
                "{} table(s) in the sample have no primary key:\n{}\n  \
                 Without one the sampler has no row identity to select on, and a \
                 full-copy table still needs one to join its parents. Add a primary key, \
                 or drop the table with `never_include` in {SAMPLE_SECTION}.",
                tables.len(),
                bullets(tables),
            ),
            Self::ForeignKeyCycle { tables } => write!(
                f,
                "{} table(s) reference each other in a cycle:\n{}\n  \
                 The row removals then have no order that keeps every foreign key \
                 satisfied at each step. Copy one of them whole (`[sample] \
                 always_include`), which takes it out of the removals altogether, or \
                 break the cycle on the copy before sampling.",
                tables.len(),
                bullets(tables),
            ),
            Self::IterationLimit => write!(
                f,
                "The sample selection did not settle after {MAX_PASSES} passes over the \
                 foreign key graph.\n  \
                 Nothing was written. Please report this with the schema that produced it."
            ),
            Self::IntegrityViolation { violations } => write!(
                f,
                "The sampled database failed its own foreign key check:\n{}\n  \
                 Nothing was written — the whole run was rolled back. This means the \
                 selection walk missed a reference; please report it.",
                bullets(violations),
            ),
        }
    }
}

/// The config section these rules are declared in, named once.
const SAMPLE_SECTION: &str = "`[sample]` in scrub.toml";

/// Render one item per indented line, matching the scrub's own diagnostics.
fn bullets(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("    - {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Plan ───────────────────────────────────────────────────────────────────

/// One table in the sampled universe.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleTable {
    /// The table name.
    pub table: String,
    /// What decides its rows.
    pub role: SampleRole,
    /// The primary-key columns that identify a row.
    pub key: Vec<String>,
    /// The temporary keep-set table name.
    pub keep: String,
    /// The temporary descend-set table name.
    pub descend: String,
}

/// The resolved sampling plan for one database.
#[derive(Debug, Clone, PartialEq)]
pub struct SamplePlan {
    /// The deterministic seed.
    pub seed: u64,
    /// Every table in the sampled universe, in DELETE order (children first).
    pub tables: Vec<SampleTable>,
    /// Foreign keys the closure walk follows.
    pub walk_edges: Vec<ForeignKeyConstraint>,
    /// Every foreign key inside the universe, for the integrity re-check.
    pub verify_edges: Vec<ForeignKeyConstraint>,
    /// Framework-owned tables whose `[framework] purge` must run AFTER the
    /// sample: a table the sample empties references them, so purging first
    /// would hit the reference. Every other purge still runs first, so a
    /// framework table referencing a sampled one is empty before its parents go.
    pub purge_after: BTreeSet<String>,
}

/// Everything the planner needs, gathered by the caller.
pub struct SampleInputs<'a> {
    /// Root specs from `--sample`, in command-line order.
    pub roots: &'a [SampleSpec],
    /// The deterministic seed.
    pub seed: u64,
    /// The `[sample]` rules.
    pub rules: &'a SampleRules,
    /// The classified user tables (partitions already excluded), as
    /// `(name, primary-key columns)`.
    pub tables: &'a [(String, Vec<String>)],
    /// Every foreign key in `public`.
    pub foreign_keys: &'a [ForeignKeyConstraint],
    /// Framework-owned tables present in the database.
    pub framework_tables: &'a BTreeSet<String>,
    /// Framework-owned tables `[framework] purge` empties.
    pub purged: &'a BTreeSet<String>,
    /// Partitions of a partitioned table. Their rows are selected and removed
    /// through the parent, and their foreign keys are clones of the parent's,
    /// so an edge naming one is skipped rather than double-counted.
    pub partitions: &'a BTreeSet<String>,
}

/// Parse one `--sample <table>=<count|percent%>` argument.
///
/// # Errors
///
/// Returns [`SampleError::InvalidSpec`] when the argument has no `=`, names an
/// empty table, or carries an amount that is not a positive count or a
/// percentage in `(0, 100]`.
pub fn parse_spec(raw: &str) -> Result<SampleSpec, SampleError> {
    let invalid = |detail: &str| SampleError::InvalidSpec {
        spec: raw.to_owned(),
        detail: detail.to_owned(),
    };
    let (table, amount) = raw
        .split_once('=')
        .ok_or_else(|| invalid("no `=` between the table and the amount"))?;
    let table = table.trim();
    if table.is_empty() {
        return Err(invalid("the table name is empty"));
    }
    let amount = amount.trim();
    if amount.is_empty() {
        return Err(invalid("the amount is empty"));
    }
    let amount = if let Some(percent) = amount.strip_suffix('%') {
        let value: f64 = percent
            .parse()
            .map_err(|_| invalid("the percentage is not a number"))?;
        if !(value.is_finite() && value > 0.0 && value <= 100.0) {
            return Err(invalid(
                "a percentage must be greater than 0 and at most 100",
            ));
        }
        SampleAmount::Percent(value)
    } else {
        let value: u64 = amount
            .parse()
            .map_err(|_| invalid("the count is not a whole number (add `%` for a percentage)"))?;
        if value == 0 {
            return Err(invalid("a root of 0 rows would select nothing"));
        }
        SampleAmount::Count(value)
    };
    Ok(SampleSpec {
        table: table.to_owned(),
        amount,
    })
}

impl SampleAmount {
    /// How many root rows this amount selects from a table of `total` rows.
    ///
    /// A percentage rounds **up**: 1% of ten rows is one row, not none — a
    /// sample that silently selected nothing would look like an empty database.
    #[must_use]
    pub fn rows(self, total: i64) -> i64 {
        let total = total.max(0);
        match self {
            Self::Count(n) => i64::try_from(n).unwrap_or(i64::MAX).min(total),
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            Self::Percent(pct) => {
                let wanted = (total as f64 * pct / 100.0).ceil();
                (wanted as i64).clamp(0, total)
            }
        }
    }
}

impl SampleRole {
    /// Whether rows of this table are removed at all.
    const fn is_subsetted(self) -> bool {
        matches!(self, Self::Root(_) | Self::Related | Self::NeverInclude)
    }

    /// Whether the walk may descend from this table into its children *at all*.
    ///
    /// A full-copy lookup table may not: descending from one `countries` row
    /// would pull in every user in that country, and the sample would stop
    /// being a sample. Whether a given table actually HAS descend-eligible rows
    /// is a second question — see [`Self::descends_from_seed`].
    const fn descends(self) -> bool {
        matches!(self, Self::Root(_) | Self::Related)
    }

    /// Whether this table starts out with descend-eligible rows, before the
    /// walk runs. Only a root does: every other descend source is one the walk
    /// descended into.
    const fn descends_from_seed(self) -> bool {
        matches!(self, Self::Root(_))
    }

    /// A one-word label for the report.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Root(_) => "root",
            Self::AlwaysInclude => "always-include",
            Self::NeverInclude => "never-include",
            Self::Related => "related",
        }
    }
}

/// Resolve the sampling plan, refusing every graph gap before a row is deleted.
///
/// # Errors
///
/// Returns the [`SampleError`] describing the first refusal.
pub fn build_plan(inputs: &SampleInputs<'_>) -> Result<SamplePlan, SampleError> {
    let keys: BTreeMap<&str, &Vec<String>> = inputs
        .tables
        .iter()
        .map(|(name, key)| (name.as_str(), key))
        .collect();

    check_rules(inputs, &keys)?;
    let roles = resolve_roles(inputs, &keys)?;
    let (internal, walk, purge_after) = classify_edges(inputs, &roles)?;
    check_coverage(&roles, &walk)?;
    check_row_keys(&roles, &keys)?;

    let numbered: BTreeMap<&str, usize> = keys
        .keys()
        .enumerate()
        .map(|(index, name)| (*name, index))
        .collect();
    let ordered = delete_order(&roles, &internal)?;
    let tables = ordered
        .into_iter()
        .map(|name| {
            let index = numbered[name.as_str()];
            SampleTable {
                role: roles[&name],
                key: keys[name.as_str()].clone(),
                keep: format!("_autumn_sample_keep_{index}"),
                descend: format!("_autumn_sample_desc_{index}"),
                table: name,
            }
        })
        .collect();

    Ok(SamplePlan {
        seed: inputs.seed,
        tables,
        walk_edges: walk,
        verify_edges: internal,
        purge_after,
    })
}

/// Validate `[sample]` against the live schema before anything is planned.
fn check_rules(
    inputs: &SampleInputs<'_>,
    keys: &BTreeMap<&str, &Vec<String>>,
) -> Result<(), SampleError> {
    let declared: Vec<&String> = inputs
        .rules
        .always_include
        .iter()
        .chain(&inputs.rules.never_include)
        .collect();

    let framework = sorted_unique(
        declared
            .iter()
            .filter(|t| inputs.framework_tables.contains(**t))
            .map(|t| (*t).clone()),
    );
    if !framework.is_empty() {
        return Err(SampleError::FrameworkRule { tables: framework });
    }

    let stale = sorted_unique(
        declared
            .iter()
            .filter(|t| !keys.contains_key(t.as_str()))
            .map(|t| (*t).clone()),
    );
    if !stale.is_empty() {
        return Err(SampleError::StaleRule { tables: stale });
    }

    let never: BTreeSet<&String> = inputs.rules.never_include.iter().collect();
    let both = sorted_unique(
        inputs
            .rules
            .always_include
            .iter()
            .filter(|t| never.contains(*t))
            .cloned(),
    );
    if !both.is_empty() {
        return Err(SampleError::RuleContradiction { tables: both });
    }
    Ok(())
}

/// Assign every table its role, refusing unusable roots.
fn resolve_roles(
    inputs: &SampleInputs<'_>,
    keys: &BTreeMap<&str, &Vec<String>>,
) -> Result<BTreeMap<String, SampleRole>, SampleError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for spec in inputs.roots {
        if !seen.insert(spec.table.as_str()) {
            return Err(SampleError::DuplicateRoot {
                table: spec.table.clone(),
            });
        }
    }
    let unknown = sorted_unique(
        inputs
            .roots
            .iter()
            .filter(|s| !keys.contains_key(s.table.as_str()))
            .map(|s| s.table.clone()),
    );
    if !unknown.is_empty() {
        return Err(SampleError::UnknownRoot { tables: unknown });
    }
    let never: BTreeSet<&String> = inputs.rules.never_include.iter().collect();
    let excluded = sorted_unique(
        inputs
            .roots
            .iter()
            .filter(|s| never.contains(&s.table))
            .map(|s| s.table.clone()),
    );
    if !excluded.is_empty() {
        return Err(SampleError::RootNeverIncluded { tables: excluded });
    }

    let always: BTreeSet<&String> = inputs.rules.always_include.iter().collect();
    // A root silently overriding `always_include` would subset a table declared
    // full-copy AND make it a descend source, which is the blow-up that rule
    // exists to prevent. Refuse it, exactly as a `never_include` root is
    // refused.
    let copied_whole = sorted_unique(
        inputs
            .roots
            .iter()
            .filter(|s| always.contains(&s.table))
            .map(|s| s.table.clone()),
    );
    if !copied_whole.is_empty() {
        return Err(SampleError::RootAlwaysIncluded {
            tables: copied_whole,
        });
    }

    let mut roles: BTreeMap<String, SampleRole> = keys
        .keys()
        .map(|name| {
            let owned = (*name).to_owned();
            let role = if never.contains(&owned) {
                SampleRole::NeverInclude
            } else if always.contains(&owned) {
                SampleRole::AlwaysInclude
            } else {
                SampleRole::Related
            };
            (owned, role)
        })
        .collect();
    for spec in inputs.roots {
        roles.insert(spec.table.clone(), SampleRole::Root(spec.amount));
    }
    Ok(roles)
}

/// Split every foreign key into the ones inside the sample and the ones the
/// walk follows, refusing the two shapes that would dangle.
type ClassifiedEdges = (
    Vec<ForeignKeyConstraint>,
    Vec<ForeignKeyConstraint>,
    BTreeSet<String>,
);

fn classify_edges(
    inputs: &SampleInputs<'_>,
    roles: &BTreeMap<String, SampleRole>,
) -> Result<ClassifiedEdges, SampleError> {
    let describe = |edge: &ForeignKeyConstraint| {
        format!(
            "{} -> {} ({})",
            edge.child_table, edge.parent_table, edge.name
        )
    };

    let mut outside_refs = Vec::new();
    let mut dangling = Vec::new();
    let mut partition_local = Vec::new();
    let mut retained_into_purged = Vec::new();
    let mut purge_after = BTreeSet::new();
    let mut internal = Vec::new();
    for edge in inputs.foreign_keys {
        // A constraint cloned from a partitioned parent was already dropped by
        // the caller, which can tell a clone from a partition-local key by its
        // catalog parentage. Anything still naming a partition is therefore
        // declared on that partition alone — an edge the plan cannot express,
        // because it keys every partition's rows on the parent.
        if inputs.partitions.contains(&edge.child_table)
            || inputs.partitions.contains(&edge.parent_table)
        {
            partition_local.push(describe(edge));
            continue;
        }
        let child = roles.get(&edge.child_table);
        let parent = roles.get(&edge.parent_table);
        match (child, parent) {
            (Some(child_role), Some(parent_role)) => {
                if *parent_role == SampleRole::NeverInclude
                    && *child_role != SampleRole::NeverInclude
                {
                    dangling.push(describe(edge));
                }
                internal.push(edge.clone());
            }
            // Nothing removes rows from a table outside the sampled universe —
            // in practice a framework-owned one — but the rows it points at ARE
            // removed, unless the parent is copied whole or that table is
            // emptied in the same run. Keyed on "outside the universe" rather
            // than on the framework-table list, which holds only the names the
            // scrub probes for: a name missing from it would otherwise reach
            // the deletes and fail as a raw constraint violation.
            (None, Some(parent_role))
                if parent_role.is_subsetted() && !inputs.purged.contains(&edge.child_table) =>
            {
                outside_refs.push(describe(edge));
            }
            // The mirror image: a table the sample removes rows from points INTO
            // a purged framework table. Purges run before the sample so the case
            // above holds, which would empty the parent while these rows still
            // reference it — so this one purge has to wait until the sample has
            // emptied its child. That is only possible when the sample empties
            // the child completely; if it keeps rows, no order satisfies the key.
            (Some(child_role), None) if inputs.purged.contains(&edge.parent_table) => {
                if *child_role == SampleRole::NeverInclude {
                    purge_after.insert(edge.parent_table.clone());
                } else {
                    retained_into_purged.push(describe(edge));
                }
            }
            _ => {}
        }
    }

    if !partition_local.is_empty() {
        partition_local.sort();
        return Err(SampleError::PartitionLocalForeignKey {
            edges: partition_local,
        });
    }
    if !retained_into_purged.is_empty() {
        retained_into_purged.sort();
        return Err(SampleError::RetainedReferencesPurged {
            edges: retained_into_purged,
        });
    }

    if !outside_refs.is_empty() {
        outside_refs.sort();
        return Err(SampleError::OutsideTableReferencesSampled {
            edges: outside_refs,
        });
    }
    if !dangling.is_empty() {
        dangling.sort();
        return Err(SampleError::NeverIncludeReferenced { edges: dangling });
    }

    let walk = internal
        .iter()
        .filter(|edge| {
            roles[&edge.child_table] != SampleRole::NeverInclude
                && roles[&edge.parent_table] != SampleRole::NeverInclude
        })
        .cloned()
        .collect();
    Ok((internal, walk, purge_after))
}

/// Refuse any table rows can never flow into: sampling it would empty it
/// silently, which is the one outcome AC #5 forbids.
///
/// This mirrors the runtime walk exactly rather than asking the looser question
/// "is the table connected to anything". Two sets grow together:
///
/// - **descend sources** — a root, or a table the walk descended into. Only
///   these have descend-eligible rows, so only these pull their children in. A
///   table reached purely by ascent (a shared org a kept user points at) is
///   NOT one, which is what stops one such row from dragging its whole subtree
///   back in — and therefore what leaves its other children unreachable.
/// - **covered** — a table rows actually flow into: a root, a full-copy table,
///   a child of a descend source, or the parent of a covered table.
fn check_coverage(
    roles: &BTreeMap<String, SampleRole>,
    walk: &[ForeignKeyConstraint],
) -> Result<(), SampleError> {
    let mut descend: BTreeSet<&str> = roles
        .iter()
        .filter(|(_, role)| role.descends_from_seed())
        .map(|(name, _)| name.as_str())
        .collect();
    let mut covered: BTreeSet<&str> = roles
        .iter()
        .filter(|(_, role)| matches!(role, SampleRole::Root(_) | SampleRole::AlwaysInclude))
        .map(|(name, _)| name.as_str())
        .collect();

    let mut grew = true;
    while grew {
        grew = false;
        for edge in walk {
            let (child, parent) = (edge.child_table.as_str(), edge.parent_table.as_str());
            if descend.contains(parent) && roles[child].descends() {
                grew |= descend.insert(child);
            }
            if descend.contains(parent) {
                grew |= covered.insert(child);
            }
            if covered.contains(child) {
                grew |= covered.insert(parent);
            }
        }
    }

    let uncovered = sorted_unique(
        roles
            .iter()
            .filter(|(name, role)| {
                **role == SampleRole::Related && !covered.contains(name.as_str())
            })
            .map(|(name, _)| name.clone()),
    );
    if uncovered.is_empty() {
        Ok(())
    } else {
        Err(SampleError::UncoveredTables { tables: uncovered })
    }
}

/// Every table the sample keeps rows of needs a primary key to identify them.
fn check_row_keys(
    roles: &BTreeMap<String, SampleRole>,
    keys: &BTreeMap<&str, &Vec<String>>,
) -> Result<(), SampleError> {
    let missing = sorted_unique(
        roles
            .iter()
            .filter(|(name, role)| {
                **role != SampleRole::NeverInclude && keys[name.as_str()].is_empty()
            })
            .map(|(name, _)| name.clone()),
    );
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SampleError::NoRowKey { tables: missing })
    }
}

/// Order the tables so a child is always emptied before its parent, which is
/// what keeps every foreign key satisfied at each step.
///
/// A self-reference is fine — a row and the row it points at are removed by the
/// same statement, and the constraint is checked when that statement ends — but
/// a cycle between distinct tables has no such order and is refused.
fn delete_order(
    roles: &BTreeMap<String, SampleRole>,
    edges: &[ForeignKeyConstraint],
) -> Result<Vec<String>, SampleError> {
    // Only the tables rows are removed from need an order. A full-copy table
    // keeps every row, so nothing can dangle through it — which is also why
    // declaring one `always_include` is a real way out of a cycle.
    let removed: BTreeSet<&str> = roles
        .iter()
        .filter(|(_, role)| role.is_subsetted())
        .map(|(name, _)| name.as_str())
        .collect();
    let mut parents: BTreeMap<&str, BTreeSet<&str>> =
        removed.iter().map(|n| (*n, BTreeSet::new())).collect();
    let mut incoming: BTreeMap<&str, BTreeSet<&str>> =
        removed.iter().map(|n| (*n, BTreeSet::new())).collect();
    for edge in edges {
        let (child, parent) = (edge.child_table.as_str(), edge.parent_table.as_str());
        if child == parent || !removed.contains(child) || !removed.contains(parent) {
            continue;
        }
        parents.entry(child).or_default().insert(parent);
        incoming.entry(parent).or_default().insert(child);
    }

    // Kahn's algorithm, alphabetical among the ready nodes so the order — and
    // therefore the emitted SQL — is identical on every run.
    let mut ready: BTreeSet<&str> = incoming
        .iter()
        .filter(|(_, children)| children.is_empty())
        .map(|(name, _)| *name)
        .collect();
    let mut remaining = incoming.clone();
    let mut ordered: Vec<String> = Vec::with_capacity(roles.len());
    while let Some(next) = ready.iter().next().copied() {
        ready.remove(next);
        ordered.push(next.to_owned());
        for parent in &parents[next] {
            let children = remaining.get_mut(parent).expect("edge target is a table");
            children.remove(next);
            if children.is_empty() {
                ready.insert(parent);
            }
        }
        remaining.remove(next);
    }

    if ordered.len() == removed.len() {
        // The full-copy tables are never deleted from, so they carry no ordering
        // constraint — but the plan still needs an entry for each (their
        // keep-sets are what the walk ascends from).
        ordered.extend(
            roles
                .iter()
                .filter(|(_, role)| !role.is_subsetted())
                .map(|(name, _)| name.clone()),
        );
        return Ok(ordered);
    }

    // What is left is the cycle plus everything downstream of it. Kahn already
    // stripped the nodes with no children left; strip the ones with no parents
    // left too, and only the tables actually in the cycle remain — naming a
    // blameless parent alongside them would send the reader to the wrong table.
    let mut residue: BTreeSet<&str> = remaining.keys().copied().collect();
    loop {
        let sinks: Vec<&str> = residue
            .iter()
            .filter(|name| !parents[**name].iter().any(|p| residue.contains(p)))
            .copied()
            .collect();
        if sinks.is_empty() {
            break;
        }
        for sink in sinks {
            residue.remove(sink);
        }
    }
    Err(SampleError::ForeignKeyCycle {
        tables: sorted_unique(residue.into_iter().map(str::to_owned)),
    })
}

/// Collect into a sorted, de-duplicated list — the shape every diagnostic uses.
fn sorted_unique(items: impl Iterator<Item = String>) -> Vec<String> {
    items.collect::<BTreeSet<_>>().into_iter().collect()
}

// ─── SQL ────────────────────────────────────────────────────────────────────

/// The expression identifying one row of `key` under `alias`.
///
/// A single-column key stays in its own type, so the temporary keep-set keeps
/// an indexable native column. A composite key becomes text through
/// `quote_nullable`, which escapes each component — so `('a,b', 'c')` and
/// `('a', 'b,c')` cannot collapse onto the same key.
fn key_expr(alias: &str, key: &[String]) -> String {
    if let [single] = key {
        return format!("{alias}.{}", quote_ident(single));
    }
    let parts: Vec<String> = key
        .iter()
        .map(|column| format!("quote_nullable({alias}.{})", quote_ident(column)))
        .collect();
    format!("({})", parts.join(" || ',' || "))
}

/// A constraint name rendered safe for a SQL block comment.
///
/// The name comes from `pg_constraint`, which is catalog text rather than a
/// literal: `*/` inside it would close the comment and let whatever follows
/// run as part of the statement. It is a label, so anything outside a plain
/// identifier alphabet becomes `_`.
fn comment_safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The `ON` clause joining a child to its parent across every key component.
fn join_on(edge: &ForeignKeyConstraint, child: &str, parent: &str) -> String {
    edge.child_columns
        .iter()
        .zip(&edge.parent_columns)
        .map(|(c, p)| format!("{parent}.{} = {child}.{}", quote_ident(p), quote_ident(c)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

impl SamplePlan {
    /// The table entry for `name`.
    fn table(&self, name: &str) -> &SampleTable {
        self.tables
            .iter()
            .find(|t| t.table == name)
            .expect("every edge names a table in the plan")
    }

    /// The tables that need a keep-set at all.
    ///
    /// An excluded table is emptied by a bare `DELETE`, so it needs no row
    /// identity — which is why it is also the one table allowed to have no
    /// primary key. Building it a set anyway would render an empty key
    /// expression and emit invalid SQL.
    fn keyed_tables(&self) -> impl Iterator<Item = &SampleTable> {
        self.tables
            .iter()
            .filter(|t| t.role != SampleRole::NeverInclude)
    }

    /// `CREATE TEMP TABLE` for every keep- and descend-set.
    ///
    /// `AS SELECT … WITH NO DATA` copies the key's own type rather than casting
    /// everything to text, so the join back to the source table can use its
    /// primary-key index. `ON COMMIT DROP` ties every set to the scrub's
    /// transaction: a rollback leaves nothing behind.
    #[must_use]
    pub fn setup_statements(&self) -> Vec<String> {
        self.keyed_tables()
            .flat_map(|table| {
                [&table.keep, &table.descend].map(|set| {
                    format!(
                        "CREATE TEMPORARY TABLE {} ON COMMIT DROP AS \
                         SELECT {} AS k FROM {} AS t WITH NO DATA",
                        quote_ident(set),
                        key_expr("t", &table.key),
                        qualified(&table.table),
                    )
                })
            })
            .collect()
    }

    /// Index and analyse every set, run AFTER the roots are seeded.
    ///
    /// Order matters twice over: an index built on an empty table is built for
    /// nothing, and autovacuum never touches a temporary table — so without an
    /// explicit `ANALYZE` the planner sizes every set at its 10-page default
    /// and picks a hash join over the index path for the whole first pass.
    #[must_use]
    pub fn index_statements(&self) -> Vec<String> {
        let mut out = Vec::new();
        for table in self.keyed_tables() {
            for set in [&table.keep, &table.descend] {
                out.push(format!("CREATE INDEX ON {}(k)", quote_ident(set)));
                out.push(format!("ANALYZE {}", quote_ident(set)));
            }
        }
        out
    }

    /// Re-analyse every set, run after each closure pass so the next pass plans
    /// against the sizes it will actually see.
    #[must_use]
    pub fn analyze_statements(&self) -> Vec<String> {
        self.keyed_tables()
            .flat_map(|table| {
                [&table.keep, &table.descend].map(|set| format!("ANALYZE {}", quote_ident(set)))
            })
            .collect()
    }

    /// Seed the roots (deterministically) and the always-include tables.
    ///
    /// A root's rows are ordered by a hash of the seed and the row key — not by
    /// physical order and not by `random()` — so the same seed against the same
    /// source data selects the identical rows, and `LIMIT` keeps the sort
    /// bounded (Postgres uses a top-N heapsort) on a table of any size.
    #[must_use]
    pub fn seed_statements(&self, counts: &BTreeMap<String, i64>) -> Vec<String> {
        let seed = quote_literal(&self.seed.to_string());
        let mut out = Vec::new();
        for table in &self.tables {
            let key = key_expr("t", &table.key);
            match table.role {
                SampleRole::Root(amount) => {
                    let rows = amount.rows(counts.get(&table.table).copied().unwrap_or(0));
                    out.push(format!(
                        "INSERT INTO {} (k) SELECT k FROM (\
                         SELECT {key} AS k FROM {} AS t \
                         ORDER BY md5({seed} || '|' || ({key})::text), ({key})::text \
                         LIMIT {rows}) AS chosen",
                        quote_ident(&table.keep),
                        qualified(&table.table),
                    ));
                    out.push(format!(
                        "INSERT INTO {} (k) SELECT k FROM {}",
                        quote_ident(&table.descend),
                        quote_ident(&table.keep),
                    ));
                }
                SampleRole::AlwaysInclude => out.push(format!(
                    "INSERT INTO {} (k) SELECT {key} FROM {} AS t",
                    quote_ident(&table.keep),
                    qualified(&table.table),
                )),
                SampleRole::NeverInclude | SampleRole::Related => {}
            }
        }
        out
    }

    /// One closure pass: ascend then descend over every walked edge. Run
    /// repeatedly until a pass selects nothing new.
    #[must_use]
    pub fn walk_statements(&self) -> Vec<String> {
        let mut out = Vec::new();
        for edge in &self.walk_edges {
            let child = self.table(&edge.child_table);
            let parent = self.table(&edge.parent_table);
            let child_key = key_expr("c", &child.key);
            let parent_key = key_expr("p", &parent.key);
            let on = join_on(edge, "c", "p");
            let tag = format!("/* {} */ ", comment_safe(&edge.name));

            // Ascend: keep the parent of every kept child, so the reference
            // resolves. A full-copy parent already holds every row.
            if parent.role != SampleRole::AlwaysInclude {
                out.push(format!(
                    "{tag}INSERT INTO {keep} (k) SELECT DISTINCT {parent_key} \
                     FROM {parent_table} AS p \
                     JOIN {child_table} AS c ON {on} \
                     JOIN {child_keep} AS ck ON ck.k = {child_key} \
                     WHERE NOT EXISTS (SELECT 1 FROM {keep} AS existing \
                     WHERE existing.k = {parent_key})",
                    keep = quote_ident(&parent.keep),
                    parent_table = qualified(&parent.table),
                    child_table = qualified(&child.table),
                    child_keep = quote_ident(&child.keep),
                ));
            }

            // Descend: keep every child of a descend-eligible parent, and make
            // those children descend-eligible in turn. A full-copy child already
            // holds every row, and nothing ever descends out of it, so neither
            // of its sets is worth filling.
            if parent.role.descends() && child.role.descends() {
                for set in [&child.keep, &child.descend] {
                    out.push(format!(
                        "{tag}INSERT INTO {set} (k) SELECT DISTINCT {child_key} \
                         FROM {child_table} AS c \
                         JOIN {parent_table} AS p ON {on} \
                         JOIN {parent_descend} AS pd ON pd.k = {parent_key} \
                         WHERE NOT EXISTS (SELECT 1 FROM {set} AS existing \
                         WHERE existing.k = {child_key})",
                        set = quote_ident(set),
                        child_table = qualified(&child.table),
                        parent_table = qualified(&parent.table),
                        parent_descend = quote_ident(&parent.descend),
                    ));
                }
            }
        }
        out
    }

    /// The row-removing `DELETE`s, children before parents.
    #[must_use]
    pub fn delete_statements(&self) -> Vec<String> {
        self.tables
            .iter()
            .filter_map(|table| match table.role {
                SampleRole::AlwaysInclude => None,
                SampleRole::NeverInclude => {
                    Some(format!("DELETE FROM {}", qualified(&table.table)))
                }
                SampleRole::Root(_) | SampleRole::Related => Some(format!(
                    "DELETE FROM {} AS t WHERE NOT EXISTS \
                     (SELECT 1 FROM {} AS k WHERE k.k = {})",
                    qualified(&table.table),
                    quote_ident(&table.keep),
                    key_expr("t", &table.key),
                )),
            })
            .collect()
    }

    /// The tables the sample removes rows from, so the caller can rewrite their
    /// files afterwards. A full-copy table is untouched and needs no rewrite.
    #[must_use]
    pub fn subsetted_tables(&self) -> Vec<&str> {
        self.tables
            .iter()
            .filter(|t| t.role.is_subsetted())
            .map(|t| t.table.as_str())
            .collect()
    }

    /// Every table the sample reads or writes, which the scrub locks for the
    /// duration so a row inserted mid-run cannot escape the subset — a
    /// full-copy table included, because the walk reads it to decide which
    /// parents to keep.
    #[must_use]
    pub fn locked_tables(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.table.as_str()).collect()
    }

    /// One orphan-counting query per foreign key, as `(label, sql)`.
    ///
    /// Postgres enforces these constraints itself, so this is a second opinion
    /// rather than the only one — and it is the one that catches a constraint
    /// left `NOT VALID` by a migration, which the server does not re-check.
    #[must_use]
    pub fn integrity_statements(&self) -> Vec<(String, String)> {
        self.verify_edges
            .iter()
            .map(|edge| {
                let on = join_on(edge, "c", "p");
                // Under the default MATCH SIMPLE a composite reference with any
                // NULL component is satisfied by definition, so only
                // fully-populated references are checked for a missing parent.
                let populated = edge
                    .child_columns
                    .iter()
                    .map(|c| format!("c.{} IS NOT NULL", quote_ident(c)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let missing = format!("p.{} IS NULL", quote_ident(&edge.parent_columns[0]));
                // MATCH FULL admits only all-NULL or all-populated tuples, so a
                // partially-NULL one is itself a violation — and one Postgres
                // will not re-check for us on a constraint a migration left
                // `NOT VALID`, which is exactly what this recount is for.
                let mixed_null = if edge.match_full && edge.child_columns.len() > 1 {
                    let any_null = edge
                        .child_columns
                        .iter()
                        .map(|c| format!("c.{} IS NULL", quote_ident(c)))
                        .collect::<Vec<_>>()
                        .join(" OR ");
                    Some(format!(
                        "({any_null}) AND ({populated_any})",
                        populated_any = edge
                            .child_columns
                            .iter()
                            .map(|c| format!("c.{} IS NOT NULL", quote_ident(c)))
                            .collect::<Vec<_>>()
                            .join(" OR ")
                    ))
                } else {
                    None
                };
                (
                    format!(
                        "{} ({} -> {})",
                        edge.name, edge.child_table, edge.parent_table
                    ),
                    format!(
                        "SELECT count(*) AS n FROM {} AS c \
                         LEFT JOIN {} AS p ON {on} \
                         WHERE ({populated} AND {missing}){extra}",
                        qualified(&edge.child_table),
                        qualified(&edge.parent_table),
                        extra = mixed_null
                            .as_ref()
                            .map_or_else(String::new, |m| format!(" OR ({m})")),
                    ),
                )
            })
            .collect()
    }
}

// ─── Execution ──────────────────────────────────────────────────────────────

/// A single `count(*)`.
#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    n: i64,
}

/// What one table contributed to the sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleCount {
    /// The table name.
    pub table: String,
    /// Why it holds the rows it does (`root`, `related`, …).
    pub role: &'static str,
    /// Rows in the source.
    pub before: i64,
    /// Rows in the sample.
    pub after: i64,
}

/// What one sampled database ended up with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleOutcome {
    /// Per-table row counts, in delete order.
    pub counts: Vec<SampleCount>,
    /// Closure passes the walk needed.
    pub passes: usize,
    /// Foreign keys re-verified after the deletes.
    pub verified: usize,
    /// Total relation size of the sampled tables before the deletes, in bytes.
    pub size_before: i64,
}

/// The on-disk size of every table the sample touches, indexes and TOAST
/// included.
///
/// Deliberately not `pg_database_size`: that is dominated by the system
/// catalogs and template data on a small database, so it would report a 99%
/// "saving" of nothing. The question a sample answers is how much of the app's
/// own data the copy carries.
///
/// # Errors
///
/// Returns the database error.
pub fn data_size(conn: &mut PgConnection, plan: &SamplePlan) -> Result<i64, diesel::result::Error> {
    let names = plan
        .tables
        .iter()
        .map(|t| quote_literal(&t.table))
        .collect::<Vec<_>>()
        .join(", ");
    if names.is_empty() {
        return Ok(0);
    }
    // Walked through `pg_inherits` rather than measured directly: a
    // partitioned parent holds no rows of its own, so `pg_total_relation_size`
    // on it is zero and a partitioned schema would report a size of nothing.
    let rows: Vec<CountRow> = sql_query(format!(
        "WITH RECURSIVE named AS ( \
           SELECT c.oid FROM pg_class c \
           JOIN pg_namespace ns ON ns.oid = c.relnamespace AND ns.nspname = 'public' \
           WHERE c.relname IN ({names}) \
         ), tree AS ( \
           SELECT oid FROM named \
           UNION \
           SELECT i.inhrelid FROM pg_inherits i JOIN tree t ON t.oid = i.inhparent \
         ) \
         SELECT coalesce(sum(pg_total_relation_size(oid)), 0)::bigint AS n FROM tree"
    ))
    .load(conn)?;
    Ok(rows.first().map_or(0, |r| r.n))
}

/// Live row counts for every table in the sample, which size the roots and
/// anchor the report.
///
/// # Errors
///
/// Returns the database error.
pub fn source_counts(
    conn: &mut PgConnection,
    plan: &SamplePlan,
) -> Result<BTreeMap<String, i64>, diesel::result::Error> {
    let mut counts = BTreeMap::new();
    for table in &plan.tables {
        counts.insert(table.table.clone(), count_rows(conn, &table.table)?);
    }
    Ok(counts)
}

/// The row count of one table.
fn count_rows(conn: &mut PgConnection, table: &str) -> Result<i64, diesel::result::Error> {
    let rows: Vec<CountRow> =
        sql_query(format!("SELECT count(*) AS n FROM {}", qualified(table))).load(conn)?;
    Ok(rows.first().map_or(0, |r| r.n))
}

/// Why a sample run inside the scrub's transaction ended.
///
/// The two channels are separate so a refusal cannot be mistaken for a database
/// failure: `diesel` insists a transaction closure's error type be built from
/// its own, which would otherwise flatten a `SampleError` into an opaque
/// rollback.
#[derive(Debug)]
pub enum SampleFailure {
    /// The sample itself was refused; the transaction rolls back.
    Refused(SampleError),
    /// The database rejected a statement.
    Db(diesel::result::Error),
}

impl From<diesel::result::Error> for SampleFailure {
    fn from(e: diesel::result::Error) -> Self {
        Self::Db(e)
    }
}

/// Run the sample inside the scrub's own transaction.
///
/// A refusal rolls the transaction back, so nothing is ever left half-sampled —
/// and, because this runs before the scrub's own rewrites in the same
/// transaction, no rows can be committed sampled but unscrubbed.
///
/// # Errors
///
/// Returns [`SampleFailure::Refused`] when the sample is refused, or
/// [`SampleFailure::Db`] when a statement fails.
pub fn apply(conn: &mut PgConnection, plan: &SamplePlan) -> Result<SampleOutcome, SampleFailure> {
    let size_before = data_size(conn, plan)?;

    let before = source_counts(conn, plan)?;

    for statement in plan.setup_statements() {
        sql_query(statement).execute(conn)?;
    }
    for statement in plan.seed_statements(&before) {
        sql_query(statement).execute(conn)?;
    }
    for statement in plan.index_statements() {
        sql_query(statement).execute(conn)?;
    }

    let walk = plan.walk_statements();
    let analyze = plan.analyze_statements();
    let mut passes = 0;
    loop {
        let mut selected = 0;
        for statement in &walk {
            selected += sql_query(statement).execute(conn)?;
        }
        for statement in &analyze {
            sql_query(statement).execute(conn)?;
        }
        passes += 1;
        if selected == 0 {
            break;
        }
        if passes >= MAX_PASSES {
            return Err(SampleFailure::Refused(SampleError::IterationLimit));
        }
    }

    for statement in plan.delete_statements() {
        sql_query(statement).execute(conn)?;
    }

    let mut violations = Vec::new();
    let checks = plan.integrity_statements();
    for (label, sql) in &checks {
        let orphans = sql_query(sql)
            .load::<CountRow>(conn)?
            .first()
            .map_or(0, |r| r.n);
        if orphans > 0 {
            violations.push(format!("{label}: {orphans} unresolved reference(s)"));
        }
    }
    if !violations.is_empty() {
        violations.sort();
        return Err(SampleFailure::Refused(SampleError::IntegrityViolation {
            violations,
        }));
    }

    let mut counts = Vec::with_capacity(plan.tables.len());
    for table in &plan.tables {
        counts.push(SampleCount {
            role: table.role.as_str(),
            before: before[&table.table],
            after: count_rows(conn, &table.table)?,
            table: table.table.clone(),
        });
    }

    Ok(SampleOutcome {
        counts,
        passes,
        verified: checks.len(),
        size_before,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Fixtures ────────────────────────────────────────────────────────────

    fn table(name: &str, key: &[&str]) -> (String, Vec<String>) {
        (
            name.to_owned(),
            key.iter().map(|k| (*k).to_owned()).collect(),
        )
    }

    fn fk(
        name: &str,
        child: &str,
        child_col: &str,
        parent: &str,
        parent_col: &str,
    ) -> ForeignKeyConstraint {
        ForeignKeyConstraint {
            name: name.to_owned(),
            child_table: child.to_owned(),
            child_columns: vec![child_col.to_owned()],
            parent_table: parent.to_owned(),
            parent_columns: vec![parent_col.to_owned()],
            match_full: false,
        }
    }

    fn root(table: &str, amount: SampleAmount) -> SampleSpec {
        SampleSpec {
            table: table.to_owned(),
            amount,
        }
    }

    /// `users(id) ← comments(user_id)`, plus a `countries` lookup `users`
    /// references and an unrelated `audit_logs`.
    fn schema() -> (Vec<(String, Vec<String>)>, Vec<ForeignKeyConstraint>) {
        (
            vec![
                table("users", &["id"]),
                table("comments", &["id"]),
                table("countries", &["id"]),
                table("audit_logs", &["id"]),
            ],
            vec![
                fk("comments_user_fk", "comments", "user_id", "users", "id"),
                fk("users_country_fk", "users", "country_id", "countries", "id"),
            ],
        )
    }

    fn plan_of(
        roots: &[SampleSpec],
        rules: &SampleRules,
        tables: &[(String, Vec<String>)],
        foreign_keys: &[ForeignKeyConstraint],
    ) -> Result<SamplePlan, SampleError> {
        build_plan(&SampleInputs {
            roots,
            seed: 7,
            rules,
            tables,
            foreign_keys,
            framework_tables: &BTreeSet::new(),
            purged: &BTreeSet::new(),
            partitions: &BTreeSet::new(),
        })
    }

    /// The default rules for the fixture schema: `countries` in full,
    /// `audit_logs` dropped — without them the fixture has an uncovered table.
    fn fixture_rules() -> SampleRules {
        SampleRules {
            always_include: vec!["countries".to_owned()],
            never_include: vec!["audit_logs".to_owned()],
        }
    }

    fn role_of(plan: &SamplePlan, table: &str) -> SampleRole {
        plan.tables
            .iter()
            .find(|t| t.table == table)
            .unwrap_or_else(|| panic!("{table} is not in the plan"))
            .role
    }

    fn joined(statements: &[String]) -> String {
        statements.join("\n")
    }

    // ── Spec parsing ────────────────────────────────────────────────────────

    #[test]
    fn spec_parses_a_percentage() {
        assert_eq!(
            parse_spec("users=1%").unwrap(),
            root("users", SampleAmount::Percent(1.0))
        );
        assert_eq!(
            parse_spec("users=2.5%").unwrap(),
            root("users", SampleAmount::Percent(2.5))
        );
    }

    #[test]
    fn spec_parses_an_absolute_count() {
        assert_eq!(
            parse_spec("users=500").unwrap(),
            root("users", SampleAmount::Count(500))
        );
    }

    #[test]
    fn spec_rejects_malformed_amounts() {
        for bad in [
            "users",      // no amount
            "=1%",        // no table
            "users=",     // empty amount
            "users=0",    // an empty root selects nothing
            "users=0%",   //
            "users=101%", // more than the whole table
            "users=-5",
            "users=abc",
            "users=1.5", // fractional rows only make sense as a percentage
        ] {
            assert!(
                matches!(parse_spec(bad), Err(SampleError::InvalidSpec { .. })),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_percentage_rounds_up_to_at_least_one_row() {
        // 1% of 10 rows is 0.1 — a sample that selected nothing would be a
        // silently empty database, so it rounds up.
        assert_eq!(SampleAmount::Percent(1.0).rows(10), 1);
        assert_eq!(SampleAmount::Percent(1.0).rows(1000), 10);
        assert_eq!(SampleAmount::Percent(100.0).rows(1000), 1000);
        // An empty source table stays empty.
        assert_eq!(SampleAmount::Percent(50.0).rows(0), 0);
    }

    #[test]
    fn a_count_is_capped_at_the_source_row_count() {
        assert_eq!(SampleAmount::Count(500).rows(1000), 500);
        assert_eq!(SampleAmount::Count(5000).rows(1000), 1000);
    }

    // ── Roles and coverage ──────────────────────────────────────────────────

    #[test]
    fn plan_assigns_root_always_never_and_derived_roles() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        assert_eq!(
            role_of(&plan, "users"),
            SampleRole::Root(SampleAmount::Percent(1.0))
        );
        assert_eq!(role_of(&plan, "comments"), SampleRole::Related);
        assert_eq!(role_of(&plan, "countries"), SampleRole::AlwaysInclude);
        assert_eq!(role_of(&plan, "audit_logs"), SampleRole::NeverInclude);
    }

    #[test]
    fn plan_refuses_a_table_no_root_can_reach() {
        let (tables, keys) = schema();
        // `audit_logs` is connected to nothing and declared nothing: sampling
        // would empty it silently, which is exactly what AC #5 forbids.
        let err = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &SampleRules {
                always_include: vec!["countries".to_owned()],
                never_include: Vec::new(),
            },
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::UncoveredTables {
                tables: vec!["audit_logs".to_owned()]
            }
        );
    }

    #[test]
    fn plan_covers_a_table_reachable_only_through_a_lookup_table() {
        // `regions` is reachable from the always-include `countries`, not from
        // the root — the walk still covers it, so it is not refused.
        let mut tables = schema().0;
        tables.push(table("regions", &["id"]));
        let mut keys = schema().1;
        keys.push(fk(
            "countries_region_fk",
            "countries",
            "region_id",
            "regions",
            "id",
        ));
        let plan = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        assert_eq!(role_of(&plan, "regions"), SampleRole::Related);
    }

    #[test]
    fn plan_refuses_a_table_hanging_off_a_table_the_walk_only_ascended_into() {
        // `orgs` is reached by ascent (a kept user points at it), so its rows
        // are never descended from — which means nothing reaches `org_settings`
        // and it would be emptied silently. Coverage has to model that, not
        // just "is it connected to something".
        let (mut tables, mut keys) = schema();
        tables.push(table("orgs", &["id"]));
        tables.push(table("org_settings", &["id"]));
        keys.push(fk("users_org_fk", "users", "org_id", "orgs", "id"));
        keys.push(fk(
            "org_settings_org_fk",
            "org_settings",
            "org_id",
            "orgs",
            "id",
        ));
        let err = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::UncoveredTables {
                tables: vec!["org_settings".to_owned()]
            }
        );
    }

    #[test]
    fn plan_covers_a_table_below_a_root_through_two_hops() {
        // The other side of the same rule: descent is transitive, so a
        // grandchild of a root is covered without any declaration.
        let (mut tables, mut keys) = schema();
        tables.push(table("comment_votes", &["id"]));
        keys.push(fk(
            "comment_votes_comment_fk",
            "comment_votes",
            "comment_id",
            "comments",
            "id",
        ));
        let plan = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        assert_eq!(role_of(&plan, "comment_votes"), SampleRole::Related);
    }

    #[test]
    fn plan_refuses_a_reference_into_a_never_include_table() {
        let (mut tables, mut keys) = schema();
        tables.push(table("notes", &["id"]));
        keys.push(fk("notes_user_fk", "notes", "user_id", "users", "id"));
        keys.push(fk(
            "notes_audit_fk",
            "notes",
            "audit_id",
            "audit_logs",
            "id",
        ));
        let err = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::NeverIncludeReferenced {
                edges: vec!["notes -> audit_logs (notes_audit_fk)".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_an_unknown_root() {
        let (tables, keys) = schema();
        let err = plan_of(
            &[root("nope", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::UnknownRoot {
                tables: vec!["nope".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_a_duplicate_root() {
        let (tables, keys) = schema();
        let err = plan_of(
            &[
                root("users", SampleAmount::Count(10)),
                root("users", SampleAmount::Count(20)),
            ],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::DuplicateRoot {
                table: "users".to_owned()
            }
        );
    }

    #[test]
    fn plan_refuses_a_root_that_is_also_never_included() {
        let (tables, keys) = schema();
        let err = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules {
                always_include: vec!["countries".to_owned()],
                never_include: vec!["users".to_owned(), "audit_logs".to_owned()],
            },
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::RootNeverIncluded {
                tables: vec!["users".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_a_root_that_is_also_copied_whole() {
        // A size and "every row" are two different answers, and letting the
        // root win would quietly make a lookup table a descend source.
        let (tables, keys) = schema();
        let err = plan_of(
            &[root("countries", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::RootAlwaysIncluded {
                tables: vec!["countries".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_a_table_declared_both_always_and_never() {
        let (tables, keys) = schema();
        let err = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules {
                always_include: vec!["countries".to_owned(), "audit_logs".to_owned()],
                never_include: vec!["audit_logs".to_owned()],
            },
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::RuleContradiction {
                tables: vec!["audit_logs".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_a_stale_sample_rule() {
        let (tables, keys) = schema();
        let err = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules {
                always_include: vec!["countries".to_owned(), "gone".to_owned()],
                never_include: vec!["audit_logs".to_owned()],
            },
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::StaleRule {
                tables: vec!["gone".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_a_framework_table_in_the_sample_rules() {
        let (tables, keys) = schema();
        let err = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 0,
            rules: &SampleRules {
                always_include: vec!["countries".to_owned()],
                never_include: vec!["audit_logs".to_owned(), "autumn_jobs".to_owned()],
            },
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::from(["autumn_jobs".to_owned()]),
            purged: &BTreeSet::new(),
            partitions: &BTreeSet::new(),
        })
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::FrameworkRule {
                tables: vec!["autumn_jobs".to_owned()]
            }
        );
    }

    #[test]
    fn plan_refuses_a_table_without_a_primary_key() {
        let (mut tables, mut keys) = schema();
        tables.push(table("logins", &[]));
        keys.push(fk("logins_user_fk", "logins", "user_id", "users", "id"));
        let err = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::NoRowKey {
                tables: vec!["logins".to_owned()]
            }
        );
    }

    #[test]
    fn plan_ignores_a_missing_primary_key_on_a_never_include_table() {
        // A table that is emptied outright needs no row identity.
        let (mut tables, keys) = schema();
        tables.push(table("raw_events", &[]));
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules {
                always_include: vec!["countries".to_owned()],
                never_include: vec!["audit_logs".to_owned(), "raw_events".to_owned()],
            },
            &tables,
            &keys,
        )
        .unwrap();
        assert_eq!(role_of(&plan, "raw_events"), SampleRole::NeverInclude);
    }

    #[test]
    fn plan_refuses_a_reference_cycle_between_tables() {
        let (mut tables, mut keys) = schema();
        tables.push(table("orgs", &["id"]));
        keys.push(fk("users_org_fk", "users", "org_id", "orgs", "id"));
        keys.push(fk("orgs_owner_fk", "orgs", "owner_id", "users", "id"));
        let err = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::ForeignKeyCycle {
                tables: vec!["orgs".to_owned(), "users".to_owned()]
            }
        );
    }

    #[test]
    fn a_cycle_is_broken_by_copying_one_of_its_tables_whole() {
        // `always_include` takes a table out of the removals entirely, so the
        // remaining deletes have an order again — which is exactly what the
        // cycle diagnostic tells the reader to do.
        let (mut tables, mut keys) = schema();
        tables.push(table("orgs", &["id"]));
        keys.push(fk("users_org_fk", "users", "org_id", "orgs", "id"));
        keys.push(fk("orgs_owner_fk", "orgs", "owner_id", "users", "id"));
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules {
                always_include: vec!["countries".to_owned(), "orgs".to_owned()],
                never_include: vec!["audit_logs".to_owned()],
            },
            &tables,
            &keys,
        )
        .unwrap();
        assert_eq!(role_of(&plan, "orgs"), SampleRole::AlwaysInclude);
        assert!(
            !joined(&plan.delete_statements()).contains("\"orgs\""),
            "a full-copy table keeps every row, so it is never deleted from"
        );
    }

    #[test]
    fn plan_allows_a_self_referencing_table() {
        // A row and the row it points at are removed by the same statement, so
        // the constraint is satisfied when the statement ends.
        let (tables, mut keys) = schema();
        keys.push(fk("users_manager_fk", "users", "manager_id", "users", "id"));
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        assert!(
            plan.walk_edges.iter().any(|e| e.name == "users_manager_fk"),
            "a self-reference is still walked, so managers are pulled in"
        );
    }

    #[test]
    fn plan_refuses_a_table_outside_the_sample_that_references_a_sampled_one() {
        let (tables, mut keys) = schema();
        keys.push(fk("jobs_user_fk", "autumn_jobs", "user_id", "users", "id"));
        let err = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 0,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::from(["autumn_jobs".to_owned()]),
            purged: &BTreeSet::new(),
            partitions: &BTreeSet::new(),
        })
        .unwrap_err();
        assert_eq!(
            err,
            SampleError::OutsideTableReferencesSampled {
                edges: vec!["autumn_jobs -> users (jobs_user_fk)".to_owned()]
            }
        );
    }

    #[test]
    fn plan_accepts_a_purged_framework_table_referencing_a_sampled_one() {
        // `[framework] purge` empties it before the sample deletes, so nothing
        // is left to dangle.
        let (tables, mut keys) = schema();
        keys.push(fk("jobs_user_fk", "autumn_jobs", "user_id", "users", "id"));
        let plan = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 0,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::from(["autumn_jobs".to_owned()]),
            purged: &BTreeSet::from(["autumn_jobs".to_owned()]),
            partitions: &BTreeSet::new(),
        })
        .unwrap();
        assert!(
            plan.walk_edges
                .iter()
                .all(|e| e.child_table != "autumn_jobs"),
            "a framework table is never part of the walk"
        );
    }

    #[test]
    fn without_a_root_the_app_tables_are_uncovered() {
        // Nothing descends out of a full-copy table, so an `always_include`
        // declaration alone reaches nothing. The CLI never gets here (no
        // `--sample` means no sampling at all); the planner still refuses
        // rather than emptying every table it was not told about.
        let (tables, keys) = schema();
        let err = plan_of(&[], &fixture_rules(), &tables, &keys).unwrap_err();
        assert_eq!(
            err,
            SampleError::UncoveredTables {
                tables: vec!["comments".to_owned(), "users".to_owned()]
            }
        );
    }

    #[test]
    fn a_purge_waits_for_the_sample_when_an_emptied_table_references_it() {
        // `audit_logs` (never_include, so the sample empties it) references the
        // purged `autumn_jobs`. Purging first would hit those rows, so the plan
        // defers that one purge until the sample has emptied its child.
        let (tables, mut keys) = schema();
        keys.push(fk(
            "audit_logs_job_fk",
            "audit_logs",
            "job_id",
            "autumn_jobs",
            "id",
        ));
        let plan = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 7,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::from(["autumn_jobs".to_owned()]),
            purged: &BTreeSet::from(["autumn_jobs".to_owned()]),
            partitions: &BTreeSet::new(),
        })
        .unwrap();
        assert_eq!(
            plan.purge_after,
            BTreeSet::from(["autumn_jobs".to_owned()]),
            "the purge its emptied child references must run after the sample"
        );
    }

    #[test]
    fn a_purge_a_retained_table_references_is_refused() {
        // Same edge, but from `comments`, whose rows the sample KEEPS. Purging
        // before the sample hits them and purging after still hits them, so no
        // order exists and the plan refuses instead of failing mid-transaction.
        let (tables, mut keys) = schema();
        keys.push(fk(
            "comments_job_fk",
            "comments",
            "job_id",
            "autumn_jobs",
            "id",
        ));
        let err = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 7,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::from(["autumn_jobs".to_owned()]),
            purged: &BTreeSet::from(["autumn_jobs".to_owned()]),
            partitions: &BTreeSet::new(),
        })
        .unwrap_err();
        let SampleError::RetainedReferencesPurged { edges } = err else {
            panic!("expected a retained-into-purged refusal, got {err:?}");
        };
        assert_eq!(edges.len(), 1);
        assert!(edges[0].contains("comments_job_fk"), "{edges:?}");
    }

    #[test]
    fn a_purge_nothing_sampled_references_still_runs_first() {
        // The unchanged majority: no sampled table points at the purged one, so
        // nothing is deferred and the single pre-sample pass runs every purge.
        let (tables, keys) = schema();
        let plan = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 7,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::from(["autumn_jobs".to_owned()]),
            purged: &BTreeSet::from(["autumn_jobs".to_owned()]),
            partitions: &BTreeSet::new(),
        })
        .unwrap();
        assert!(plan.purge_after.is_empty());
    }

    #[test]
    fn a_partition_local_foreign_key_is_refused_rather_than_dropped() {
        // A key declared on the partition itself, not cloned from the parent
        // (the caller filters clones out by catalog parentage, so one reaching
        // here is partition-local). The plan plays every partition's rows
        // through the partitioned parent, so it cannot honour a key that binds
        // one partition — dropping it silently would leave the edge out of both
        // the walk and the integrity re-check, which is the fail-open this
        // feature exists to avoid.
        let (tables, mut keys) = schema();
        keys.push(fk(
            "comments_2026_local_fk",
            "comments_2026",
            "user_id",
            "users",
            "id",
        ));
        let err = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 7,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::new(),
            purged: &BTreeSet::new(),
            partitions: &BTreeSet::from(["comments_2026".to_owned()]),
        })
        .unwrap_err();
        let SampleError::PartitionLocalForeignKey { edges } = err else {
            panic!("expected a partition-local refusal, got {err:?}");
        };
        assert_eq!(edges.len(), 1);
        assert!(edges[0].contains("comments_2026_local_fk"), "{edges:?}");
    }

    #[test]
    fn a_partition_with_no_local_foreign_key_plans_through_its_parent() {
        // The clone case: with the clones filtered out upstream, no edge names
        // the partition and the plan plays its rows through the parent.
        let (tables, keys) = schema();
        let plan = build_plan(&SampleInputs {
            roots: &[root("users", SampleAmount::Count(10))],
            seed: 7,
            rules: &fixture_rules(),
            tables: &tables,
            foreign_keys: &keys,
            framework_tables: &BTreeSet::new(),
            purged: &BTreeSet::new(),
            partitions: &BTreeSet::from(["comments_2026".to_owned()]),
        })
        .unwrap();
        assert!(
            plan.walk_edges
                .iter()
                .all(|e| e.child_table != "comments_2026"),
            "a partition's cloned constraint must not be walked"
        );
        assert!(
            plan.verify_edges
                .iter()
                .all(|e| e.child_table != "comments_2026"),
            "nor verified twice"
        );
    }

    // ── Delete order ────────────────────────────────────────────────────────

    #[test]
    fn deletes_run_children_before_parents() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let order: Vec<&str> = plan.tables.iter().map(|t| t.table.as_str()).collect();
        let at = |name: &str| order.iter().position(|t| *t == name).unwrap();
        assert!(
            at("comments") < at("users"),
            "a child must be emptied before its parent: {order:?}"
        );
        assert!(
            at("users") < at("countries"),
            "the lookup table's parents come last: {order:?}"
        );
    }

    // ── Generated SQL ───────────────────────────────────────────────────────

    #[test]
    fn root_seeding_is_deterministic_bounded_and_seeded() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Percent(1.0))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let sql = joined(&plan.seed_statements(&BTreeMap::from([
            ("users".to_owned(), 1000_i64),
            ("countries".to_owned(), 12),
        ])));
        // The order is a hash of the seed and the row key — not physical order,
        // not `random()`, so the same seed replays the same rows.
        assert!(sql.contains("md5('7' || '|'"), "seeded order key: {sql}");
        assert!(sql.contains("ORDER BY"), "{sql}");
        assert!(sql.contains("LIMIT 10"), "1% of 1000 rows: {sql}");
        // The lookup table is copied whole, with no ordering or limit.
        assert!(
            sql.contains("FROM \"public\".\"countries\""),
            "always-include tables are seeded in full: {sql}"
        );
    }

    #[test]
    fn a_different_seed_changes_the_order_key() {
        let (tables, keys) = schema();
        let counts = BTreeMap::from([("users".to_owned(), 100_i64), ("countries".to_owned(), 1)]);
        let with_seed = |seed: u64| {
            build_plan(&SampleInputs {
                roots: &[root("users", SampleAmount::Count(5))],
                seed,
                rules: &fixture_rules(),
                tables: &tables,
                foreign_keys: &keys,
                framework_tables: &BTreeSet::new(),
                purged: &BTreeSet::new(),
                partitions: &BTreeSet::new(),
            })
            .unwrap()
            .seed_statements(&counts)
        };
        assert_ne!(joined(&with_seed(1)), joined(&with_seed(2)));
    }

    #[test]
    fn a_never_include_table_is_never_seeded() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let sql = joined(&plan.seed_statements(&BTreeMap::new()));
        assert!(
            !sql.contains("audit_logs"),
            "an excluded table selects no rows at all: {sql}"
        );
        let deletes = joined(&plan.delete_statements());
        assert!(
            deletes.contains("DELETE FROM \"public\".\"audit_logs\""),
            "an excluded table is emptied: {deletes}"
        );
    }

    #[test]
    fn an_always_include_table_is_never_deleted_from() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let deletes = joined(&plan.delete_statements());
        assert!(
            !deletes.contains("\"countries\""),
            "a full-copy table keeps every row: {deletes}"
        );
    }

    #[test]
    fn statements_are_schema_qualified() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let all = format!(
            "{}\n{}\n{}",
            joined(&plan.seed_statements(&BTreeMap::from([("users".to_owned(), 10_i64)]))),
            joined(&plan.walk_statements()),
            joined(&plan.delete_statements()),
        );
        // A tenant `search_path` must not be able to redirect a DELETE to a
        // table nothing classified — the same rule the scrub's own writes keep.
        // Checked for EVERY table: one unqualified name is one redirected write.
        for (name, _) in &tables {
            let quoted = quote_ident(name);
            assert_eq!(
                all.matches(&quoted).count(),
                all.matches(&format!("\"public\".{quoted}")).count(),
                "every reference to {name} must be public-qualified: {all}"
            );
        }
    }

    #[test]
    fn the_walk_ascends_into_the_parent_and_descends_into_the_child() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let set_of = |table: &str| {
            let t = plan.tables.iter().find(|t| t.table == table).unwrap();
            (t.keep.clone(), t.descend.clone())
        };
        let (users_keep, users_descend) = set_of("users");
        let (comments_keep, comments_descend) = set_of("comments");
        let walk = plan.walk_statements();

        // Ascend fills the PARENT's keep set from the child's. Transposing the
        // two sets would still produce plausible-looking SQL, so the assertion
        // names both ends.
        assert!(
            walk.iter().any(|s| s.starts_with(&format!(
                "/* comments_user_fk */ INSERT INTO {}",
                quote_ident(&users_keep)
            )) && s.contains(&quote_ident(&comments_keep))),
            "the ascend must fill users' keep set from comments': {walk:?}"
        );
        // Descend fills the CHILD's keep AND descend sets, from the parent's
        // descend set — never from its keep set, or an ascended-only row would
        // pull its whole subtree in.
        for target in [&comments_keep, &comments_descend] {
            assert!(
                walk.iter().any(|s| s.starts_with(&format!(
                    "/* comments_user_fk */ INSERT INTO {}",
                    quote_ident(target)
                )) && s.contains(&quote_ident(&users_descend))),
                "the descend must fill {target} from users' descend set: {walk:?}"
            );
        }
        assert!(
            !walk.iter().any(|s| s.contains(&format!(
                "INSERT INTO {} (k) SELECT DISTINCT c.",
                quote_ident(&users_descend)
            ))),
            "ascent must never make a row descend-eligible: {walk:?}"
        );
    }

    #[test]
    fn the_walk_never_descends_out_of_an_always_include_table() {
        // Descending from a lookup table would pull in every row that
        // references it — every user in the database — and the sample would
        // stop being a sample.
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let countries = plan
            .tables
            .iter()
            .find(|t| t.table == "countries")
            .unwrap()
            .descend
            .clone();
        let sql = joined(&plan.walk_statements());
        assert!(
            !sql.contains(&countries),
            "no statement may read or write the lookup table's descend set: {sql}"
        );
    }

    #[test]
    fn a_composite_primary_key_becomes_an_unambiguous_row_key() {
        let tables = vec![
            table("users", &["id"]),
            table("tags", &["id"]),
            table("user_tags", &["user_id", "tag_id"]),
        ];
        let keys = vec![
            fk("user_tags_user_fk", "user_tags", "user_id", "users", "id"),
            fk("user_tags_tag_fk", "user_tags", "tag_id", "tags", "id"),
        ];
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules::default(),
            &tables,
            &keys,
        )
        .unwrap();
        let sql = joined(&plan.delete_statements());
        // `quote_nullable` escapes each component, so `('a,b', 'c')` and
        // `('a', 'b,c')` cannot collapse onto the same key.
        assert!(
            sql.contains("quote_nullable"),
            "a composite key must be rendered unambiguously: {sql}"
        );
    }

    #[test]
    fn a_composite_foreign_key_joins_on_every_component() {
        let tables = vec![
            table("orders", &["tenant_id", "id"]),
            table("order_lines", &["id"]),
        ];
        let keys = vec![ForeignKeyConstraint {
            name: "order_lines_order_fk".to_owned(),
            child_table: "order_lines".to_owned(),
            child_columns: vec!["tenant_id".to_owned(), "order_id".to_owned()],
            parent_table: "orders".to_owned(),
            parent_columns: vec!["tenant_id".to_owned(), "id".to_owned()],
            match_full: false,
        }];
        let plan = plan_of(
            &[root("orders", SampleAmount::Count(10))],
            &SampleRules::default(),
            &tables,
            &keys,
        )
        .unwrap();
        let sql = joined(&plan.walk_statements());
        // The pairing is what matters, not the mere presence of both names:
        // `tenant_id` also appears in the parent's own row key.
        assert!(
            sql.contains("p.\"tenant_id\" = c.\"tenant_id\" AND p.\"id\" = c.\"order_id\""),
            "the join must pair every component in key order: {sql}"
        );
    }

    #[test]
    fn integrity_checks_cover_every_foreign_key() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let checks = plan.integrity_statements();
        assert_eq!(checks.len(), 2, "one check per foreign key");
        for (_, sql) in plan.integrity_statements() {
            assert!(sql.contains("count(*)"), "{sql}");
            assert!(
                sql.contains("IS NULL"),
                "an orphan is a missing parent: {sql}"
            );
        }
    }

    #[test]
    fn a_match_full_composite_key_also_counts_partially_null_tuples() {
        // MATCH FULL admits only all-NULL or all-populated tuples. A
        // MATCH SIMPLE predicate checks the populated ones alone, so a
        // half-filled tuple — a violation Postgres will not re-check on a
        // constraint left NOT VALID — would pass the recount silently.
        let (tables, mut keys) = schema();
        // Widen the existing comments -> users edge into a composite MATCH FULL
        // one, so the plan is the ordinary fixture and only the key shape moves.
        keys[0].child_columns = vec!["user_id".to_owned(), "tenant_id".to_owned()];
        keys[0].parent_columns = vec!["id".to_owned(), "tenant_id".to_owned()];
        keys[0].match_full = true;
        keys[0].name = "comments_user_full_fk".to_owned();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let (_, sql) = plan
            .integrity_statements()
            .into_iter()
            .find(|(label, _)| label.starts_with("comments_user_full_fk"))
            .expect("the composite key must be checked");
        assert!(
            sql.contains(r#""user_id" IS NULL"#) && sql.contains(r#""tenant_id" IS NULL"#),
            "a partially-NULL tuple must be counted too: {sql}"
        );
    }

    #[test]
    fn a_match_simple_composite_key_ignores_partially_null_tuples() {
        // The default, unchanged: a NULL component satisfies the reference, so
        // only fully-populated tuples are checked for a missing parent.
        let (tables, mut keys) = schema();
        // The same composite edge, left at the default MATCH SIMPLE.
        keys[0].child_columns = vec!["user_id".to_owned(), "tenant_id".to_owned()];
        keys[0].parent_columns = vec!["id".to_owned(), "tenant_id".to_owned()];
        keys[0].name = "comments_user_simple_fk".to_owned();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let (_, sql) = plan
            .integrity_statements()
            .into_iter()
            .find(|(label, _)| label.starts_with("comments_user_simple_fk"))
            .expect("the composite key must be checked");
        assert!(
            !sql.contains("IS NULL) AND ("),
            "MATCH SIMPLE must not gain the mixed-NULL arm: {sql}"
        );
    }

    #[test]
    fn an_excluded_table_without_a_primary_key_builds_no_set() {
        // It is emptied by a bare DELETE, so it needs no row identity — and
        // building it one would render an empty key expression, which is not
        // SQL at all. The `never_include` remedy `NoRowKey` prescribes has to
        // actually work.
        let (mut tables, keys) = schema();
        tables.push(table("request_logs", &[]));
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &SampleRules {
                always_include: vec!["countries".to_owned()],
                never_include: vec!["audit_logs".to_owned(), "request_logs".to_owned()],
            },
            &tables,
            &keys,
        )
        .unwrap();
        let setup = joined(&plan.setup_statements());
        assert!(
            !setup.contains("request_logs") && !setup.contains("audit_logs"),
            "an excluded table gets no keep set: {setup}"
        );
        assert!(
            !setup.contains("SELECT  AS k") && !setup.contains("SELECT () AS k"),
            "no statement may carry an empty row key: {setup}"
        );
        assert!(
            joined(&plan.delete_statements()).contains("DELETE FROM \"public\".\"request_logs\""),
            "it is still emptied"
        );
    }

    #[test]
    fn a_constraint_name_cannot_escape_its_sql_comment() {
        // `conname` is catalog text. A name carrying `*/` would close the
        // comment and let whatever follows run as part of the statement.
        let (tables, mut keys) = schema();
        keys.push(fk(
            "*/WITH z AS(INSERT INTO loot SELECT 1 RETURNING 1)/*",
            "comments",
            "user_id",
            "users",
            "id",
        ));
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        for statement in plan.walk_statements() {
            let Some((tag, _)) = statement
                .strip_prefix("/* ")
                .and_then(|rest| rest.split_once(" */ "))
            else {
                panic!("every walk statement opens with a tag: {statement}");
            };
            assert!(
                !tag.contains('*') && !tag.contains('/'),
                "the tag can only be closed by the one that follows it: {tag:?}"
            );
        }
        // Only `*/` closes a block comment, so a surviving `--` is inert; the
        // characters that could close it are the ones that must go.
        assert_eq!(
            comment_safe("*/DROP TABLE users;--"),
            "__DROP TABLE users_--"
        );
    }

    #[test]
    fn sets_are_indexed_and_analysed_after_the_roots_are_seeded() {
        // An index built on an empty table is built for nothing, and a
        // temporary table autovacuum never sees plans at its 10-page default
        // until something analyses it.
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let index = joined(&plan.index_statements());
        assert!(
            index.contains("CREATE INDEX ON") && index.contains("ANALYZE "),
            "{index}"
        );
        assert!(
            !joined(&plan.setup_statements()).contains("CREATE INDEX"),
            "the index must not be built before the rows arrive"
        );
        assert!(
            plan.analyze_statements()
                .iter()
                .all(|s| s.starts_with("ANALYZE ")),
            "each pass re-analyses the sets it just grew"
        );
    }

    #[test]
    fn setup_creates_a_keep_and_descend_set_per_table() {
        let (tables, keys) = schema();
        let plan = plan_of(
            &[root("users", SampleAmount::Count(10))],
            &fixture_rules(),
            &tables,
            &keys,
        )
        .unwrap();
        let sql = joined(&plan.setup_statements());
        let keyed = plan
            .tables
            .iter()
            .filter(|t| t.role != SampleRole::NeverInclude)
            .count();
        assert_eq!(
            sql.matches("CREATE TEMPORARY TABLE").count(),
            keyed * 2,
            "{sql}"
        );
        assert!(sql.contains("ON COMMIT DROP"), "{sql}");
    }
}
