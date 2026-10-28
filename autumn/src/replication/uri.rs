//! Translate between a `SQLite` URL and a database file (issue #2589).
//!
//! One parser and one encoder. The backup, the restore and the deploy use
//! them, so they name the same file, with the same options, that the app opens.

use std::path::{Path, PathBuf};

/// A `SQLite` URL resolved to its database file and its open options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseTarget {
    path: PathBuf,
    /// The raw `name=value` query options, in order, without `mode`.
    options: Vec<String>,
    /// The last `mode` was `ro`.
    read_only: bool,
}

impl DatabaseTarget {
    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The database file, by value.
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        self.path
    }

    /// A URI that opens this file with its own options and never creates it.
    ///
    /// The source options stay (a `vfs` sets the locks, item 12). The `mode`
    /// becomes `rw`, or stays `ro`, so a missing file fails the open (item 7).
    /// `None` only for a path Windows cannot carry as Unicode.
    #[must_use]
    pub fn no_create_uri(&self) -> Option<String> {
        let mode = if self.read_only { "mode=ro" } else { "mode=rw" };
        let mut uri = file_uri(&self.path)?;
        uri.push('?');
        for option in &self.options {
            uri.push_str(option);
            uri.push('&');
        }
        uri.push_str(mode);
        Some(uri)
    }
}

/// Resolve a `SQLite` URL. `None` for an in-memory or non-`SQLite` target.
///
/// Accepts `sqlite:///abs/path`, `sqlite:path` and `file:` URIs. A `file:`
/// URI is read the way `SQLite` reads it: options are percent-decoded, and the
/// last `mode` and the last `vfs` win.
#[must_use]
pub fn database_target(url: &str) -> Option<DatabaseTarget> {
    let target = if url.starts_with("file:") {
        url
    } else {
        let rest = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))?;
        if rest.is_empty() {
            return None;
        }
        rest
    };
    if target == ":memory:" {
        return None;
    }
    // Every other spelling is a literal path for `sqlite3_open`. Windows drops
    // a `/` before a drive for a plain name too, so `sqlite:///C:/x` is `C:/x`.
    let Some(uri) = target.strip_prefix("file:") else {
        return Some(DatabaseTarget {
            path: decoded_path(native_from_uri_path(
                target.as_bytes().to_vec(),
                cfg!(windows),
            ))?,
            options: Vec::new(),
            read_only: false,
        });
    };
    // A fragment ends the URI. The query holds options, not path.
    let uri = uri.split('#').next().unwrap_or(uri);
    let (path, query) = uri.split_once('?').unwrap_or((uri, ""));

    let mut options = Vec::new();
    let mut mode = Vec::new();
    let mut vfs = Vec::new();
    // `&` only: `SQLite` does not split on `;`.
    for option in query.split('&').filter(|o| !o.is_empty()) {
        let (name, value) = option.split_once('=').unwrap_or((option, ""));
        let name = c_string(percent_decode(name));
        if name == b"mode" {
            mode = c_string(percent_decode(value));
            continue;
        }
        if name == b"vfs" {
            vfs = c_string(percent_decode(value));
        }
        options.push(option.to_owned());
    }
    if mode == b"memory" || vfs == b"memdb" {
        return None;
    }

    // `file://<authority>/path`: only an empty or `localhost` authority is
    // valid, and the path starts at the `/` that ends it.
    let path = path.strip_prefix("//").map_or(path, |rest| {
        rest.find('/')
            .map_or("", |slash| rest.get(slash..).unwrap_or_default())
    });
    let decoded = c_string(percent_decode(path));
    // Check AFTER decoding: `file:%3Amemory%3A` is `:memory:`.
    if decoded.is_empty() || decoded == b":memory:" {
        return None;
    }
    Some(DatabaseTarget {
        path: decoded_path(native_from_uri_path(decoded, cfg!(windows)))?,
        options,
        read_only: mode == b"ro",
    })
}

/// Encode `path` as a `SQLite` `file:` URI with no query.
///
/// Each byte outside a small safe set is percent-encoded, so `%`, `?`, `#`
/// and non-UTF-8 bytes stay in the name. A name that starts with `file:` is
/// safe too, because the URI has its own `file:` prefix. An absolute path gets
/// `file://`, so a path `//x` cannot become an authority.
///
/// `None` only on Windows, for a path that is not valid Unicode.
#[must_use]
pub fn file_uri(path: &Path) -> Option<String> {
    use std::fmt::Write as _;

    let bytes = uri_path_from_native(native_bytes(path)?, cfg!(windows));
    let mut uri = String::with_capacity(bytes.len().saturating_add(8));
    uri.push_str(if bytes.first() == Some(&b'/') {
        "file://"
    } else {
        "file:"
    });
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    Some(uri)
}

/// A URI that opens `path` read-write and never creates it.
#[must_use]
pub fn no_create_uri(path: &Path) -> Option<String> {
    file_uri(path).map(|uri| uri + "?mode=rw")
}

/// The raw bytes of a path: the native bytes on POSIX, UTF-8 elsewhere.
#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the non-unix sibling can refuse; one signature keeps the caller single"
)]
fn native_bytes(path: &Path) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;

    Some(path.as_os_str().as_bytes().to_vec())
}

/// Windows names files in UTF-16, so a path that is not Unicode has no UTF-8
/// form.
#[cfg(not(unix))]
fn native_bytes(path: &Path) -> Option<Vec<u8>> {
    path.to_str().map(|text| text.as_bytes().to_vec())
}

/// Turn a decoded URI path into native path bytes.
///
/// `file:///C:/x` decodes to `/C:/x`. Windows needs the drive letter first,
/// so the slash goes (item 9). On POSIX `/C:/x` is a real path and stays.
fn native_from_uri_path(bytes: Vec<u8>, windows: bool) -> Vec<u8> {
    match bytes.as_slice() {
        [b'/', drive, b':', ..] if windows && drive.is_ascii_alphabetic() => {
            bytes.get(1..).unwrap_or_default().to_vec()
        }
        _ => bytes,
    }
}

/// Turn native path bytes into URI path bytes.
///
/// On Windows: `\` becomes `/`, a verbatim `\\?\` prefix goes, and a drive
/// path gets a leading `/`, so the URI is `file:///C:/x`. On POSIX the bytes
/// do not change.
fn uri_path_from_native(bytes: Vec<u8>, windows: bool) -> Vec<u8> {
    if !windows {
        return bytes;
    }
    let bytes: Vec<u8> = bytes
        .into_iter()
        .map(|byte| if byte == b'\\' { b'/' } else { byte })
        .collect();
    let bytes = strip_verbatim_prefix(bytes);
    match bytes.as_slice() {
        [drive, b':', ..] if drive.is_ascii_alphabetic() => [b"/".as_slice(), &bytes].concat(),
        _ => bytes,
    }
}

/// Remove a Windows verbatim prefix: `//?/C:/x` is `C:/x`, and
/// `//?/UNC/server/share` is `//server/share`. `canonicalize` returns this form.
fn strip_verbatim_prefix(bytes: Vec<u8>) -> Vec<u8> {
    if let Some(share) = bytes.strip_prefix(b"//?/UNC/") {
        return [b"//".as_slice(), share].concat();
    }
    if let Some(rest) = bytes.strip_prefix(b"//?/") {
        return rest.to_vec();
    }
    bytes
}

/// Turn decoded `SQLite` URI bytes into a path.
///
/// `SQLite` percent-decodes to BYTES and hands them to `open(2)`, so
/// `file:app%FF.db` names `app\xFF.db`: a real POSIX filename that is not
/// UTF-8. A lossy conversion names a different file.
#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the non-unix sibling genuinely refuses; one signature keeps the caller single"
)]
fn decoded_path(bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// Non-POSIX hosts name files in UTF-16, so a non-UTF-8 byte sequence names no
/// file at all. Refuse it rather than guess with a replacement character.
#[cfg(not(unix))]
fn decoded_path(bytes: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Cut decoded bytes at the first NUL, as the C API does: `file:app%00x.db`
/// opens `app`, and `mode=memory%00x` is `mode=memory`.
fn c_string(mut bytes: Vec<u8>) -> Vec<u8> {
    if let Some(nul) = bytes.iter().position(|byte| *byte == 0) {
        bytes.truncate(nul);
    }
    bytes
}

/// Percent-decode `SQLite` URI text the way `sqlite3_open` does.
///
/// `%HH` with two hex digits becomes that byte. Anything else stays, as in
/// `SQLite`'s own lenient parser. Returns BYTES: a filename need not be UTF-8.
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let decoded = if byte == b'%' {
            bytes
                .get(index.saturating_add(1))
                .zip(bytes.get(index.saturating_add(2)))
                .and_then(|(high, low)| {
                    let high = char::from(*high).to_digit(16)?;
                    let low = char::from(*low).to_digit(16)?;
                    u8::try_from(high.saturating_mul(16).saturating_add(low)).ok()
                })
        } else {
            None
        };
        if let Some(value) = decoded {
            out.push(value);
            index = index.saturating_add(3);
        } else {
            out.push(byte);
            index = index.saturating_add(1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use diesel::connection::SimpleConnection as _;
    use diesel::{Connection as _, SqliteConnection};

    /// Item 1: `SQLite` decodes option names and values first.
    #[test]
    fn an_encoded_mode_memory_is_in_memory() {
        for memory in [
            "file:app.db?mode=mem%6fry",
            "file:app.db?mo%64e=memory",
            "file:app.db?%6Dode=%6Demory",
            "file:app.db?mode=memory%00x",
            "file:app.db?mode%00x=memory",
            "file:app?vfs=memdb%00x",
        ] {
            assert_eq!(database_target(memory), None, "{memory}");
        }
    }

    /// Item 2: the last `mode` wins.
    #[test]
    fn the_last_mode_wins() {
        let durable = database_target("file:app.db?mode=memory&mode=rwc").expect("a file");
        assert_eq!(durable.path(), Path::new("app.db"));
        assert_eq!(database_target("file:app.db?mode=rwc&mode=memory"), None);
    }

    /// Item 6: the `memdb` VFS is in memory. The last decoded `vfs` wins.
    #[test]
    fn the_memdb_vfs_is_in_memory() {
        for memory in [
            "file:app?vfs=memdb",
            "file:app?vfs=mem%64b",
            "file:app?vfs=unix&vfs=memdb",
        ] {
            assert_eq!(database_target(memory), None, "{memory}");
        }
        assert!(database_target("file:app?vfs=memdb&vfs=unix").is_some());
    }

    /// Item 9: `file:///C:/x` names `C:/x` on Windows. On POSIX it stays `/C:/x`.
    #[test]
    fn a_windows_drive_loses_its_leading_slash_only_on_windows() {
        assert_eq!(
            native_from_uri_path(b"/C:/data/app.db".to_vec(), true),
            b"C:/data/app.db"
        );
        assert_eq!(
            native_from_uri_path(b"/C:/data/app.db".to_vec(), false),
            b"/C:/data/app.db"
        );
        assert_eq!(
            native_from_uri_path(b"/data/app.db".to_vec(), true),
            b"/data/app.db"
        );
        let to_uri = |text: &str, windows| uri_path_from_native(text.as_bytes().to_vec(), windows);
        assert_eq!(to_uri(r"C:\data\app.db", true), b"/C:/data/app.db");
        assert_eq!(to_uri(r"\\?\C:\data\app.db", true), b"/C:/data/app.db");
        assert_eq!(
            to_uri(r"\\?\UNC\server\share\app.db", true),
            b"//server/share/app.db"
        );
        assert_eq!(to_uri(r"a\b.db", false), br"a\b.db");
    }

    /// Item 9 on a real Windows host: URI to path and back to a no-create URI.
    #[cfg(windows)]
    #[test]
    fn a_windows_file_uri_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let mut conn = SqliteConnection::establish(path.to_str().expect("utf-8")).expect("create");
        conn.batch_execute("CREATE TABLE t (id INTEGER);")
            .expect("write");
        drop(conn);

        let uri = file_uri(&path).expect("a URI");
        assert!(uri.starts_with("file:///"), "{uri}");
        let target = database_target(&uri).expect("a file");
        assert!(target.path().is_absolute(), "{:?}", target.path());
        assert!(target.path().exists(), "{:?}", target.path());
        let reopened = target.no_create_uri().expect("a URI");
        SqliteConnection::establish(&reopened).expect("opens the same file");

        // A plain `sqlite:///C:/…` path names the same drive path.
        let plain = format!(
            "sqlite:///{}",
            path.to_str().expect("utf-8").replace('\\', "/")
        );
        let target = database_target(&plain).expect("a file");
        assert!(target.path().exists(), "{plain} -> {:?}", target.path());
    }

    /// The encoder and the parser are inverse, for every awkward name.
    #[test]
    fn file_uri_round_trips_through_the_parser() {
        for path in [
            "app.db",
            "file:prod.db",
            "file:backups/out.sqlite",
            "a%20b?c#d.db",
            "/srv/app data/app.db",
            "//double/slash.db",
            "/srv/file:prod.db",
            "dir/mode=memory.db",
        ] {
            let uri = file_uri(Path::new(path)).expect("a URI");
            assert!(uri.starts_with("file:"), "{uri}");
            assert_eq!(
                database_target(&uri).map(|t| t.path().to_path_buf()),
                Some(PathBuf::from(path)),
                "{path} -> {uri}"
            );
        }
    }

    /// Item 3 and 15: non-UTF-8 bytes survive the round trip.
    #[cfg(unix)]
    #[test]
    fn file_uri_keeps_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt as _;

        let path = Path::new(std::ffi::OsStr::from_bytes(b"/srv/app\xFF.db"));
        let uri = file_uri(path).expect("a URI");
        assert_eq!(uri, "file:///srv/app%FF.db");
        assert_eq!(database_target(&uri).expect("a file").path(), path);
    }

    #[cfg(unix)]
    proptest::proptest! {
        /// Every POSIX name (any bytes but NUL) survives the round trip.
        #[test]
        fn any_posix_name_round_trips(bytes in proptest::collection::vec(1_u8..=255, 1..40)) {
            use std::os::unix::ffi::OsStrExt as _;

            let path = Path::new(std::ffi::OsStr::from_bytes(&bytes));
            proptest::prop_assume!(path.as_os_str() != ":memory:");
            let uri = file_uri(path).expect("a URI");
            let back = database_target(&uri).map(DatabaseTarget::into_path);
            proptest::prop_assert_eq!(back.as_deref(), Some(path), "{}", uri);
        }
    }

    /// Item 12: the source options carry through. Only `mode` changes.
    #[test]
    fn the_no_create_uri_keeps_the_vfs_and_drops_the_old_mode() {
        let target = database_target("file:/srv/app.db?vfs=unix-dotfile&mode=rwc&cache=shared")
            .expect("a file");
        assert_eq!(
            target.no_create_uri().as_deref(),
            Some("file:///srv/app.db?vfs=unix-dotfile&cache=shared&mode=rw")
        );
        let hidden = database_target("file:/srv/app.db?mode=ro%00x").expect("a file");
        assert!(
            hidden
                .no_create_uri()
                .is_some_and(|uri| uri.ends_with("mode=ro"))
        );
        let read_only = database_target("file:/srv/app.db?mode=ro").expect("a file");
        assert_eq!(
            read_only.no_create_uri().as_deref(),
            Some("file:///srv/app.db?mode=ro")
        );
        let plain = database_target("sqlite:///srv/a?b.db").expect("a file");
        assert_eq!(
            plain.no_create_uri().as_deref(),
            Some("file:///srv/a%3Fb.db?mode=rw")
        );
    }

    /// Item 7: the no-create URI never makes a file.
    #[test]
    fn the_no_create_uri_refuses_a_missing_file_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("gone.db");
        let uri = no_create_uri(&missing).expect("a URI");
        assert!(SqliteConnection::establish(&uri).is_err());
        assert!(!missing.exists(), "the open must not create {missing:?}");
    }

    /// The no-create URI opens an existing file with an awkward name. Unix
    /// only: Windows does not allow `?` or `:` in a name.
    #[cfg(unix)]
    #[test]
    fn the_no_create_uri_opens_an_existing_awkward_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file:a%b?c#d.db");
        let mut conn = SqliteConnection::establish(path.to_str().expect("utf-8")).expect("create");
        conn.batch_execute("CREATE TABLE t (id INTEGER);")
            .expect("write");
        drop(conn);

        let uri = no_create_uri(&path).expect("a URI");
        let mut conn = SqliteConnection::establish(&uri).expect("opens");
        conn.batch_execute("SELECT * FROM t;")
            .expect("the same file, with its table");
    }
}
