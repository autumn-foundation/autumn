//! A small file engine for tests of the engine-agnostic fleet core.
//!
//! One database is one text file. Line 1 is the key name, line 2 is the
//! epoch stamp, and each later line is one value.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::db::fleet::FleetError;
use crate::db::fleet_engine::{FleetEngine, ShipMode};
use crate::fleet_layout::FleetDbKey;
use crate::lock::FencingToken;

/// A file engine. Each open database holds `bytes` of memory.
#[derive(Debug)]
pub struct TextEngine {
    pub bytes: u64,
    /// When set, `open` does not check the key. A broken engine for the kit.
    pub skip_identity: bool,
    /// When set, `open` does not check or raise the epoch.
    pub skip_epoch: bool,
    /// When set, `remove` removes nothing.
    pub keep_on_remove: bool,
    pub opens: AtomicU64,
}

impl TextEngine {
    pub const fn new(bytes: u64) -> Self {
        Self {
            bytes,
            skip_identity: false,
            skip_epoch: false,
            keep_on_remove: false,
            opens: AtomicU64::new(0),
        }
    }

    /// Appends `value` to the database.
    pub fn write(handle: &TextHandle, value: &str) -> Result<(), String> {
        if handle.0.closed.load(Ordering::Acquire) {
            return Err("the handle is closed".to_owned());
        }
        let mut text = std::fs::read_to_string(&handle.0.path).map_err(|e| e.to_string())?;
        text.push_str(value);
        text.push('\n');
        std::fs::write(&handle.0.path, text).map_err(|e| e.to_string())
    }

    /// The values in the database.
    pub fn read(handle: &TextHandle) -> Result<Vec<String>, String> {
        let text = std::fs::read_to_string(&handle.0.path).map_err(|e| e.to_string())?;
        Ok(text.lines().skip(2).map(str::to_owned).collect())
    }
}

/// The open database of a [`TextEngine`].
#[derive(Debug, Clone)]
pub struct TextHandle(Arc<TextState>);

#[derive(Debug)]
pub struct TextState {
    path: PathBuf,
    users: AtomicUsize,
    closed: AtomicBool,
}

impl TextHandle {
    /// Marks the handle in use until the guard drops.
    pub fn checkout(&self) -> Checkout {
        self.0.users.fetch_add(1, Ordering::AcqRel);
        Checkout(Arc::clone(&self.0))
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
}

/// A user of a [`TextHandle`].
pub struct Checkout(Arc<TextState>);

impl Drop for Checkout {
    fn drop(&mut self) {
        self.0.users.fetch_sub(1, Ordering::AcqRel);
    }
}

fn io(op: &'static str) -> impl FnOnce(std::io::Error) -> FleetError {
    move |e| FleetError::Io {
        op,
        detail: e.to_string(),
    }
}

impl FleetEngine for TextEngine {
    type Handle = TextHandle;

    async fn open(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: Option<FencingToken>,
    ) -> Result<TextHandle, FleetError> {
        let name = key.name();
        let epoch = epoch.map_or(0, FencingToken::get);
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(io("read database"))?;
            let mut lines = text.lines();
            let found = lines.next().unwrap_or_default().to_owned();
            let stamp: u64 = lines.next().unwrap_or("0").parse().unwrap_or(0);
            if !self.skip_identity && found != name {
                return Err(FleetError::Misplaced { name, found });
            }
            if self.skip_epoch {
                // A broken engine: no check, no stamp.
            } else if stamp > epoch && epoch > 0 {
                return Err(FleetError::NewerEpoch {
                    name,
                    epoch,
                    found: stamp,
                });
            }
            if epoch > stamp && !self.skip_epoch {
                let rest: Vec<&str> = text.lines().skip(2).collect();
                let mut body = format!("{found}\n{epoch}\n");
                for line in rest {
                    body.push_str(line);
                    body.push('\n');
                }
                std::fs::write(path, body).map_err(io("stamp database"))?;
            }
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io("create directory"))?;
            }
            std::fs::write(path, format!("{name}\n{epoch}\n")).map_err(io("create database"))?;
        }
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(TextHandle(Arc::new(TextState {
            path: path.to_path_buf(),
            users: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        })))
    }

    fn close(&self, handle: &TextHandle) {
        handle.0.closed.store(true, Ordering::Release);
    }

    fn in_use(&self, handle: &TextHandle) -> bool {
        handle.0.users.load(Ordering::Acquire) > 0
    }

    async fn snapshot(
        &self,
        _handle: &TextHandle,
        path: &Path,
        dir: &Path,
    ) -> Result<Vec<PathBuf>, FleetError> {
        let file = path.file_name().ok_or_else(|| FleetError::Io {
            op: "snapshot database",
            detail: "the path has no file name".to_owned(),
        })?;
        let copy = dir.join(file);
        std::fs::copy(path, &copy).map_err(io("snapshot database"))?;
        Ok(vec![copy])
    }

    fn remove(&self, _key: &FleetDbKey, path: &Path) -> Result<bool, FleetError> {
        if self.keep_on_remove {
            return Ok(path.exists());
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io("delete database")(e)),
        }
    }

    fn resident_bytes(&self, _handle: &TextHandle) -> u64 {
        self.bytes
    }

    fn ship_mode(&self) -> ShipMode {
        ShipMode::Rebuild
    }
}
