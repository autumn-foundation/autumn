//! `autumn migrate new` and `autumn migrate check-collisions`.
//!
//! Diesel records applied migrations **by version** (the leading
//! `YYYYMMDDHHMMSS` directory prefix). When two differently-named migration
//! directories share one version, a fresh database applies exactly one of
//! them and records the version as done -- the other is skipped forever,
//! with no error anywhere. A skipped migration is not merely absent: a
//! constraint it was supposed to add will appear to exist in review and not
//! exist in the database.
//!
//! `autumn migrate new` (the preventive half) hands out a version that is
//! free everywhere this checkout can see: the working tree, every local and
//! remote-tracking git branch, and this CLI's own compiled-in framework
//! migrations. `autumn migrate check-collisions` (the enforcing half) is the
//! CI-time backstop for a collision this checkout could not see when the
//! migration was generated -- a branch pushed after, or a teammate's
//! concurrent PR.
//!
//! Neither command can see a *plugin's* migrations: those live inside a
//! compiled Rust crate, not a directory this checkout's git history
//! contains, and an app cannot be expected to coordinate versions with every
//! plugin author who might ship alongside it. That gap is closed at the
//! framework level instead -- every registered `EmbeddedMigrations` set
//! (framework, plugin, and app) is checked for cross-set version collisions
//! at app startup by
//! [`autumn_web::migrate::check_migration_version_collisions`], which fails
//! loudly before any migration is applied, regardless of whether the
//! colliding authors ever coordinated.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use chrono::{Duration, NaiveDateTime, Utc};

use super::DEFAULT_MIGRATIONS_DIR;

/// Diesel's migration-version format: a 14-digit `YYYYMMDDHHMMSS` stamp.
const VERSION_FMT: &str = "%Y%m%d%H%M%S";
const VERSION_LEN: usize = 14;

/// The version prefix of a migration directory name (everything before the
/// first `_`), validated as Diesel's 14-digit shape. `None` for anything
/// else -- a stray non-migration entry under `migrations/`, or a directory
/// name this parser cannot make sense of.
fn version_prefix(dir_name: &str) -> Option<String> {
    let prefix = dir_name.split('_').next()?;
    (prefix.len() == VERSION_LEN && prefix.bytes().all(|b| b.is_ascii_digit()))
        .then(|| prefix.to_string())
}

fn parse_version(version: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(version, VERSION_FMT).ok()
}

fn format_version(dt: NaiveDateTime) -> String {
    dt.format(VERSION_FMT).to_string()
}

/// Migration directory names (not just versions) present in the working
/// tree at `dir`, sorted for stable output.
fn local_migration_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| version_prefix(name).is_some())
        .collect();
    names.sort();
    names
}

/// `git for-each-ref --format=%(refname) <patterns>`. An empty/missing repo
/// (or no `git` on PATH) degrades to an empty list rather than an error --
/// both commands then fall back to "working tree only".
fn git_refs(patterns: &[&str]) -> Vec<String> {
    let Ok(output) = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)"])
        .args(patterns)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .filter(|r| !r.is_empty())
        .collect()
}

/// Every branch on `origin`, mapped to its current commit OID, from `git
/// ls-remote --heads origin` -- a lightweight ref advertisement that
/// transfers no objects, so it stays cheap even on a large repository.
/// `None` when this cannot be determined (no network, no `origin` remote,
/// no `git` on PATH); the caller then treats coverage as unknown,
/// degrading the same as a shallow/partial clone.
///
/// This checks the actual remote state, not local configuration: `remote.
/// origin.fetch` reflects what a bare `git fetch` WOULD pull, but a CI
/// checkout commonly runs an explicit, narrower fetch (a specific ref, a
/// shallow depth) that never consults that refspec at all -- so a config
/// check can read "fetches every branch" while only one branch was ever
/// actually retrieved.
///
/// Keyed by branch name with the OID as the value, not just a name set or a
/// count: a long-lived local clone can retain a stale `refs/remotes/origin/
/// <deleted-branch>` (nothing prunes it without an explicit `git fetch
/// --prune`) while missing a newly-created branch, so a bare count can
/// coincidentally still match. A name-only set closes that gap but not a
/// narrower one: a branch this checkout already knows by name can still
/// have moved on the remote since the last fetch, and the local ref -- and
/// therefore whatever `migration_dirs_at_ref` reads -- is the OLD tip until
/// fetched again. Comparing OIDs, not just names, is what catches a branch
/// that has been pushed to (not just created) since this checkout last saw
/// it.
fn remote_branch_heads() -> Option<BTreeMap<String, String>> {
    let output = Command::new("git")
        .args(["ls-remote", "--heads", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (sha, refname) = line.split_once('\t')?;
                let name = refname.strip_prefix("refs/heads/")?;
                Some((name.to_owned(), sha.to_owned()))
            })
            .collect(),
    )
}

/// Every `refs/remotes/origin/*` branch (excluding the `HEAD` symref)
/// currently in this checkout, mapped to the commit OID it points at --
/// the local counterpart to [`remote_branch_heads`], compared against it to
/// decide whether every branch is not just present but actually current.
fn local_origin_branch_heads() -> BTreeMap<String, String> {
    let Ok(output) = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/remotes/origin",
        ])
        .output()
    else {
        return BTreeMap::new();
    };
    if !output.status.success() {
        return BTreeMap::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (sha, refname) = line.split_once(' ')?;
            let name = refname.strip_prefix("refs/remotes/origin/")?;
            (name != "HEAD").then(|| (name.to_owned(), sha.to_owned()))
        })
        .collect()
}

/// The current directory's path relative to the repository root, as git's
/// `<ref>:path` syntax needs it: that syntax is always root-relative, never
/// CWD-relative, so a migrations directory that is not itself the repo root
/// (an app nested in a monorepo, e.g. this workspace's own `autumn/`) would
/// otherwise resolve against the wrong path and `git ls-tree` would fail
/// silently -- exactly the shallow-clone degrade this checker uses to mean
/// "no directory at that ref", except every ref would wrongly look empty.
///
/// Empty when this is already the repo root, or git cannot answer (in which
/// case every ref lookup below fails the same way and degrades safely to
/// "working tree only").
fn repo_root_prefix() -> String {
    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--show-prefix"])
        .output()
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Migration directory names present under `migrations_dir_name` (a path
/// relative to the current directory) at `git_ref` (a commit-ish, typically
/// a branch ref). Empty (not an error) when the ref has no such directory --
/// most branches predate a given migration, and a fresh branch may not have
/// `migrations/` at all.
fn migration_dirs_at_ref(git_ref: &str, migrations_dir_name: &str) -> Vec<String> {
    let root_relative_dir = format!("{}{migrations_dir_name}", repo_root_prefix());
    let Ok(output) = Command::new("git")
        .args([
            "ls-tree",
            "-d",
            // Without this, `git ls-tree` additionally filters its *output* by
            // the current directory's own repo-relative path (even though
            // `<ref>:<path>` itself already resolves root-relative) -- so
            // from inside a nested crate, every entry gets silently filtered
            // away and this looks exactly like "ref has no such directory".
            "--full-tree",
            "--name-only",
            &format!("{git_ref}:{root_relative_dir}"),
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .filter(|d| !d.is_empty())
        .collect()
}

/// This CLI's own compiled-in framework migration directory names
/// (`autumn_web::migrate::FRAMEWORK_MIGRATIONS`) -- so a freshly generated
/// app migration cannot collide with the framework release this app depends
/// on, without the app needing its own copy of the framework's history.
fn framework_migration_names() -> Vec<String> {
    use autumn_web::reexports::diesel::migration::{Migration, MigrationSource};
    use autumn_web::reexports::diesel::pg::Pg;

    let migrations: Vec<Box<dyn Migration<Pg>>> = autumn_web::migrate::FRAMEWORK_MIGRATIONS
        .migrations()
        .unwrap_or_default();
    migrations.iter().map(|m| m.name().to_string()).collect()
}

/// Pick the first migration version at or after `now` that is not in
/// `taken`, and never before the latest version already in `taken`.
///
/// UTC, because the version is a total order shared by everyone: a
/// local-time stamp from a contributor west of UTC can sort before a
/// migration written earlier by someone east of them, and Diesel would then
/// run them in an order neither author intended.
///
/// Uniqueness alone is not enough, because the version is also the run
/// order: a machine whose clock is behind can otherwise leave `now` earlier
/// than a migration already in the tree, and Diesel would run this new one
/// FIRST on every fresh database.
///
/// # Errors
/// Returns an error (rather than guessing) when the latest taken version is
/// more than a week ahead of `now` -- that far ahead means a mistyped year
/// or a badly wrong clock, not skew worth silently absorbing -- when the
/// latest taken version isn't a valid calendar timestamp at all (Diesel only
/// requires 14 digits, not a real date, so a hand-typed version like
/// `20261301000000` -- month 13 -- satisfies `version_prefix` but can't be
/// arithmetic'd on) -- or when no free second exists in the two minutes
/// after the starting point.
fn resolve_free_version(taken: &BTreeSet<String>, now: NaiveDateTime) -> Result<String, String> {
    // Lexical string comparison, not `parse_version`+`max`: all versions are
    // fixed-width digit strings, so string order and chronological order are
    // the same order REGARDLESS of whether the string is a valid calendar
    // date. Deciding "is anything already ahead of `now`" this way means a
    // garbage-but-14-digit version (`20261301000000`) still counts instead of
    // silently vanishing from consideration -- only the arithmetic below
    // (bumping past it, checking the horizon) needs it to actually parse, and
    // that failure gets a loud error rather than a quietly wrong ordering.
    let max_taken_str = taken.iter().max().cloned();

    let mut version = now;
    if let Some(max_taken_str) = &max_taken_str
        && max_taken_str.as_str() > format_version(now).as_str()
    {
        let Some(max_taken) = parse_version(max_taken_str) else {
            return Err(format!(
                "the latest existing migration version is {max_taken_str}, which is not a \
                 valid calendar timestamp -- Diesel only requires 14 digits, not a real date, \
                 but this generator needs to do date arithmetic on the latest one to guarantee \
                 the next version sorts after it.\n\
                 Fix migrations/{max_taken_str}_* to a real UTC timestamp (or rename it if it \
                 was a typo) before generating another migration.",
            ));
        };
        let horizon = version + Duration::days(7);
        if max_taken > horizon {
            return Err(format!(
                "the latest existing migration version is {}, which is more than a week ahead \
                 of this machine's UTC clock ({}).\n\
                 Either the clock is wrong, or a migration carries a mistyped version.\n\
                 Refusing to guess: generating a version before it would make Diesel run this \
                 migration first, and generating one after it would push every future \
                 migration behind a bogus date.\n\
                 Look for migrations/{}_* here or on another branch.",
                format_version(max_taken),
                format_version(now),
                format_version(max_taken),
            ));
        }
        version = max_taken + Duration::seconds(1);
        eprintln!(
            "note: this machine's UTC clock ({}) is behind the latest existing migration ({}); \
             using {} so this one still sorts -- and therefore runs -- after it",
            format_version(now),
            format_version(max_taken),
            format_version(version),
        );
    }

    let mut bumped = 0u32;
    while taken.contains(&format_version(version)) {
        version += Duration::seconds(1);
        bumped += 1;
        if bumped > 120 {
            return Err(format!(
                "no free version in the two minutes after {}; that is not a clock problem -- \
                 check migrations/ for a block of hand-written consecutive stamps",
                format_version(version),
            ));
        }
    }
    if bumped > 0 {
        eprintln!("note: the starting second was already claimed; moved forward {bumped}s");
    }
    Ok(format_version(version))
}

/// `<snake_case>` starting with a letter: lowercase letters, digits, and
/// single underscores. Kept to this shape so `version_prefix` (everything
/// before the first `_`) stays unambiguous -- a name starting with a digit
/// would be indistinguishable from part of the version.
fn validate_migration_name(name: &str) -> Result<(), String> {
    let starts_right = name.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    let chars_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let no_double_underscore = !name.contains("__");
    let no_edge_underscore = !name.starts_with('_') && !name.ends_with('_');
    if !name.is_empty() && starts_right && chars_ok && no_double_underscore && no_edge_underscore {
        Ok(())
    } else {
        Err(format!(
            "'{name}' is not a snake_case name starting with a letter (lowercase letters, \
             digits and single underscores)"
        ))
    }
}

/// `autumn migrate new <name>` -- create `migrations/<version>_<name>/` with
/// a version that will not collide with any migration this checkout can
/// see: the working tree, every local and remote-tracking git branch, and
/// this CLI's own compiled-in framework migrations.
pub fn run_new(name: &str) {
    if let Err(e) = validate_migration_name(name) {
        eprintln!("error: {e}");
        std::process::exit(2);
    }

    let migrations_dir = Path::new(DEFAULT_MIGRATIONS_DIR);
    if !migrations_dir.exists() {
        eprintln!(
            "\u{2717} Migrations directory not found: {DEFAULT_MIGRATIONS_DIR}/\n  Create it \
             with `diesel setup`."
        );
        std::process::exit(1);
    }

    let mut taken: BTreeSet<String> = local_migration_entries(migrations_dir)
        .iter()
        .filter_map(|d| version_prefix(d))
        .collect();
    for git_ref in git_refs(&["refs/heads", "refs/remotes"]) {
        for dir_name in migration_dirs_at_ref(&git_ref, DEFAULT_MIGRATIONS_DIR) {
            if let Some(v) = version_prefix(&dir_name) {
                taken.insert(v);
            }
        }
    }
    for name in framework_migration_names() {
        if let Some(v) = version_prefix(&name) {
            taken.insert(v);
        }
    }

    let version = match resolve_free_version(&taken, Utc::now().naive_utc()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let dir_name = format!("{version}_{name}");
    let dir = migrations_dir.join(&dir_name);
    if dir.exists() {
        eprintln!("error: {} already exists", dir.display());
        std::process::exit(1);
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("error: cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }

    let up_sql = format!(
        "-- {name}\n\
         --\n\
         -- WHY: state the problem this migration solves, not what the SQL does. The DDL\n\
         -- below already says what it does; what a reader six months from now cannot\n\
         -- recover is why it was worth a schema change.\n\
         --\n\
         -- LOCKING: if this touches a large or hot table, say which lock each statement\n\
         -- takes and for how long. `ALTER TABLE ... ADD CONSTRAINT ... CHECK` and `ADD\n\
         -- COLUMN ... NOT NULL` without a default both scan the whole table under ACCESS\n\
         -- EXCLUSIVE; on a chokepoint table that is an outage. Splitting into `NOT VALID`\n\
         -- plus a separate `VALIDATE CONSTRAINT` migration avoids it.\n"
    );
    let down_sql = format!(
        "-- Reverse of {dir_name}.\n\
         --\n\
         -- If the reverse cannot preserve data, say so HERE and make the loss an explicit\n\
         -- operator act rather than a silent DELETE.\n"
    );

    if let Err(e) = std::fs::write(dir.join("up.sql"), up_sql) {
        eprintln!("error: cannot write {}: {e}", dir.join("up.sql").display());
        std::process::exit(1);
    }
    if let Err(e) = std::fs::write(dir.join("down.sql"), down_sql) {
        eprintln!(
            "error: cannot write {}: {e}",
            dir.join("down.sql").display()
        );
        std::process::exit(1);
    }

    println!("{}", dir.display());
}

/// The remote-tracking ref for the repository's default branch.
///
/// Asks the remote directly first, via `git ls-remote --symref origin
/// HEAD` -- the same lightweight, no-objects-transferred query
/// `remote_branch_names` already uses, so this adds no new network
/// round-trip when both are called. That answer is authoritative and works
/// for any default branch name; falls back to the local `refs/remotes/
/// origin/HEAD` symref only when the network query fails (offline, no
/// `origin` remote), and to a fixed name list as a last resort.
///
/// The local symref is checked LAST, not first, because it can go silently
/// stale: `git fetch` updates branch refs but never touches this symref, so
/// a checkout that predates a default-branch rename (`master` -> `main`,
/// or any org-wide rename) keeps resolving the OLD name forever unless
/// something explicitly runs `git remote set-head origin -a`. Preferring
/// it would misclassify every migration already on the real (new) default
/// branch as "introduced by this checkout," and a pre-existing collision
/// on the real default branch would then fail an unrelated PR's gate.
/// Guessing from a fixed name list (`main`, `trunk`, ...) has the same
/// failure mode for any default branch name not on that list:
/// `default_branch_entries` would stay empty and EVERY existing
/// working-tree migration would be misclassified the same way.
///
/// `None` when nothing resolves (e.g. no `origin` remote, no network AND no
/// local symref, no `git` on PATH) -- the caller then treats every
/// working-tree migration as introduced by this checkout, which is the safe
/// direction only in the sense that it never hides a real collision (it can
/// still misattribute one to the wrong checkout, but that is what
/// `remote_branch_names` returning `None` already puts the caller on notice
/// about via the coverage warning).
fn resolve_default_branch_ref() -> Option<String> {
    if let Some(branch) = remote_head_branch() {
        return Some(format!("refs/remotes/origin/{branch}"));
    }
    if let Ok(output) = Command::new("git")
        .args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
        .output()
        && output.status.success()
    {
        let r = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !r.is_empty() {
            return Some(r);
        }
    }
    ["main", "master", "trunk", "trunk-dev"]
        .into_iter()
        .map(|name| format!("refs/remotes/origin/{name}"))
        .find(|r| {
            Command::new("git")
                .args(["show-ref", "--verify", "--quiet", r])
                .status()
                .is_ok_and(|s| s.success())
        })
}

/// `origin`'s actual default branch name, resolved directly from the
/// remote via `git ls-remote --symref origin HEAD` -- the same lightweight,
/// no-objects-transferred query `remote_branch_names` already uses. Its
/// output includes a `ref: refs/heads/<name>\tHEAD` line naming the
/// branch `HEAD` points at; `None` when that line is absent (a remote with
/// a detached/unset HEAD, or the query itself failing).
fn remote_head_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["ls-remote", "--symref", "origin", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("ref: ")?;
            let refname = rest.split('\t').next()?;
            refname.strip_prefix("refs/heads/").map(str::to_owned)
        })
}

/// `autumn migrate check-collisions` -- the CI-time backstop: fail when a
/// migration version this checkout introduces is already claimed by a
/// different directory on the default branch, on another pushed branch, or
/// in this CLI's own compiled-in framework migrations.
///
/// Reports a collision only when one of the colliding directories is one
/// THIS checkout's working tree introduces (present locally, absent from
/// the resolved default branch) -- a collision between two OTHER branches
/// is real but is that branch's own gate to fail on, not this one's. A gate
/// that failed on every collision it could see, including ones neither
/// branch involved here caused, would get switched off within a week.
///
/// Reads remote-tracking refs only (`refs/remotes/origin/*`); local
/// branches do not exist on a CI runner, so counting them here would make a
/// developer's run and CI's run disagree about a version nobody else can
/// see. (Local branches are covered on the preventive side, in
/// [`run_new`].)
pub fn run_check_collisions() {
    let migrations_dir = Path::new(DEFAULT_MIGRATIONS_DIR);
    if !migrations_dir.exists() {
        println!("OK: no {DEFAULT_MIGRATIONS_DIR}/ directory -- nothing to check");
        return;
    }

    let mut claimants: BTreeMap<String, Vec<String>> = BTreeMap::new();

    let working_tree = local_migration_entries(migrations_dir);
    for dir_name in &working_tree {
        claimants
            .entry(dir_name.clone())
            .or_default()
            .push("working tree".to_owned());
    }

    // Resolved once and reused below for the coverage check too -- both uses
    // want the same "what does `origin` currently advertise" answer, and
    // `git ls-remote` is a network round trip worth paying only once.
    let remote_heads = remote_branch_heads();
    let local_heads = local_origin_branch_heads();

    // Whether `refs/remotes/origin/<branch_name>` is safe to scan for
    // migration claims: not just present, but AT THE COMMIT `origin`
    // currently advertises. A long-lived clone can retain a ref for a
    // branch `origin` no longer has at all (nothing prunes it without an
    // explicit `git fetch --prune`), or one that still exists but has moved
    // since this checkout's last fetch -- scanning either reads content
    // that either no longer exists upstream or isn't what would actually be
    // pushed, so either can claim a version against a phantom, blocking a
    // migration that reuses a version this checkout only THINKS is taken.
    // `false` (nothing scanned) when advertisement state itself is unknown
    // (no network, no `origin` remote): the coverage warning below already
    // tells the caller the cross-branch half did not run, so actually
    // scanning unverifiable local refs in that state would both contradict
    // the warning and risk a false FAIL against content nobody can confirm.
    let is_ref_current = |branch_name: &str| -> bool {
        remote_heads.as_ref().is_some_and(|remote_heads| {
            remote_heads
                .get(branch_name)
                .is_some_and(|remote_sha| local_heads.get(branch_name) == Some(remote_sha))
        })
    };

    let default_ref = resolve_default_branch_ref();
    let mut default_branch_entries: BTreeSet<String> = BTreeSet::new();
    // Whether `default_branch_entries` above is trustworthy enough to use as
    // a baseline for "did THIS checkout introduce this migration" -- `false`
    // when the default ref couldn't even be resolved, OR when it resolved
    // but its local tip doesn't match what origin currently advertises. In
    // that case `default_branch_entries` stays EMPTY, which is NOT evidence
    // that nothing exists there -- just that this checkout never verified
    // it. Treating it as evidence anyway would misattribute a pre-existing
    // collision between the (unverified) default branch and some OTHER
    // (verified) branch to this checkout, since a working-tree migration
    // inherited unchanged from trunk would look "not yet on the default
    // branch" purely because we never confirmed otherwise.
    let default_verified = if let Some(default_ref) = &default_ref {
        let is_current = default_ref
            .strip_prefix("refs/remotes/origin/")
            .is_some_and(|name| is_ref_current(name));
        if is_current {
            for dir_name in migration_dirs_at_ref(default_ref, DEFAULT_MIGRATIONS_DIR) {
                default_branch_entries.insert(dir_name.clone());
                claimants
                    .entry(dir_name)
                    .or_default()
                    .push(default_ref.clone());
            }
        }
        is_current
    } else {
        false
    };

    // Scoped to `origin` only, not the whole `refs/remotes` namespace: a
    // differently configured remote (e.g. `upstream`) is not what this
    // checkout would push a fix to, and its refs are outside
    // `is_ref_current`'s verification entirely -- scanning them would
    // silently bypass the OID check above for every ref they contain.
    let remote_refs = git_refs(&["refs/remotes/origin"]);
    let refs_seen = remote_refs.len();
    for git_ref in &remote_refs {
        if Some(git_ref) == default_ref.as_ref() {
            continue; // already recorded above
        }
        if let Some(branch_name) = git_ref.strip_prefix("refs/remotes/origin/")
            && !is_ref_current(branch_name)
        {
            continue;
        }
        for dir_name in migration_dirs_at_ref(git_ref, DEFAULT_MIGRATIONS_DIR) {
            claimants.entry(dir_name).or_default().push(git_ref.clone());
        }
    }

    for name in framework_migration_names() {
        claimants
            .entry(name)
            .or_default()
            .push("autumn framework migrations".to_owned());
    }

    // `refs_seen > 0` alone does not mean every branch was fetched -- see
    // `remote_branch_heads`. Every failure mode (nothing fetched at all, a
    // narrowed subset, or a known branch that has since moved) gets the
    // same loud warning and the same working-tree-only degrade, since all
    // three mean a real collision could go undetected.
    //
    // Compares (name, OID) PAIRS, not just names or counts. A name-only
    // check still misses two cases: a stale local `refs/remotes/origin/*`
    // for a since-deleted branch keeps the local ref COUNT matching the
    // remote's even while a genuinely new branch is missing (why this isn't
    // a bare count), and a branch this checkout already knows by name can
    // still have been pushed to since the last fetch -- the local ref (and
    // therefore whatever `migration_dirs_at_ref` reads) is the OLD tip
    // until fetched again, so the branch being "present" proves nothing
    // about it being CURRENT. Extra local refs (stale, deleted branches)
    // are harmless here: this checker only reads them, so a leftover ref
    // for a branch that no longer exists costs nothing beyond checking a
    // branch that no longer matters.
    let full_branch_coverage = remote_heads.as_ref().is_some_and(|remote_heads| {
        !remote_heads.is_empty()
            && remote_heads
                .iter()
                .all(|(name, sha)| local_heads.get(name) == Some(sha))
    });
    if !full_branch_coverage {
        eprintln!(
            "\nWARNING: this clone does not have every branch's refs available, so ONLY the\n\
             \x20        working tree and this CLI's compiled-in framework migrations were\n\
             \x20        checked -- the cross-branch half of this check did not run (or only\n\
             \x20        ran against a narrowed subset of branches).\n\n\
             \x20        In CI this means the checkout is shallow or single-branch: set\n\
             \x20        `fetch-depth: 0` on actions/checkout (and drop any `ref:`/sparse\n\
             \x20        narrowing), or fetch every branch explicitly:\n\n\
             \x20            git fetch origin '+refs/heads/*:refs/remotes/origin/*'\n\n\
             \x20        Locally, `git fetch origin` is enough.\n"
        );
    }

    let mut by_version: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for dir_name in claimants.keys() {
        if let Some(v) = version_prefix(dir_name) {
            by_version.entry(v).or_default().insert(dir_name.clone());
        }
    }

    let working_tree_only: BTreeSet<&String> = working_tree
        .iter()
        .filter(|d| !default_branch_entries.contains(*d))
        .collect();

    let mut mine: Vec<(String, BTreeSet<String>)> = Vec::new();
    let mut others = 0usize;
    for (version, names) in &by_version {
        if names.len() < 2 {
            continue;
        }
        // Attributing a collision to THIS checkout needs either a verified
        // default-branch baseline (so "not in default_branch_entries"
        // reliably means "not already on trunk"), or every claimant coming
        // from the working tree alone (a same-checkout duplicate is real
        // regardless of branch state -- it needs no branch data at all).
        // Without either, a name that's only "in working_tree_only" because
        // the default branch couldn't be verified might already be shipped,
        // unchanged, on that branch -- misattributing a pre-existing
        // collision between two OTHER branches to this checkout instead of
        // counting it under `others`.
        let purely_local = names.iter().all(|n| working_tree.contains(n));
        let attributable = default_verified || purely_local;
        if attributable && names.iter().any(|n| working_tree_only.contains(n)) {
            mine.push((version.clone(), names.clone()));
        } else {
            others += 1;
        }
    }

    if !mine.is_empty() {
        eprintln!("\nFAIL: migration version claimed by more than one directory:");
        for (version, names) in &mine {
            eprintln!("\n  version {version}:");
            for name in names {
                let sources = claimants
                    .get(name)
                    .map(|s| s.join(", "))
                    .unwrap_or_default();
                eprintln!("    migrations/{name}   ({sources})");
            }
        }
        eprintln!(
            "\nDiesel records applied migrations BY VERSION, so on a fresh database only ONE\n\
             directory per version ever runs -- the other is skipped forever, silently. A\n\
             skipped migration is not merely absent: a constraint it was supposed to add will\n\
             appear to exist in review and not exist in the database.\n\n\
             Fix: renumber YOUR migration (the one not yet on the default branch) to a free\n\
             version.\n\n\
             \x20   autumn migrate new <name>       # picks a free version\n\
             \x20   git mv migrations/<old> migrations/<new>_<same_name>\n\n\
             Keep the descriptive suffix, and if your migration is one half of an ordered\n\
             pair (ADD ... NOT VALID / VALIDATE), keep the pair in order."
        );
        std::process::exit(1);
    }

    if others > 0 {
        eprintln!(
            "note: {others} version collision(s) exist between OTHER branches and do not \
             involve this one -- they will fail on their own pushes."
        );
    }

    if full_branch_coverage {
        println!(
            "OK: no migration this checkout introduces collides with the default branch or \
             with any of {refs_seen} remote ref(s)"
        );
    } else {
        println!(
            "OK: every migration version in the working tree is unique (cross-branch check \
             SKIPPED -- see the warning above)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        parse_version(s).unwrap()
    }

    #[test]
    fn version_prefix_accepts_14_digit_prefix() {
        assert_eq!(
            version_prefix("20260812000000_add_widgets"),
            Some("20260812000000".to_owned())
        );
    }

    #[test]
    fn version_prefix_rejects_non_migration_entries() {
        assert_eq!(version_prefix("README.md"), None);
        assert_eq!(version_prefix("2026081200000_short"), None);
        assert_eq!(version_prefix("not_a_timestamp_at_all"), None);
    }

    #[test]
    fn validate_migration_name_accepts_snake_case() {
        assert!(validate_migration_name("add_widget_archived_at").is_ok());
    }

    #[test]
    fn validate_migration_name_rejects_leading_digit() {
        assert!(validate_migration_name("1add_widget").is_err());
    }

    #[test]
    fn validate_migration_name_rejects_pascal_case() {
        assert!(validate_migration_name("AddWidgetArchivedAt").is_err());
    }

    #[test]
    fn validate_migration_name_rejects_double_underscore() {
        assert!(validate_migration_name("add__widget").is_err());
    }

    #[test]
    fn resolve_free_version_uses_now_when_nothing_is_taken() {
        let now = dt("20260812120000");
        assert_eq!(
            resolve_free_version(&BTreeSet::new(), now).unwrap(),
            "20260812120000"
        );
    }

    #[test]
    fn resolve_free_version_bumps_past_an_exact_second_collision() {
        let mut taken = BTreeSet::new();
        taken.insert("20260812120000".to_owned());
        let now = dt("20260812120000");
        assert_eq!(resolve_free_version(&taken, now).unwrap(), "20260812120001");
    }

    #[test]
    fn resolve_free_version_skips_a_run_of_taken_seconds() {
        let mut taken = BTreeSet::new();
        for s in ["20260812120000", "20260812120001", "20260812120002"] {
            taken.insert(s.to_owned());
        }
        let now = dt("20260812120000");
        assert_eq!(resolve_free_version(&taken, now).unwrap(), "20260812120003");
    }

    #[test]
    fn resolve_free_version_sorts_after_a_future_taken_version_within_the_clock_skew_horizon() {
        let mut taken = BTreeSet::new();
        taken.insert("20260812120500".to_owned());
        let now = dt("20260812120000"); // 5 minutes behind the latest taken version
        assert_eq!(resolve_free_version(&taken, now).unwrap(), "20260812120501");
    }

    #[test]
    fn resolve_free_version_refuses_to_guess_past_a_week_ahead() {
        let mut taken = BTreeSet::new();
        taken.insert("20270101000000".to_owned()); // ~a mistyped year ahead
        let now = dt("20260812120000");
        let err = resolve_free_version(&taken, now).unwrap_err();
        assert!(err.contains("more than a week ahead"), "{err}");
    }

    #[test]
    fn resolve_free_version_rejects_a_calendar_invalid_max_taken_version() {
        // 14 digits, so `version_prefix` (and Diesel) accept it as a
        // version, but month 13 is not a real calendar date -- arithmetic on
        // it (bumping past it, checking the horizon) is undefined, so this
        // must error rather than silently drop it from consideration and
        // risk generating a version that sorts before it.
        let mut taken = BTreeSet::new();
        taken.insert("20261301000000".to_owned());
        let now = dt("20260812120000");
        let err = resolve_free_version(&taken, now).unwrap_err();
        assert!(err.contains("20261301000000"), "{err}");
        assert!(err.contains("not a valid calendar timestamp"), "{err}");
    }

    #[test]
    fn resolve_free_version_ignores_a_calendar_invalid_version_that_is_not_the_max() {
        // A garbage version that is NOT the lexical max (an earlier taken
        // version dominates it) never enters the arithmetic path at all, so
        // it must not block an otherwise-valid resolution.
        let mut taken = BTreeSet::new();
        taken.insert("20260812120000".to_owned());
        taken.insert("00000013000000".to_owned()); // 14 digits, garbage, but sorts first
        let now = dt("20260812120000");
        assert_eq!(resolve_free_version(&taken, now).unwrap(), "20260812120001");
    }

    #[test]
    fn resolve_free_version_is_never_before_the_max_taken_version() {
        let mut taken = BTreeSet::new();
        taken.insert("20260812120000".to_owned());
        taken.insert("20260812130000".to_owned());
        let now = dt("20260810000000"); // clock a couple days behind every taken version
        let version = resolve_free_version(&taken, now).unwrap();
        assert!(version.as_str() > "20260812130000", "{version}");
    }
}
