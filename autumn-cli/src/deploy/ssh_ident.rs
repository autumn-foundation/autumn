//! Validated SSH user and host values for the deploy `ssh` and `scp` argv.
//!
//! `ssh` and `scp` read an argument that starts with `-` as an option. A
//! `ProxyCommand` option runs a local command. The `[deploy]` user and hosts
//! come from project config, so any contributor can set them.
//!
//! These types accept only a strict grammar. A value cannot start with `-`. A
//! value cannot hold a shell or `ssh_config` token character (`%`, `$`,
//! quotes, spaces).

use std::borrow::Cow;
use std::fmt;
use std::net::Ipv6Addr;

/// The longest SSH user name we accept, in bytes.
const MAX_USER_LEN: usize = 64;

/// The longest host name we accept, in bytes (the DNS limit).
const MAX_HOST_LEN: usize = 253;

/// A byte that a user or host name can hold: an ASCII letter or digit, `.`,
/// `_` or `-`.
const fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

/// A byte that a user name can hold. A user name can also hold `@`, for a
/// domain login such as `jdoe@corp.example`. `ssh` and `scp` split the
/// destination at the last `@`.
const fn is_user_byte(b: u8) -> bool {
    is_word_byte(b) || b == b'@'
}

/// A `[deploy]` user or host that is not safe to put in `ssh`/`scp` argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshIdentError {
    /// The `[deploy] user` value.
    InvalidUser(String),
    /// A `[deploy]` host value.
    InvalidHost(String),
}

impl fmt::Display for SshIdentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUser(raw) => write!(
                f,
                "`[deploy] user` {raw:?} is not a valid SSH user name. Use 1 to \
                 {MAX_USER_LEN} ASCII letters, digits, `.`, `_`, `-` or `@`. Start with a \
                 letter, digit or `_`"
            ),
            Self::InvalidHost(raw) => write!(
                f,
                "deploy host {raw:?} is not a valid SSH host. Use a plain IPv6 address, \
                 or a host name or IPv4 address of 1 to {MAX_HOST_LEN} ASCII letters, \
                 digits, `.`, `_` or `-`. Start a name with a letter or digit"
            ),
        }
    }
}

impl std::error::Error for SshIdentError {}

/// An SSH user name that is safe in `user@host` argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshUser(String);

impl SshUser {
    /// Parse a `[deploy] user` value. The value is not trimmed.
    ///
    /// # Errors
    ///
    /// Returns [`SshIdentError::InvalidUser`] when the value does not obey
    /// these rules:
    ///
    /// - 1 to [`MAX_USER_LEN`] ASCII letters, digits, `.`, `_`, `-` or `@`.
    /// - The first character is a letter, digit or `_`.
    pub fn parse(raw: &str) -> Result<Self, SshIdentError> {
        let first_ok = raw
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
        if first_ok && raw.len() <= MAX_USER_LEN && raw.bytes().all(is_user_byte) {
            Ok(Self(raw.to_owned()))
        } else {
            Err(SshIdentError::InvalidUser(raw.to_owned()))
        }
    }

    /// The user name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SshUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An SSH host that is safe in `user@host` and `user@host:path` argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost(String);

impl SshHost {
    /// Parse a `[deploy]` host value. The value is not trimmed.
    ///
    /// # Errors
    ///
    /// Returns [`SshIdentError::InvalidHost`] when the value is not one of:
    ///
    /// - A plain IPv6 address (no brackets, no zone).
    /// - 1 to [`MAX_HOST_LEN`] ASCII letters, digits, `.`, `_` or `-`. The
    ///   first character is a letter or digit.
    pub fn parse(raw: &str) -> Result<Self, SshIdentError> {
        let first_ok = raw
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
        let is_name = first_ok && raw.len() <= MAX_HOST_LEN && raw.bytes().all(is_word_byte);
        if is_name || raw.parse::<Ipv6Addr>().is_ok() {
            Ok(Self(raw.to_owned()))
        } else {
            Err(SshIdentError::InvalidHost(raw.to_owned()))
        }
    }

    /// The host as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The host for an `scp` remote operand. `scp` splits `host:path` at the
    /// first `:`, so an IPv6 address must be in brackets.
    #[must_use]
    pub fn scp_form(&self) -> Cow<'_, str> {
        // Only an IPv6 address can hold `:`. A name cannot.
        if self.0.contains(':') {
            Cow::Owned(format!("[{}]", self.0))
        } else {
            Cow::Borrowed(self.as_str())
        }
    }
}

impl fmt::Display for SshHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_HOST_LEN, MAX_USER_LEN, SshHost, SshIdentError, SshUser};
    use proptest::prelude::*;

    /// Values that `ssh` or `scp` read as an option.
    const OPTION_SHAPED: [&str; 6] = [
        "-oProxyCommand=touch /tmp/pwned",
        "-oProxyCommand=sh",
        "-v",
        "-",
        "--",
        "-F/dev/null",
    ];

    #[test]
    fn user_accepts_common_names() {
        for raw in [
            "root",
            "deploy",
            "deploy_user",
            "first.last",
            "_svc",
            "ci-bot",
            "User1",
            "jdoe@corp.example",
        ] {
            assert_eq!(SshUser::parse(raw).expect(raw).as_str(), raw);
        }
        let longest = "a".repeat(MAX_USER_LEN);
        assert!(SshUser::parse(&longest).is_ok());
    }

    #[test]
    fn user_refuses_option_shaped_values() {
        for raw in OPTION_SHAPED {
            assert_eq!(
                SshUser::parse(raw),
                Err(SshIdentError::InvalidUser(raw.to_owned())),
                "{raw}"
            );
        }
    }

    #[test]
    fn user_refuses_token_and_shell_characters() {
        let too_long = "a".repeat(MAX_USER_LEN + 1);
        for raw in [
            "",
            " deploy",
            "deploy ",
            "de ploy",
            "@corp.example",
            "a:b",
            "a;b",
            "$(id)",
            "`id`",
            "a%r",
            "a\nb",
            "a\tb",
            ".hidden",
            "a'b",
            "a\"b",
            "a\\b",
            "a/b",
            "d\u{e9}ploy",
            too_long.as_str(),
        ] {
            assert!(SshUser::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn host_accepts_names_and_addresses() {
        let longest = "a".repeat(MAX_HOST_LEN);
        for raw in [
            "203.0.113.10",
            "app.example.com",
            "app.example.com.",
            "web-1",
            "web_1",
            "localhost",
            "2001:db8::1",
            "::1",
            longest.as_str(),
        ] {
            assert_eq!(SshHost::parse(raw).expect(raw).as_str(), raw);
        }
    }

    #[test]
    fn host_refuses_option_shaped_values() {
        for raw in OPTION_SHAPED {
            assert_eq!(
                SshHost::parse(raw),
                Err(SshIdentError::InvalidHost(raw.to_owned())),
                "{raw}"
            );
        }
    }

    #[test]
    fn host_refuses_token_and_shell_characters() {
        let too_long = "a".repeat(MAX_HOST_LEN + 1);
        for raw in [
            "",
            " host",
            "host ",
            "host name",
            "a@b",
            "host:22",
            "[::1]",
            "fe80::1%eth0",
            "a;b",
            "$(id)",
            "%h",
            ".",
            ".host",
            "_host",
            "a/b",
            "h\u{f6}st",
            too_long.as_str(),
        ] {
            assert!(SshHost::parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn scp_form_brackets_only_ipv6() {
        assert_eq!(
            SshHost::parse("2001:db8::1").unwrap().scp_form(),
            "[2001:db8::1]"
        );
        assert_eq!(
            SshHost::parse("app.example.com").unwrap().scp_form(),
            "app.example.com"
        );
        assert_eq!(
            SshHost::parse("203.0.113.10").unwrap().scp_form(),
            "203.0.113.10"
        );
    }

    #[test]
    fn errors_name_the_key_and_the_value() {
        let user = SshUser::parse("-oX").unwrap_err().to_string();
        assert!(
            user.contains("`[deploy] user`") && user.contains("\"-oX\""),
            "{user}"
        );
        let host = SshHost::parse("-oX").unwrap_err().to_string();
        assert!(
            host.contains("deploy host") && host.contains("\"-oX\""),
            "{host}"
        );
    }

    proptest! {
        #[test]
        fn a_leading_dash_is_always_refused(rest in ".*") {
            let raw = format!("-{rest}");
            prop_assert!(SshUser::parse(&raw).is_err());
            prop_assert!(SshHost::parse(&raw).is_err());
        }

        #[test]
        fn an_accepted_user_is_kept_and_holds_no_option_or_token(
            raw in "[a-zA-Z0-9._:@ %$'-]{0,12}",
        ) {
            if let Ok(user) = SshUser::parse(&raw) {
                prop_assert_eq!(user.as_str(), raw.as_str());
                prop_assert!(!raw.starts_with(['-', '.', '@']));
                prop_assert!(!raw.contains([':', ' ', '%', '$', '\'']));
            }
        }

        #[test]
        fn an_accepted_host_is_a_name_or_an_ipv6_address(
            raw in "[a-fA-F0-9._:@ %$'-]{0,12}",
        ) {
            if let Ok(host) = SshHost::parse(&raw) {
                prop_assert_eq!(host.as_str(), raw.as_str());
                prop_assert!(!raw.starts_with('-'));
                prop_assert!(!raw.contains(['@', ' ', '%', '$', '\'']));
                if raw.contains(':') {
                    prop_assert!(raw.parse::<std::net::Ipv6Addr>().is_ok());
                }
            }
        }
    }
}
