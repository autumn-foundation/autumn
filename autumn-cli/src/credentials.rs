//! CLI implementation for `autumn credentials edit` and `autumn credentials show`.

use std::io::Write;
use std::path::{Path, PathBuf};

use autumn_web::credentials::{
    CredentialsError, MasterKey, credentials_path, decrypt, encrypt, load_credentials,
    resolve_master_key,
};

pub struct EditOptions {
    pub env: String,
}

pub struct ShowOptions {
    pub env: String,
    pub reveal: bool,
}

struct TempFileGuard {
    path: PathBuf,
    inner: Option<tempfile::NamedTempFile>,
}

impl TempFileGuard {
    fn new(env: &str, plaintext: &[u8]) -> std::io::Result<Self> {
        let mut file = tempfile::Builder::new()
            .prefix(&format!("autumn-credentials-{env}-"))
            .suffix(".toml")
            .tempfile()?;
        file.write_all(plaintext)?;
        file.flush()?;
        let path = file.path().to_path_buf();
        Ok(Self {
            path,
            inner: Some(file),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        zero_file(&self.path);
        self.inner = None;
    }
}

/// Run `autumn credentials edit [--env <env>]`.
///
/// Resolves the master key, decrypts the credentials file into a temp file,
/// opens `$VISUAL` / `$EDITOR` (falling back to `vi` on Unix, `notepad` on
/// Windows), re-encrypts on save, then zeroes and removes the temp file.
pub fn run_edit(opts: &EditOptions) -> Result<(), CredentialsError> {
    let base_dir = std::env::current_dir()?;
    edit_credentials(&opts.env, &base_dir)
}

fn edit_credentials(env: &str, base_dir: &Path) -> Result<(), CredentialsError> {
    let enc_path = credentials_path(env, base_dir);

    let plaintext = if enc_path.exists() {
        let key = resolve_master_key(base_dir).map_err(|e| {
            eprintln!("autumn credentials edit: {e}");
            e
        })?;
        let ciphertext = std::fs::read(&enc_path)?;
        decrypt(&key, &ciphertext)?
    } else {
        std::fs::create_dir_all(enc_path.parent().unwrap())?;
        default_credentials_template(env).into_bytes()
    };

    let tmp_guard = TempFileGuard::new(env, &plaintext)?;
    let tmp_path = tmp_guard.path();

    let editor = resolve_editor();
    let status = launch_editor(&editor, tmp_path)
        .map_err(|e| std::io::Error::other(format!("cannot launch editor '{editor}': {e}")))?;
    if !status.success() {
        return Err(CredentialsError::Io(std::io::Error::other(
            "editor exited with non-zero status",
        )));
    }
    let new_plaintext = std::fs::read(tmp_path)?;

    toml::from_str::<toml::Table>(std::str::from_utf8(&new_plaintext).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "file is not valid UTF-8")
    })?)
    .map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("TOML parse error: {e}"),
        )
    })?;

    let key = match resolve_master_key(base_dir) {
        Ok(k) => k,
        Err(CredentialsError::NoKeyFound) => {
            let k = MasterKey::generate();
            let key_path = base_dir.join("config/master.key");
            std::fs::create_dir_all(key_path.parent().unwrap())?;

            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&key_path)?;
            f.write_all(k.to_hex().as_bytes())?;

            println!("  Created config/master.key (keep this secret, do not commit)");
            k
        }
        Err(e) => return Err(e),
    };

    let ciphertext = encrypt(&key, &new_plaintext);

    let tmp_enc = enc_path.with_extension("enc.tmp");
    std::fs::write(&tmp_enc, &ciphertext)?;
    std::fs::rename(&tmp_enc, &enc_path)?;

    println!("  Saved {}", enc_path.display());
    Ok(())
}

/// Run `autumn credentials show [--env <env>] [--reveal]`.
pub fn run_show(opts: &ShowOptions) {
    let base_dir = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("autumn credentials: cannot determine current directory: {e}");
        std::process::exit(1);
    });

    let result = show_credentials(&opts.env, opts.reveal, &base_dir);
    if let Err(e) = result {
        eprintln!("autumn credentials show: {e}");
        std::process::exit(1);
    }
}

fn show_credentials(env: &str, reveal: bool, base_dir: &Path) -> Result<(), CredentialsError> {
    let enc_path = credentials_path(env, base_dir);
    if !enc_path.exists() {
        println!("No credentials file found at {}", enc_path.display());
        return Ok(());
    }

    let store = load_credentials(env, base_dir)?;

    if reveal {
        let key = resolve_master_key(base_dir)?;
        let ciphertext = std::fs::read(&enc_path)?;
        let plaintext = decrypt(&key, &ciphertext)?;
        let s = String::from_utf8(plaintext)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8"))?;
        print!("{s}");
    } else {
        println!("# Credentials for '{env}' (values redacted; use --reveal to show)");
        for key_name in store.keys() {
            println!("{key_name} = [REDACTED]");
        }
    }

    Ok(())
}

fn resolve_editor() -> String {
    if let Ok(v) = std::env::var("VISUAL")
        && !v.is_empty()
    {
        return v;
    }
    if let Ok(v) = std::env::var("EDITOR")
        && !v.is_empty()
    {
        return v;
    }
    if cfg!(windows) {
        "notepad".to_owned()
    } else {
        "vi".to_owned()
    }
}

fn launch_editor(
    editor: &str,
    file: &std::path::Path,
) -> std::io::Result<std::process::ExitStatus> {
    let (program, extra_args) = parse_editor_command(editor).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("could not parse editor command {editor:?} (unbalanced quotes?)"),
        )
    })?;
    std::process::Command::new(program)
        .args(extra_args)
        .arg(file)
        .status()
}

/// Split an `$VISUAL` / `$EDITOR` value into the program and its arguments.
///
/// Naive whitespace splitting runs the wrong binary when the editor path
/// contains a space (`C:\Program Files\Editor\edit.exe` would execute
/// `C:\Program`), so:
///
/// - a value that names an existing file is the program verbatim, spaces and
///   all, with no extra arguments;
/// - on Unix the value is split with POSIX shell quoting rules (`shlex`), so
///   `"/opt/my editor/bin/ed" --wait` resolves to the quoted path;
/// - on Windows (where `\` is a path separator, not an escape) a leading
///   double-quoted segment is the program and the rest is split on whitespace.
///
/// Returns `None` for an empty command or unbalanced quotes, so the caller
/// refuses to launch rather than guessing.
fn parse_editor_command(editor: &str) -> Option<(String, Vec<String>)> {
    let trimmed = editor.trim();
    if trimmed.is_empty() {
        return None;
    }
    if std::path::Path::new(trimmed).is_file() {
        return Some((trimmed.to_owned(), Vec::new()));
    }
    let mut parts = split_editor_words(trimmed)?.into_iter();
    let program = parts.next()?;
    Some((program, parts.collect()))
}

#[cfg(not(windows))]
fn split_editor_words(editor: &str) -> Option<Vec<String>> {
    shlex::split(editor)
}

#[cfg(windows)]
fn split_editor_words(editor: &str) -> Option<Vec<String>> {
    if let Some(rest) = editor.strip_prefix('"') {
        let (program, args) = rest.split_once('"')?;
        let mut words = vec![program.to_owned()];
        words.extend(args.split_whitespace().map(str::to_owned));
        Some(words)
    } else {
        Some(editor.split_whitespace().map(str::to_owned).collect())
    }
}

fn zero_file(path: &PathBuf) {
    use std::io::Write;
    if let (Ok(meta), Ok(mut f)) = (
        std::fs::metadata(path),
        std::fs::OpenOptions::new().write(true).open(path),
    ) {
        let mut remaining = usize::try_from(meta.len()).unwrap_or(usize::MAX);
        let chunk = [0u8; 4096];
        while remaining > 0 {
            let n = remaining.min(chunk.len());
            if f.write_all(&chunk[..n]).is_err() {
                break;
            }
            remaining -= n;
        }
        let _ = f.flush();
    }
}

fn default_credentials_template(env: &str) -> String {
    format!(
        "# Encrypted credentials for '{env}'\n\
         # Run `autumn credentials edit --env {env}` to edit these values.\n\
         # Do NOT commit config/master.key to version control.\n\
         \n\
         # stripe_secret_key = \"sk_live_...\"\n\
         # sendgrid_api_key = \"SG...\"\n\
         # s3_access_key_id = \"AKIA...\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::credentials::{MasterKey, encrypt};
    use tempfile::TempDir;

    fn setup_credentials(tmp: &TempDir, env: &str, content: &str) -> MasterKey {
        let key = MasterKey::generate();
        let ct = encrypt(&key, content.as_bytes());
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path()
                .join(format!("config/credentials/{env}.toml.enc")),
            &ct,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/master.key"), key.to_hex()).unwrap();
        key
    }

    #[test]
    fn resolve_editor_returns_visual_if_set() {
        temp_env::with_var("VISUAL", Some("nano"), || {
            assert_eq!(resolve_editor(), "nano");
        });
    }

    #[test]
    fn resolve_editor_falls_back_to_editor_var() {
        temp_env::with_vars(
            [("VISUAL", None::<&str>), ("EDITOR", Some("emacs"))],
            || {
                assert_eq!(resolve_editor(), "emacs");
            },
        );
    }

    #[test]
    fn resolve_editor_falls_back_to_platform_default() {
        temp_env::with_vars([("VISUAL", None::<&str>), ("EDITOR", None::<&str>)], || {
            let editor = resolve_editor();
            if cfg!(windows) {
                assert_eq!(editor, "notepad");
            } else {
                assert_eq!(editor, "vi");
            }
        });
    }

    #[test]
    fn parse_editor_command_splits_program_and_arguments() {
        assert_eq!(
            parse_editor_command("code --wait"),
            Some(("code".to_owned(), vec!["--wait".to_owned()]))
        );
    }

    #[test]
    fn parse_editor_command_rejects_empty_and_unbalanced_values() {
        assert_eq!(parse_editor_command(""), None);
        assert_eq!(parse_editor_command("   "), None);
        assert_eq!(parse_editor_command("\"/opt/my editor --wait"), None);
    }

    #[test]
    fn parse_editor_command_keeps_quoted_program_with_spaces_whole() {
        assert_eq!(
            parse_editor_command("\"/opt/my editor/bin/ed\" --wait"),
            Some((
                "/opt/my editor/bin/ed".to_owned(),
                vec!["--wait".to_owned()]
            ))
        );
    }

    #[test]
    fn parse_editor_command_treats_existing_path_with_spaces_as_program() {
        // Regression: `split_whitespace` turned `<dir>/my editor` into the
        // program `<dir>/my`, running a different binary than the user named.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("my editor");
        std::fs::write(&path, b"").unwrap();
        let editor = path.to_str().unwrap();
        assert_eq!(
            parse_editor_command(editor),
            Some((editor.to_owned(), Vec::new()))
        );
    }

    #[test]
    fn default_credentials_template_contains_env_name() {
        let t = default_credentials_template("production");
        assert!(t.contains("production"));
    }

    #[test]
    fn default_credentials_template_has_placeholder_comments() {
        let t = default_credentials_template("staging");
        assert!(t.contains("stripe_secret_key"));
        assert!(t.contains("sendgrid_api_key"));
    }

    #[test]
    fn show_credentials_redacted_when_reveal_false() {
        let tmp = TempDir::new().unwrap();
        setup_credentials(&tmp, "development", "stripe_key = \"sk_live_test\"\n");
        show_credentials("development", false, tmp.path()).unwrap();
    }

    #[test]
    fn show_credentials_no_file_does_not_error() {
        let tmp = TempDir::new().unwrap();
        show_credentials("development", false, tmp.path()).unwrap();
    }

    #[test]
    fn zero_file_overwrites_contents() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secret.txt");
        std::fs::write(&path, b"super secret data 12345").unwrap();
        zero_file(&path);
        let after = std::fs::read(&path).unwrap();
        assert!(after.iter().all(|&b| b == 0), "file should be zeroed");
    }

    #[test]
    fn zero_file_nonexistent_path_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.txt");
        zero_file(&path); // must not panic
    }

    #[test]
    fn show_credentials_with_reveal_prints_plaintext() {
        let tmp = TempDir::new().unwrap();
        setup_credentials(&tmp, "development", "stripe_key = \"sk_live_test\"\n");
        show_credentials("development", true, tmp.path()).unwrap();
    }

    #[test]
    fn show_credentials_reveal_no_key_returns_error() {
        use autumn_web::credentials::{MasterKey, encrypt};
        let tmp = TempDir::new().unwrap();
        // Write encrypted file but no master.key file
        let key = MasterKey::generate();
        let ct = encrypt(&key, b"x = \"y\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/development.toml.enc"),
            &ct,
        )
        .unwrap();
        // No master.key, no AUTUMN_MASTER_KEY env var → error
        temp_env::with_var("AUTUMN_MASTER_KEY", None::<&str>, || {
            let err = show_credentials("development", true, tmp.path()).unwrap_err();
            assert!(
                matches!(err, autumn_web::credentials::CredentialsError::NoKeyFound),
                "expected NoKeyFound, got {err}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn launch_editor_with_true_succeeds() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.toml");
        std::fs::write(&path, b"").unwrap();
        let status = launch_editor("true", &path).unwrap();
        assert!(status.success(), "true should exit 0");
    }

    #[cfg(unix)]
    #[test]
    fn launch_editor_splits_arguments() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.toml");
        std::fs::write(&path, b"").unwrap();
        // "sh -c true" splits into program="sh", extra_args=["-c", "true"],
        // then appends file as $0 → runs `true` and exits 0.
        let status = launch_editor("sh -c true", &path).unwrap();
        assert!(status.success());
    }

    #[test]
    fn test_temp_file_guard_zeroes_and_removes() {
        let path;
        {
            let guard = TempFileGuard::new("test-env", b"highly sensitive token data").unwrap();
            path = guard.path().to_path_buf();
            assert!(path.exists());
        }
        assert!(!path.exists(), "Temp file should be removed on drop");
    }

    #[test]
    fn edit_credentials_reuses_existing_master_key() {
        let tmp = TempDir::new().unwrap();
        let initial_key = MasterKey::generate();
        let key_dir = tmp.path().join("config");
        std::fs::create_dir_all(&key_dir).unwrap();
        let key_path = key_dir.join("master.key");
        std::fs::write(&key_path, initial_key.to_hex()).unwrap();

        let dummy_editor = if cfg!(windows) { "cmd /c type" } else { "cat" };

        temp_env::with_vars(
            [("VISUAL", Some(dummy_editor)), ("EDITOR", None::<&str>)],
            || {
                edit_credentials("production", tmp.path()).unwrap();
            },
        );

        let key_content = std::fs::read_to_string(&key_path).unwrap();
        assert_eq!(key_content, initial_key.to_hex());

        let enc_path = tmp.path().join("config/credentials/production.toml.enc");
        assert!(enc_path.exists());
        let ciphertext = std::fs::read(&enc_path).unwrap();
        let decrypted = decrypt(&initial_key, &ciphertext).unwrap();
        let decrypted_str = String::from_utf8(decrypted).unwrap();
        assert!(decrypted_str.contains("production"));
    }
}
