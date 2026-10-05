//! Read a capsule directory without following links.
//!
//! On Unix, [`Root::open`] opens the capsule directory one time, with
//! `O_NOFOLLOW`. Every later list and read goes through that handle, one
//! path segment at a time, with `O_NOFOLLOW` again. A process that swaps the
//! directory or an entry for a link after the open cannot redirect a read.
//! Other targets fall back to path-based reads.

use super::model::DataCapsuleError;

pub(super) use imp::Root;

#[cfg(unix)]
mod imp {
    use std::collections::BTreeSet;
    use std::io::Read as _;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::path::Path;

    use nix::dir::{Dir, Type};
    use nix::errno::Errno;
    use nix::fcntl::{AtFlags, OFlag, open, openat};
    use nix::sys::stat::{Mode, SFlag, fstat, fstatat};

    use super::DataCapsuleError;

    const DIR_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_DIRECTORY)
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC);
    /// `O_NONBLOCK`: opening a FIFO must not wait for a writer.
    const FILE_FLAGS: OFlag = OFlag::O_RDONLY
        .union(OFlag::O_NOFOLLOW)
        .union(OFlag::O_CLOEXEC)
        .union(OFlag::O_NONBLOCK);

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

    fn kind_of(mode: u32) -> SFlag {
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

        pub(in crate::gdpr::portability) fn read(
            &self,
            rel: &str,
        ) -> Result<Vec<u8>, DataCapsuleError> {
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
            let mut bytes = Vec::new();
            std::fs::File::from(fd)
                .read_to_end(&mut bytes)
                .map_err(|e| DataCapsuleError::io(rel, e))?;
            Ok(bytes)
        }

        pub(in crate::gdpr::portability) fn list_regular_files(
            &self,
        ) -> Result<BTreeSet<String>, DataCapsuleError> {
            let mut files = BTreeSet::new();
            walk(self.fd.as_fd(), "", &mut files)?;
            Ok(files)
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
    use std::path::{Path, PathBuf};

    use super::DataCapsuleError;

    /// A capsule directory, read by path.
    #[derive(Debug)]
    pub(in crate::gdpr::portability) struct Root {
        path: PathBuf,
    }

    impl Root {
        pub(in crate::gdpr::portability) fn open(dir: &Path) -> Result<Self, DataCapsuleError> {
            let meta = std::fs::symlink_metadata(dir).map_err(|e| DataCapsuleError::io(dir, e))?;
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

        pub(in crate::gdpr::portability) fn read(
            &self,
            rel: &str,
        ) -> Result<Vec<u8>, DataCapsuleError> {
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
            std::fs::read(&path).map_err(|e| DataCapsuleError::io(path, e))
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
