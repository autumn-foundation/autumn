//! File-emission engine shared by every generator.
//!
//! Generators describe what they want to do as a list of [`Action`]s, and
//! [`Plan::execute`] handles all the side-effecting filesystem work — including
//! collision detection, `--force` / `--dry-run`, and the human-readable
//! "Created/Modified" output that mirrors `autumn new`.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{Flags, GenerateError};

/// One filesystem operation the generator wants to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create a new file. Treated as a collision if the file already exists.
    Create { path: PathBuf, contents: String },
    /// Modify an existing file (or create it if absent). Never a collision.
    Modify { path: PathBuf, contents: String },
    /// Create a new file from raw bytes, with no UTF-8 or template handling.
    /// Used for verbatim/binary starter assets (e.g. vendored JS). Treated as a
    /// collision if the file already exists, like [`Action::Create`].
    CreateBytes { path: PathBuf, bytes: Vec<u8> },
    /// Create a file only if it does not already exist; skip silently if it
    /// does. Uses an exclusive-create open (analogous to `O_CREAT|O_EXCL`) so
    /// concurrent generator invocations cannot both win the race.
    CreateIfAbsent { path: PathBuf, contents: String },
}

impl Action {
    /// The path this action targets.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Create { path, .. }
            | Self::Modify { path, .. }
            | Self::CreateBytes { path, .. }
            | Self::CreateIfAbsent { path, .. } => path,
        }
    }
}

/// A complete generator plan — a sequence of actions plus the project root
/// they are anchored against.
#[derive(Debug, Default)]
pub struct Plan {
    /// Project root all action paths are interpreted relative to.
    pub project_root: PathBuf,
    /// The actions this plan will perform when executed.
    pub actions: Vec<Action>,
    /// Advisory messages surfaced to the user on [`Plan::execute`] (both
    /// `--dry-run` and a real run), without failing the generator — e.g. a
    /// `references` field whose target model doesn't exist yet.
    pub warnings: Vec<String>,
}

impl Plan {
    /// Create an empty plan rooted at `project_root`.
    #[must_use]
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            actions: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Record an advisory message, printed by [`Plan::execute`] but never
    /// fatal to the plan.
    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// Push a [`Action::Create`] action.
    pub fn create(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.actions.push(Action::Create {
            path: path.into(),
            contents: contents.into(),
        });
    }

    /// Push a [`Action::Modify`] action.
    pub fn modify(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.actions.push(Action::Modify {
            path: path.into(),
            contents: contents.into(),
        });
    }

    /// Push a [`Action::CreateBytes`] action (verbatim/binary file).
    pub fn create_bytes(&mut self, path: impl Into<PathBuf>, bytes: impl Into<Vec<u8>>) {
        self.actions.push(Action::CreateBytes {
            path: path.into(),
            bytes: bytes.into(),
        });
    }

    /// Push a [`Action::CreateIfAbsent`] action.
    ///
    /// The file is created atomically using an exclusive open; if the file
    /// already exists (including from a concurrent generator run) the action is
    /// silently skipped — the existing content is left untouched.
    pub fn create_if_absent(&mut self, path: impl Into<PathBuf>, contents: impl Into<String>) {
        self.actions.push(Action::CreateIfAbsent {
            path: path.into(),
            contents: contents.into(),
        });
    }

    /// All `Create` actions whose target file already exists on disk.
    fn collisions(&self) -> Vec<PathBuf> {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Create { path, .. } | Action::CreateBytes { path, .. } if path.exists() => {
                    Some(path.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Run the plan, honouring `--dry-run` and `--force`.
    ///
    /// On `--dry-run` we print the action list and exit early without touching
    /// the filesystem. On a real run we emit a `Created`/`Modified` line per
    /// action, in the same style as `autumn new`.
    ///
    /// # Errors
    /// Returns [`GenerateError::Collisions`] when any `Create` would overwrite
    /// an existing file and `--force` was not passed; or [`GenerateError::Io`]
    /// for filesystem failures during emission.
    pub fn execute(&self, flags: Flags) -> Result<(), GenerateError> {
        if flags.dry_run {
            self.print_warnings();
            self.print_dry_run();
            return Ok(());
        }

        if !flags.force {
            let collisions = self.collisions();
            if !collisions.is_empty() {
                return Err(GenerateError::Collisions(collisions));
            }
        }

        self.print_warnings();

        // Make sure parent directories of every file action exist.
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for action in &self.actions {
            let path = action.path();
            if let Some(parent) = path.parent() {
                dirs.insert(parent.to_path_buf());
            }
        }
        for dir in &dirs {
            fs::create_dir_all(dir)?;
        }

        for action in &self.actions {
            let path = action.path();
            match action {
                Action::Create { contents, .. } | Action::Modify { contents, .. } => {
                    let label = if matches!(action, Action::Modify { .. }) && path.exists() {
                        "Modified"
                    } else {
                        "Created"
                    };
                    fs::write(path, contents)?;
                    println!("  {label} {}", relative_display(path, &self.project_root));
                }
                Action::CreateBytes { bytes, .. } => {
                    fs::write(path, bytes)?;
                    println!("  Created {}", relative_display(path, &self.project_root));
                }
                Action::CreateIfAbsent { contents, .. } => {
                    match fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                    {
                        Ok(mut f) => {
                            if let Err(e) = f.write_all(contents.as_bytes()) {
                                // Remove the empty/partial file so the next run
                                // does not skip creation due to AlreadyExists.
                                let _ = fs::remove_file(path);
                                return Err(GenerateError::Io(e));
                            }
                            println!("  Created {}", relative_display(path, &self.project_root));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            // Another process already created the file; leave it untouched.
                        }
                        Err(e) => return Err(GenerateError::Io(e)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Print every advisory warning to stderr. Called only on a path that
    /// will actually run (dry-run or a real, non-colliding execution) — never
    /// before a `--force`/collision check that might abort the plan, so a
    /// failed, no-op run never emits misleading warnings about a plan that
    /// was never applied.
    fn print_warnings(&self) {
        for warning in &self.warnings {
            eprintln!("Warning: {warning}");
        }
    }

    fn print_dry_run(&self) {
        println!("Dry run — no files written.");
        for action in &self.actions {
            let label = match action {
                Action::Create { path, .. } | Action::CreateBytes { path, .. } if path.exists() => {
                    "Would overwrite"
                }
                Action::Modify { path, .. } if path.exists() => "Would modify",
                Action::CreateIfAbsent { path, .. } if path.exists() => "Would skip (exists)",
                Action::Create { .. }
                | Action::Modify { .. }
                | Action::CreateBytes { .. }
                | Action::CreateIfAbsent { .. } => "Would create",
            };
            println!(
                "  {label} {}",
                relative_display(action.path(), &self.project_root)
            );
        }
    }
}

fn relative_display(path: &Path, root: &Path) -> String {
    let display = path
        .strip_prefix(root)
        .map_or_else(|_| path.display().to_string(), |p| p.display().to_string());
    // Always render with forward slashes so the generator's output (and any
    // tests that grep for it) is platform-consistent.
    display.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Plan) {
        let tmp = tempfile::TempDir::new().unwrap();
        let plan = Plan::new(tmp.path());
        (tmp, plan)
    }

    #[test]
    fn new_plan_has_no_warnings() {
        let (_tmp, plan) = fixture();
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn warn_records_message_and_execute_does_not_fail() {
        let (_tmp, mut plan) = fixture();
        plan.warn("referenced table 'posts' is assumed to exist");
        assert_eq!(plan.warnings.len(), 1);
        // A warning never fails the plan — it's advisory only.
        plan.execute(Flags::default()).unwrap();
    }

    #[test]
    fn warnings_do_not_block_a_collision_error() {
        // A colliding Create must still fail with Collisions even when the
        // plan also carries a warning (the warning is printed to stderr,
        // which this test can't capture in-process, but it must never be
        // allowed to suppress or alter the underlying error).
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "existing").unwrap();
        plan.warn("some advisory message");
        plan.create(target, "new");
        let err = plan.execute(Flags::default()).unwrap_err();
        assert!(matches!(err, GenerateError::Collisions(_)));
    }

    #[test]
    fn create_action_writes_file() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags::default()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn create_action_creates_parent_dirs() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("nested/dir/out.txt");
        plan.create(target.clone(), "hi");
        plan.execute(Flags::default()).unwrap();
        assert!(target.exists());
    }

    #[test]
    fn collision_without_force_errors() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "old").unwrap();
        plan.create(target.clone(), "new");
        let err = plan.execute(Flags::default()).unwrap_err();
        match err {
            GenerateError::Collisions(paths) => {
                assert_eq!(paths, vec![target.clone()]);
            }
            _ => panic!("expected collision error, got {err:?}"),
        }
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn collision_with_force_overwrites() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "old").unwrap();
        plan.create(target.clone(), "new");
        plan.execute(Flags {
            force: true,
            dry_run: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn modify_action_overwrites_without_force() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "old").unwrap();
        plan.modify(target.clone(), "new");
        plan.execute(Flags::default()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        plan.create(target.clone(), "hello");
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn dry_run_skips_collision_check() {
        let (tmp, mut plan) = fixture();
        let target = tmp.path().join("out.txt");
        fs::write(&target, "existing").unwrap();
        plan.create(target.clone(), "new");
        plan.execute(Flags {
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "existing");
    }

    #[test]
    fn collision_lists_every_offender() {
        let (tmp, mut plan) = fixture();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, "x").unwrap();
        fs::write(&b, "y").unwrap();
        plan.create(a.clone(), "1");
        plan.create(b.clone(), "2");
        let err = plan.execute(Flags::default()).unwrap_err();
        let msg = err.to_string();
        // The error message normalises path separators to `/` so the
        // assertion needs to match that form (Windows uses `\` natively).
        assert!(msg.contains(a.display().to_string().replace('\\', "/").as_str()));
        assert!(msg.contains(b.display().to_string().replace('\\', "/").as_str()));
    }
}
