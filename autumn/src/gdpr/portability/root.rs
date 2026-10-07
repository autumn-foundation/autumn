//! Read and write a capsule directory without following links.
//!
//! On Unix, [`Root::open`] opens the capsule directory one time, with
//! `O_NOFOLLOW`. Every later list, read, and write goes through that handle,
//! one path segment at a time, with `O_NOFOLLOW` again. A process that swaps
//! the directory or an entry for a link after the open cannot redirect a read
//! or a write. Other targets fall back to path-based access.

use super::model::DataCapsuleError;

/// Refuse a directory that can hold no capsule file. A capsule is shallow, so
/// a deep tree is not a capsule, and the walk does not recurse into it.
fn check_depth(dir: &str) -> Result<(), DataCapsuleError> {
    if dir.split('/').count() >= super::archive::MAX_PATH_DEPTH {
        return Err(DataCapsuleError::Integrity(format!(
            "directory is deeper than a capsule: {dir}"
        )));
    }
    Ok(())
}

/// Read `file` in pieces into `chunk`, and give its length. A file never
/// sits in memory whole: a huge one fails after `max` bytes.
fn feed(
    rel: &str,
    file: std::fs::File,
    max: u64,
    chunk: &mut dyn FnMut(&[u8]),
) -> Result<u64, DataCapsuleError> {
    use std::io::Read as _;

    let mut file = file.take(max.saturating_add(1));
    let mut buf = vec![0_u8; 64 << 10];
    let mut total = 0_u64;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| DataCapsuleError::io(rel, e))?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
        too_large(rel, total, max)?;
        chunk(&buf[..n]);
    }
}

/// Fail when a file of `size` bytes is larger than `max`.
fn too_large(rel: &str, size: u64, max: u64) -> Result<(), DataCapsuleError> {
    if size > max {
        return Err(DataCapsuleError::Integrity(format!(
            "file is too large: {rel} ({size} bytes, at most {max})"
        )));
    }
    Ok(())
}

pub(super) use imp::Root;

#[cfg(unix)]
mod imp {
    use std::collections::BTreeSet;
    use std::io::Write as _;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::path::Path;

    use nix::dir::{Dir, Type};
    use nix::errno::Errno;
    use nix::fcntl::{AtFlags, OFlag, open, openat};
    use nix::sys::stat::{Mode, SFlag, fchmod, fstat, fstatat, mkdirat, mode_t};
    use nix::unistd::{UnlinkatFlags, unlinkat};

    use super::{DataCapsuleError, check_depth, too_large};

    const DIR_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_DIRECTORY)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);
    /// `O_NONBLOCK`: opening a FIFO must not wait for a writer.
    const FILE_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC)
        .union(OFlag::O_NONBLOCK);

    /// `O_EXCL`: a new file never replaces an entry, and never follows a link.
    const NEW_FILE_FLAGS: OFlag = OFlag::O_WRONLY
        .union(OFlag::O_CREAT)
        .union(OFlag::O_EXCL)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);
    const DIR_MODE: Mode = Mode::S_IRWXU;
    const FILE_MODE: Mode = Mode::S_IRUSR.union(Mode::S_IWUSR);

    /// An open capsule directory.
    #[derive(Debug)]
    pub(in crate::gdpr::portability) struct Root {
        fd: OwnedFd,
    }

    fn error(errno: Errno, what: &str) -> DataCapsuleError {
        match errno {
            // `O_NOFOLLOW` on a link gives `ELOOP` (`EMLINK` on FreeBSD).
            Errno::ELOOP | Errno::EMLINK | Errno::ENOTDIR => {
                DataCapsuleError::Integrity(format!("entry is a link or not a directory: {what}"))
            }
            Errno::ENOENT => DataCapsuleError::Integrity(format!("file is missing: {what}")),
            other => DataCapsuleError::io(what, std::io::Error::from(other)),
        }
    }

    /// `mode_t` is `u16` on macOS and `u32` on Linux.
    fn kind_of(mode: mode_t) -> SFlag {
        SFlag::from_bits_truncate(mode) & SFlag::S_IFMT
    }

    impl Root {
        pub(in crate::gdpr::portability) fn open(dir: &Path) -> Result<Self, DataCapsuleError> {
            open(dir, DIR_FLAGS, Mode::empty())
                .map(|fd| Self { fd })
                .map_err(|e| error(e, &dir.display().to_string()))
        }

        /// Open the directory `segments` below the root, one segment at a time.
        fn open_dir(
            &self,
            segments: &[&str],
            rel: &str,
        ) -> Result<Option<OwnedFd>, DataCapsuleError> {
            let mut current: Option<OwnedFd> = None;
            for segment in segments {
                let parent: BorrowedFd<'_> = current
                    .as_ref()
                    .map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
                current = Some(
                    openat(parent, *segment, DIR_FLAGS, Mode::empty())
                        .map_err(|e| error(e, rel))?,
                );
            }
            Ok(current)
        }

        #[cfg(test)]
        pub(in crate::gdpr::portability) fn read(
            &self,
            rel: &str,
        ) -> Result<Vec<u8>, DataCapsuleError> {
            self.read_at_most(rel, u64::MAX)
        }

        /// Read `rel`, or fail when it has more than `max` bytes. The size is
        /// checked before the read, and the read stops after `max` bytes.
        pub(in crate::gdpr::portability) fn read_at_most(
            &self,
            rel: &str,
            max: u64,
        ) -> Result<Vec<u8>, DataCapsuleError> {
            let mut bytes = Vec::new();
            self.read_chunks(rel, max, &mut |piece| bytes.extend_from_slice(piece))?;
            Ok(bytes)
        }

        /// Open `rel` without following a link, as a regular file of at most
        /// `max` bytes.
        fn open_regular(&self, rel: &str, max: u64) -> Result<std::fs::File, DataCapsuleError> {
            let mut segments: Vec<&str> = rel.split('/').collect();
            let name = segments
                .pop()
                .ok_or_else(|| DataCapsuleError::InvalidName(rel.to_owned()))?;
            let dir = self.open_dir(&segments, rel)?;
            let parent = dir.as_ref().map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
            let fd = openat(parent, name, FILE_FLAGS, Mode::empty()).map_err(|e| error(e, rel))?;
            let stat = fstat(&fd).map_err(|e| error(e, rel))?;
            if kind_of(stat.st_mode) != SFlag::S_IFREG {
                return Err(DataCapsuleError::Integrity(format!(
                    "entry is not a regular file: {rel}"
                )));
            }
            too_large(rel, u64::try_from(stat.st_size).unwrap_or(u64::MAX), max)?;
            Ok(std::fs::File::from(fd))
        }

        /// Feed `rel` to `chunk` in pieces, and give its length. Fails when it
        /// has more than `max` bytes, before the read and during it.
        pub(in crate::gdpr::portability) fn read_chunks(
            &self,
            rel: &str,
            max: u64,
            chunk: &mut dyn FnMut(&[u8]),
        ) -> Result<u64, DataCapsuleError> {
            let file = self.open_regular(rel, max)?;
            super::feed(rel, file, max, chunk)
        }

        pub(in crate::gdpr::portability) fn list_regular_files(
            &self,
        ) -> Result<BTreeSet<String>, DataCapsuleError> {
            let mut files = BTreeSet::new();
            walk(self.fd.as_fd(), "", &mut files)?;
            Ok(files)
        }

        /// Make the root owner-only and check that it is empty.
        pub(in crate::gdpr::portability) fn prepare(&self) -> Result<(), DataCapsuleError> {
            fchmod(&self.fd, DIR_MODE).map_err(|e| error(e, "."))?;
            let mut listing = Dir::openat(
                self.fd.as_fd(),
                ".",
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| error(e, "."))?;
            for entry in listing.iter() {
                let entry = entry.map_err(|e| error(e, "."))?;
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    return Err(DataCapsuleError::NotEmpty(".".into()));
                }
            }
            Ok(())
        }

        /// Remove the files in `written`, then each of their directories that
        /// is empty. An entry that ends in `/` is a directory. Content of
        /// another writer stays. Errors are ignored: this is a cleanup after a
        /// failure.
        pub(in crate::gdpr::portability) fn remove_written(&self, written: &[String]) {
            let mut dirs = BTreeSet::new();
            for rel in written {
                let mut segments: Vec<&str> = rel.split('/').collect();
                let Some(name) = segments.pop() else { continue };
                if !name.is_empty()
                    && let Ok(dir) = self.open_dir(&segments, rel)
                {
                    let parent = dir.as_ref().map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
                    let _ = unlinkat(parent, name, UnlinkatFlags::NoRemoveDir);
                }
                for depth in 1..=segments.len() {
                    dirs.insert(segments[..depth].join("/"));
                }
            }
            // Deepest first, so a parent is empty when its turn comes.
            let mut dirs: Vec<String> = dirs.into_iter().collect();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
            for rel in dirs {
                let mut segments: Vec<&str> = rel.split('/').collect();
                let Some(name) = segments.pop() else { continue };
                if let Ok(dir) = self.open_dir(&segments, &rel) {
                    let parent = dir.as_ref().map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
                    let _ = unlinkat(parent, name, UnlinkatFlags::RemoveDir);
                }
            }
        }

        /// Write a new owner-only file. Each directory is owner-only too.
        /// `created` gets each new entry before a step that can fail.
        pub(in crate::gdpr::portability) fn write(
            &self,
            rel: &str,
            bytes: &[u8],
            created: &mut Vec<String>,
        ) -> Result<(), DataCapsuleError> {
            let fd = self.create(rel, created)?;
            fchmod(&fd, FILE_MODE).map_err(|e| error(e, rel))?;
            std::fs::File::from(fd)
                .write_all(bytes)
                .map_err(|e| DataCapsuleError::io(rel, e))
        }

        /// Create the empty file `rel` and its directories. Each new entry
        /// goes into `created` as it is made; a directory ends in `/`.
        pub(in crate::gdpr::portability) fn create(
            &self,
            rel: &str,
            created: &mut Vec<String>,
        ) -> Result<OwnedFd, DataCapsuleError> {
            let mut segments: Vec<&str> = rel.split('/').collect();
            let name = segments
                .pop()
                .ok_or_else(|| DataCapsuleError::InvalidName(rel.to_owned()))?;
            let mut current: Option<OwnedFd> = None;
            let mut path = String::new();
            for segment in segments {
                let parent = current
                    .as_ref()
                    .map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
                path.push_str(segment);
                path.push('/');
                match mkdirat(parent, segment, DIR_MODE) {
                    Ok(()) => created.push(path.clone()),
                    Err(Errno::EEXIST) => {}
                    Err(e) => return Err(error(e, rel)),
                }
                let dir =
                    openat(parent, segment, DIR_FLAGS, Mode::empty()).map_err(|e| error(e, rel))?;
                // `mkdirat` applies the umask; set the mode on the handle.
                fchmod(&dir, DIR_MODE).map_err(|e| error(e, rel))?;
                current = Some(dir);
            }
            let parent = current
                .as_ref()
                .map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
            let fd = openat(parent, name, NEW_FILE_FLAGS, FILE_MODE).map_err(|e| error(e, rel))?;
            created.push(rel.to_owned());
            Ok(fd)
        }
    }

    /// The type of `name` in `dir`, from `fstatat` without following a link.
    fn stat_kind(dir: BorrowedFd<'_>, name: &str, rel: &str) -> Result<Type, DataCapsuleError> {
        let stat = fstatat(dir, name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(|e| error(e, rel))?;
        Ok(match kind_of(stat.st_mode) {
            k if k == SFlag::S_IFDIR => Type::Directory,
            k if k == SFlag::S_IFREG => Type::File,
            // Any other type fails the check in `walk`.
            _ => Type::Symlink,
        })
    }

    fn walk(
        dir: BorrowedFd<'_>,
        prefix: &str,
        files: &mut BTreeSet<String>,
    ) -> Result<(), DataCapsuleError> {
        let what = if prefix.is_empty() { "." } else { prefix };
        let mut listing = Dir::openat(
            dir,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| error(e, what))?;
        let mut entries = Vec::new();
        for entry in listing.iter() {
            let entry = entry.map_err(|e| error(e, what))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "." && name != ".." {
                entries.push((name, entry.file_type()));
            }
        }
        for (name, kind) in entries {
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // Some file systems do not give the type in the listing.
            let kind = kind.map_or_else(|| stat_kind(dir, &name, &rel), Ok)?;
            match kind {
                Type::Directory => {
                    check_depth(&rel)?;
                    let child = openat(dir, name.as_str(), DIR_FLAGS, Mode::empty())
                        .map_err(|e| error(e, &rel))?;
                    walk(child.as_fd(), &rel, files)?;
                }
                Type::File => {
                    files.insert(rel);
                }
                _ => {
                    return Err(DataCapsuleError::Integrity(format!(
                        "entry is not a regular file: {rel}"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod imp {
    use std::collections::BTreeSet;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use super::DataCapsuleError;

    /// A capsule directory, read by path.
    #[derive(Debug)]
    pub(in crate::gdpr::portability) struct Root {
        path: PathBuf,
    }

    impl Root {
        pub(in crate::gdpr::portability) fn open(dir: &Path) -> Result<Self, DataCapsuleError> {
            let meta = std::fs::symlink_metadata(dir).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    DataCapsuleError::Integrity(format!("file is missing: {}", dir.display()))
                } else {
                    DataCapsuleError::io(dir, e)
                }
            })?;
            if !meta.file_type().is_dir() {
                return Err(DataCapsuleError::Integrity(format!(
                    "entry is a link or not a directory: {}",
                    dir.display()
                )));
            }
            Ok(Self {
                path: dir.to_path_buf(),
            })
        }

        #[cfg(test)]
        pub(in crate::gdpr::portability) fn read(
            &self,
            rel: &str,
        ) -> Result<Vec<u8>, DataCapsuleError> {
            self.read_at_most(rel, u64::MAX)
        }

        /// Read `rel`, or fail when it has more than `max` bytes. The size is
        /// checked before the read, and the read stops after `max` bytes.
        pub(in crate::gdpr::portability) fn read_at_most(
            &self,
            rel: &str,
            max: u64,
        ) -> Result<Vec<u8>, DataCapsuleError> {
            let mut bytes = Vec::new();
            self.read_chunks(rel, max, &mut |piece| bytes.extend_from_slice(piece))?;
            Ok(bytes)
        }

        /// Feed `rel` to `chunk` in pieces, and give its length. Fails when it
        /// has more than `max` bytes, before the read and during it.
        pub(in crate::gdpr::portability) fn read_chunks(
            &self,
            rel: &str,
            max: u64,
            chunk: &mut dyn FnMut(&[u8]),
        ) -> Result<u64, DataCapsuleError> {
            let path = rel.split('/').fold(self.path.clone(), |p, s| p.join(s));
            let meta = std::fs::symlink_metadata(&path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    DataCapsuleError::Integrity(format!("file is missing: {rel}"))
                } else {
                    DataCapsuleError::io(&path, e)
                }
            })?;
            if !meta.file_type().is_file() {
                return Err(DataCapsuleError::Integrity(format!(
                    "entry is not a regular file: {rel}"
                )));
            }
            super::too_large(rel, meta.len(), max)?;
            let file = std::fs::File::open(&path).map_err(|e| DataCapsuleError::io(&path, e))?;
            super::feed(rel, file, max, chunk)
        }

        /// Make the root owner-only and check that it is empty.
        pub(in crate::gdpr::portability) fn prepare(&self) -> Result<(), DataCapsuleError> {
            crate::fs_atomic::ensure_owner_only_dir(&self.path)
                .map_err(|e| DataCapsuleError::io(&self.path, e))?;
            let mut entries =
                std::fs::read_dir(&self.path).map_err(|e| DataCapsuleError::io(&self.path, e))?;
            if entries.next().is_some() {
                return Err(DataCapsuleError::NotEmpty(self.path.clone()));
            }
            Ok(())
        }

        /// Remove the files in `written`, then each of their directories that
        /// is empty. An entry that ends in `/` is a directory. Content of
        /// another writer stays.
        pub(in crate::gdpr::portability) fn remove_written(&self, written: &[String]) {
            let mut dirs = BTreeSet::new();
            for rel in written {
                let mut segments: Vec<&str> = rel.split('/').collect();
                if segments.pop().is_some_and(|name| !name.is_empty()) {
                    let path = rel.split('/').fold(self.path.clone(), |p, s| p.join(s));
                    let _ = std::fs::remove_file(&path);
                }
                for depth in 1..=segments.len() {
                    dirs.insert(segments[..depth].join("/"));
                }
            }
            let mut dirs: Vec<String> = dirs.into_iter().collect();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
            for rel in dirs {
                let path = rel.split('/').fold(self.path.clone(), |p, s| p.join(s));
                // `remove_dir` removes only an empty directory.
                let _ = std::fs::remove_dir(path);
            }
        }

        /// Write a new owner-only file. Each directory is owner-only too.
        /// `created` gets each new entry; a directory ends in `/`.
        pub(in crate::gdpr::portability) fn write(
            &self,
            rel: &str,
            bytes: &[u8],
            created: &mut Vec<String>,
        ) -> Result<(), DataCapsuleError> {
            let mut dir = self.path.clone();
            let mut prefix = String::new();
            let mut segments: Vec<&str> = rel.split('/').collect();
            segments.pop();
            for segment in segments {
                dir.push(segment);
                prefix.push_str(segment);
                prefix.push('/');
                let existed = dir.is_dir();
                crate::fs_atomic::ensure_owner_only_dir(&dir)
                    .map_err(|e| DataCapsuleError::io(&dir, e))?;
                if !existed {
                    created.push(prefix.clone());
                }
            }
            let path = rel.split('/').fold(self.path.clone(), |p, s| p.join(s));
            // Never replace a file: another export can write here too.
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| DataCapsuleError::io(&path, e))?;
            created.push(rel.to_owned());
            file.write_all(bytes)
                .map_err(|e| DataCapsuleError::io(&path, e))
        }

        pub(in crate::gdpr::portability) fn list_regular_files(
            &self,
        ) -> Result<BTreeSet<String>, DataCapsuleError> {
            let mut files = BTreeSet::new();
            let mut stack = vec![(self.path.clone(), String::new())];
            while let Some((path, prefix)) = stack.pop() {
                let entries =
                    std::fs::read_dir(&path).map_err(|e| DataCapsuleError::io(&path, e))?;
                for entry in entries {
                    let entry = entry.map_err(|e| DataCapsuleError::io(&path, e))?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let rel = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}/{name}")
                    };
                    // `DirEntry::file_type` does not follow links.
                    let kind = entry
                        .file_type()
                        .map_err(|e| DataCapsuleError::io(entry.path(), e))?;
                    if kind.is_dir() {
                        super::check_depth(&rel)?;
                        stack.push((entry.path(), rel));
                    } else if kind.is_file() {
                        files.insert(rel);
                    } else {
                        return Err(DataCapsuleError::Integrity(format!(
                            "entry is not a regular file: {rel}"
                        )));
                    }
                }
            }
            Ok(files)
        }
    }
}
