//! The platform-support policy, as code (issue #1616).
//!
//! Autumn's PRD promises "developers build on macOS and Windows; deploy on
//! Linux". That promise only means something if a Windows developer can find
//! out, *before* hitting it, which commands run natively and which need WSL2 —
//! and if the commands that need WSL2 say so instead of half-working.
//!
//! This module is the single source of truth for that policy. Three consumers
//! read the same table, so they cannot disagree:
//!
//! 1. `autumn doctor` reports the platform's tier status and prerequisites.
//! 2. Tier 2 commands fail fast on Windows through
//!    [`tier_two_windows_error`], naming the policy and the doc.
//! 3. `docs/guide/platform-support.md` is bound to this table by a parity test
//!    (`autumn-cli/tests/integration/platform_support_policy.rs`), so the
//!    published policy cannot drift from the shipped behaviour.
//!
//! Adding a command to the CLI does not require adding it here. Adding a
//! command that *behaves differently on Windows* does — otherwise the policy
//! silently under-describes the product, which is the exact failure mode this
//! module exists to prevent.

/// Where the published policy lives, quoted in every fail-fast message so a
/// developer who hits one can read the whole policy rather than guess at it.
///
/// `trunk-dev`, not `trunk`: a refusal has to link a page that resolves the day
/// it ships, and the policy reaches `trunk` only at the next release. Matches
/// [`crate::upgrade::migrations::GUIDE_BASE_URL`] and the README's installer
/// links.
pub const POLICY_DOC_URL: &str =
    "https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/guide/platform-support.md";

/// The support tier a journey falls into on native Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportTier {
    /// Tier 1 — works natively on Windows, and a `windows-latest` CI job
    /// exercises it end to end.
    Native,
    /// Tier 2 — supported on Windows via WSL2. Native invocation fails fast
    /// with an actionable error rather than degrading silently.
    Wsl2,
}

impl SupportTier {
    /// The tier's short label, as printed by `autumn doctor` and used by the
    /// docs parity test.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Native => "Tier 1 (native)",
            Self::Wsl2 => "Tier 2 (WSL2)",
        }
    }
}

/// One row of the policy: a journey, its tier, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyEntry {
    /// The command (or journey) exactly as a developer types/reads it.
    pub command: &'static str,
    /// The tier it falls into on native Windows.
    pub tier: SupportTier,
    /// One sentence a developer can act on: what works, or what to do instead.
    pub note: &'static str,
}

/// The policy. Order is the published order: the docs parity test parses the
/// guide's two tier tables and asserts each is exactly this table's rows for
/// that tier, in this order — so a row added, removed, or moved between tiers in
/// either file fails the build.
pub const POLICY: &[PolicyEntry] = &[
    PolicyEntry {
        command: "autumn new",
        tier: SupportTier::Native,
        note: "Scaffolds a project natively. `config/master.key` gets no \
               owner-only mode on Windows (there is no `chmod`), so it inherits \
               its directory's ACLs — fine under %USERPROFILE%.",
    },
    PolicyEntry {
        command: "autumn doctor",
        tier: SupportTier::Native,
        note: "Runs natively and reports this platform's tier status.",
    },
    PolicyEntry {
        command: "autumn setup",
        tier: SupportTier::Native,
        note: "Downloads the checksum-verified tailwindcss-windows-x64.exe.",
    },
    PolicyEntry {
        command: "autumn dev",
        tier: SupportTier::Native,
        note: "Edit/rebuild/reload works natively; the reload stops the app \
               cooperatively so shutdown hooks (managed Postgres teardown) run.",
    },
    PolicyEntry {
        command: "autumn test",
        tier: SupportTier::Native,
        note: "Delegates to cargo test, which is first-class on Windows.",
    },
    PolicyEntry {
        command: "autumn serve (foreground)",
        tier: SupportTier::Native,
        note: "Builds and runs the app in the foreground, binding TCP per config.",
    },
    PolicyEntry {
        command: "managed Postgres",
        tier: SupportTier::Native,
        note: "Resolves a %LOCALAPPDATA% data dir and shuts the cluster down \
               cleanly with the app.",
    },
    PolicyEntry {
        command: "autumn deploy check / plan",
        tier: SupportTier::Native,
        note: "Local-only: plan renders the unit and step list, check grades the \
               config and probes SSH reachability with a portable TCP connect. \
               Validate a deploy config here before running it from WSL2.",
    },
    PolicyEntry {
        command: "autumn serve --daemon / stop / status / restart",
        tier: SupportTier::Wsl2,
        note: "The daemon lifecycle is built on Unix domain sockets and POSIX \
               signals; run it inside WSL2.",
    },
    PolicyEntry {
        command: "autumn deploy up / rollback / status / maintenance",
        tier: SupportTier::Wsl2,
        note: "These reach the host over ssh/sh, and up/rollback/maintenance \
               stage secrets with Unix file modes; run them inside WSL2.",
    },
    PolicyEntry {
        command: "scripts/*.sh contributor gates",
        tier: SupportTier::Wsl2,
        note: "The contributor gate scripts are bash; run them inside WSL2.",
    },
    PolicyEntry {
        command: "SystemTest browser tests",
        tier: SupportTier::Wsl2,
        note: "Chromium version probing is satisfied by file existence on \
               Windows (#1456); the browser suites themselves are gated behind \
               the system-tests feature and are exercised on Linux.",
    },
];

/// Windows-specific prerequisites `autumn doctor` flags so they are discovered
/// before a build fails, not during one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsPrerequisite {
    /// What needs it.
    pub subject: &'static str,
    /// What to install/set.
    pub requirement: &'static str,
}

/// The prerequisites, in the order `autumn doctor` prints them.
pub const WINDOWS_PREREQUISITES: &[WindowsPrerequisite] = &[
    WindowsPrerequisite {
        subject: "autumn generate auth --passkeys",
        requirement: "OpenSSL via vcpkg with VCPKG_ROOT set (see docs/guide/generators.md)",
    },
    WindowsPrerequisite {
        subject: "autumn serve --daemon / deploy up / scripts/*.sh",
        requirement: "WSL2 (Tier 2)",
    },
];

/// Look up a journey's policy row by its exact policy name.
#[must_use]
pub fn entry_for(command: &str) -> Option<&'static PolicyEntry> {
    POLICY.iter().find(|entry| entry.command == command)
}

/// The commands in a given tier, in published order.
#[must_use]
pub fn commands_in_tier(tier: SupportTier) -> Vec<&'static str> {
    POLICY
        .iter()
        .filter(|entry| entry.tier == tier)
        .map(|entry| entry.command)
        .collect()
}

/// The actionable error a Tier 2 command prints when invoked on native Windows.
///
/// Every Tier 2 command routes its refusal through here so the wording, the
/// tier name, and the doc link are identical everywhere — the alternative is
/// eleven bespoke messages that each explain a little less than the last.
///
/// # Panics
///
/// Panics if `command` is not a Tier 2 row in [`POLICY`]. That is a programming
/// error (a caller invented a command name), caught by this module's tests.
#[must_use]
pub fn tier_two_windows_error(command: &str) -> String {
    let entry = entry_for(command)
        .unwrap_or_else(|| panic!("`{command}` is not named in the platform-support policy"));
    assert!(
        entry.tier == SupportTier::Wsl2,
        "`{command}` is {} — it must not fail fast on Windows",
        entry.tier.label()
    );
    // No program prefix here: each caller already prints its own (`autumn
    // serve: `, `autumn deploy: `), and two stacked prefixes read as a bug.
    format!(
        "`{command}` is {} on Windows, not native.\n  {}\n  Run it from a WSL2 \
         shell (`wsl` in Windows Terminal), or from Linux/macOS.\n  Platform \
         support policy: {POLICY_DOC_URL}",
        entry.tier.label(),
        entry.note,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row's tier, or `None` when the policy does not name it.
    fn tier_for(command: &str) -> Option<SupportTier> {
        entry_for(command).map(|entry| entry.tier)
    }

    #[test]
    fn every_ac_named_tier_one_command_is_tier_one() {
        // The issue names these explicitly as the Tier 1 floor. If one ever
        // moves to Tier 2, that is a policy change that must be deliberate.
        for command in [
            "autumn new",
            "autumn doctor",
            "autumn setup",
            "autumn dev",
            "autumn test",
            "autumn serve (foreground)",
            "managed Postgres",
        ] {
            assert_eq!(
                tier_for(command),
                Some(SupportTier::Native),
                "{command} must be Tier 1"
            );
        }
    }

    #[test]
    fn every_ac_named_tier_two_command_is_tier_two() {
        for command in [
            "autumn serve --daemon / stop / status / restart",
            "autumn deploy up / rollback / status / maintenance",
            "scripts/*.sh contributor gates",
        ] {
            assert_eq!(
                tier_for(command),
                Some(SupportTier::Wsl2),
                "{command} must be Tier 2"
            );
        }
    }

    #[test]
    fn the_deploy_group_is_split_by_what_actually_reaches_a_host() {
        // `autumn deploy` is not one tier. `up`/`rollback`/`status`/
        // `maintenance` open an SSH session (and the mutating ones stage secrets
        // with Unix file modes), so they are Tier 2. `check` and `plan` are
        // local-only — config grading, unit rendering, and a portable TCP
        // reachability probe — so they are Tier 1, and refusing them would both
        // be untrue and remove the only way a Windows developer can validate a
        // deploy config before switching to WSL2. `autumn-cli/src/deploy.rs`
        // draws the same line in `reaches_a_remote_host`.
        assert_eq!(
            tier_for("autumn deploy check / plan"),
            Some(SupportTier::Native)
        );
        assert_eq!(
            tier_for("autumn deploy up / rollback / status / maintenance"),
            Some(SupportTier::Wsl2)
        );
    }

    #[test]
    fn unknown_commands_have_no_tier() {
        assert_eq!(tier_for("autumn definitely-not-a-command"), None);
    }

    #[test]
    fn policy_rows_are_unique_and_documented() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in POLICY {
            assert!(
                seen.insert(entry.command),
                "duplicate policy row for `{}`",
                entry.command
            );
            assert!(
                !entry.note.trim().is_empty(),
                "`{}` has no actionable note",
                entry.command
            );
        }
    }

    #[test]
    fn tier_two_error_carries_no_program_prefix_of_its_own() {
        // Callers add theirs (`autumn serve: `, `autumn deploy: `). A prefix
        // here would stack into "autumn deploy: autumn: ...".
        for command in commands_in_tier(SupportTier::Wsl2) {
            let message = tier_two_windows_error(command);
            assert!(
                !message.starts_with("autumn:"),
                "message must not carry its own prefix: {message}"
            );
        }
    }

    #[test]
    fn tier_two_error_names_the_tier_the_fix_and_the_policy() {
        let message = tier_two_windows_error("autumn serve --daemon / stop / status / restart");
        assert!(message.contains("Tier 2 (WSL2)"), "{message}");
        assert!(message.contains("WSL2 shell"), "{message}");
        assert!(message.contains(POLICY_DOC_URL), "{message}");
        // The refusal must say what is actually unsupported, not just "unsupported".
        assert!(message.contains("Unix domain sockets"), "{message}");
    }

    #[test]
    fn tier_two_error_is_available_for_every_tier_two_row() {
        for command in commands_in_tier(SupportTier::Wsl2) {
            let message = tier_two_windows_error(command);
            assert!(message.contains(command), "{message}");
            assert!(message.contains(POLICY_DOC_URL), "{message}");
        }
    }

    #[test]
    #[should_panic(expected = "not named in the platform-support policy")]
    fn tier_two_error_rejects_an_unlisted_command() {
        let _ = tier_two_windows_error("autumn made-up");
    }

    #[test]
    #[should_panic(expected = "it must not fail fast on Windows")]
    fn tier_two_error_rejects_a_tier_one_command() {
        let _ = tier_two_windows_error("autumn dev");
    }

    #[test]
    fn windows_prerequisites_cover_the_documented_openssl_requirement() {
        // docs/guide/generators.md documents the vcpkg/OpenSSL requirement for
        // `--passkeys`; doctor must surface it rather than leave a Windows
        // developer to discover it through a linker error.
        let passkeys = WINDOWS_PREREQUISITES
            .iter()
            .find(|p| p.subject.contains("--passkeys"))
            .expect("doctor must flag the passkeys OpenSSL prerequisite");
        assert!(passkeys.requirement.contains("vcpkg"), "{passkeys:?}");
        assert!(passkeys.requirement.contains("VCPKG_ROOT"), "{passkeys:?}");
    }

    #[test]
    fn both_tiers_are_non_empty() {
        assert!(!commands_in_tier(SupportTier::Native).is_empty());
        assert!(!commands_in_tier(SupportTier::Wsl2).is_empty());
    }
}
