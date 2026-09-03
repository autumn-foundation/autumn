//! Deploy-time verification: is the posture manifest in this artifact the one
//! CI acknowledged, and is it the one CI signed?
//!
//! Two independent questions, one command:
//!
//! - **Content** — recompute the manifest's posture digest and compare it to
//!   the digest recorded when the change was acknowledged. Answers "is this the
//!   posture a human approved", and is immune to cosmetic regeneration.
//! - **Signature** — `gh attestation verify`, i.e. exactly the keyless Sigstore
//!   pipeline #1615 already ships. Answers "did this file come out of our CI,
//!   unmodified". No second signing story, no key custody, no new dependency
//!   beyond the `gh` the supply-chain guide already asks for.
//!
//! The signature half is *not* optional by default. A verification that quietly
//! degrades to "well, the digest matched" when `gh` is missing is a verification
//! that proves nothing about provenance, so the escape hatch
//! ([`VerifyOptions::skip_signature`]) is explicit, loud, and documented as
//! air-gapped-only.

use std::process::Command;

use super::model::{ManifestError, PostureManifest};

/// What to verify.
#[derive(Debug, Clone)]
pub struct VerifyOptions<'a> {
    /// Path to the posture manifest to check.
    pub manifest: &'a str,
    /// The digest CI acknowledged (full, or a prefix of at least
    /// [`MIN_DIGEST_PREFIX`] hex characters).
    pub expect_digest: Option<&'a str>,
    /// `owner/repo` the attestation was minted by.
    pub repo: Option<&'a str>,
    /// Skip the signature check. Air-gapped use only; says so in the output.
    pub skip_signature: bool,
}

/// Shortest digest prefix that may stand in for a full digest. Shared with the
/// acknowledgment marker so there is exactly one rule about how much of a
/// digest is enough.
pub const MIN_DIGEST_PREFIX: usize = super::ack::SHORT_DIGEST_LEN;

/// One thing that was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub passed: bool,
    /// Deliberately not performed, on the operator's explicit instruction.
    pub waived: bool,
    pub detail: String,
}

/// The result of a verification run.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub manifest: String,
    pub posture_digest: String,
    pub checks: Vec<Check>,
}

impl VerifyReport {
    /// Verification passes only when every check that ran passed **and at
    /// least one actually ran**.
    ///
    /// The second half matters: a run that waives both halves has verified
    /// nothing, and "nothing went wrong" is not the same claim as "this is the
    /// manifest we acknowledged, signed by our CI".
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.waived || c.passed) && self.checks.iter().any(|c| c.passed)
    }
}

/// Whether `provided` is an acceptable stand-in for `expected`.
///
/// Case-insensitive, hex-only, and never shorter than [`MIN_DIGEST_PREFIX`]:
/// a two-character "prefix" would match one digest in 256.
#[must_use]
pub fn digest_matches(expected: &str, provided: &str) -> bool {
    let provided = provided.trim().to_ascii_lowercase();
    if !(MIN_DIGEST_PREFIX..=64).contains(&provided.len())
        || !provided.chars().all(|c| c.is_ascii_hexdigit())
    {
        return false;
    }
    expected.trim().to_ascii_lowercase().starts_with(&provided)
}

/// How the signature half is performed. A trait so the orchestration above it
/// is testable without a network, a GitHub account, or `gh` on PATH.
pub trait SignatureVerifier {
    /// Verify `manifest` was attested by `repo`. `Ok(detail)` on success,
    /// `Err(detail)` with something a human can act on otherwise.
    fn verify(&self, manifest: &str, repo: &str) -> Result<String, String>;
}

/// The real thing: shells out to `gh attestation verify`.
pub struct GhAttestationVerifier;

/// The command `gh attestation verify` is invoked as, as a program plus args.
///
/// Split out so the exact invocation is asserted by a test rather than
/// discovered in production.
#[must_use]
pub fn gh_verify_command(manifest: &str, repo: &str) -> (String, Vec<String>) {
    (
        "gh".to_owned(),
        vec![
            "attestation".to_owned(),
            "verify".to_owned(),
            manifest.to_owned(),
            "--repo".to_owned(),
            repo.to_owned(),
        ],
    )
}

impl SignatureVerifier for GhAttestationVerifier {
    fn verify(&self, manifest: &str, repo: &str) -> Result<String, String> {
        let (program, args) = gh_verify_command(manifest, repo);
        let output = Command::new(&program).args(&args).output().map_err(|e| {
            format!(
                "could not run `gh attestation verify`: {e} — install GitHub CLI \u{2265} 2.49 \
                 (see docs/guide/supply-chain.md), or pass --skip-signature if this host is \
                 deliberately offline"
            )
        })?;
        if output.status.success() {
            Ok(format!("`gh attestation verify` accepted {manifest}"))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "`gh attestation verify` rejected {manifest}: {}",
                stderr.trim()
            ))
        }
    }
}

/// Run a verification with a caller-supplied signature verifier.
pub fn verify_with(
    opts: &VerifyOptions<'_>,
    verifier: &dyn SignatureVerifier,
) -> Result<VerifyReport, ManifestError> {
    let manifest = PostureManifest::read(opts.manifest)?;
    let posture_digest = manifest.posture_digest();
    let mut checks = Vec::new();

    checks.push(match opts.expect_digest {
        Some(expected) if digest_matches(&posture_digest, expected) => Check {
            name: "acknowledged-posture",
            passed: true,
            waived: false,
            detail: format!("matches the acknowledged digest {expected}"),
        },
        Some(expected) => Check {
            name: "acknowledged-posture",
            passed: false,
            waived: false,
            detail: format!(
                "does NOT match the acknowledged digest {expected} \u{2014} this manifest \
                 describes a different security posture than the one CI recorded"
            ),
        },
        None => Check {
            name: "acknowledged-posture",
            passed: false,
            waived: true,
            detail: "skipped: no --expect-digest given, so nothing ties this manifest to an \
                     acknowledged posture"
                .to_owned(),
        },
    });

    checks.push(if opts.skip_signature {
        Check {
            name: "signature",
            passed: false,
            waived: true,
            detail: "WAIVED by --skip-signature: provenance was NOT checked".to_owned(),
        }
    } else {
        opts.repo.map_or_else(
            || Check {
                name: "signature",
                passed: false,
                waived: false,
                detail: "cannot verify: pass --repo <owner/repo> (or --skip-signature on a \
                         deliberately offline host)"
                    .to_owned(),
            },
            |repo| {
                let (passed, detail) = match verifier.verify(opts.manifest, repo) {
                    Ok(detail) => (true, detail),
                    Err(detail) => (false, detail),
                };
                Check {
                    name: "signature",
                    passed,
                    waived: false,
                    detail,
                }
            },
        )
    });

    Ok(VerifyReport {
        manifest: opts.manifest.to_owned(),
        posture_digest,
        checks,
    })
}

/// Read the manifest at `path`, for callers that only want its digest.
pub fn manifest_digest(path: &str) -> Result<String, ManifestError> {
    Ok(PostureManifest::read(path)?.posture_digest())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A manifest with one gated route, written to a temp file.
    fn write_manifest(dir: &std::path::Path, classification: &str) -> String {
        let json = format!(
            r#"{{"schema_version":3,"dimensions":{{
                 "routes":{{"provenance":"provable","source":"m","entries":[
                   {{"path":"/admin","method":"GET","name":"a","classification":"{classification}",
                     "roles":["admin"],"scopes":[],"policy":false,"source":"user","provenance":"provable"}}]}},
                 "csrf":{{"provenance":"declared","source":"c","exempt_paths":[],"entries":[]}},
                 "security_headers":{{"provenance":"declared","source":"c","entries":[]}},
                 "authorization_policies":{{"provenance":"provable","source":"m","runtime_caveat":"x","entries":[]}}
               }},"excluded":[]}}"#
        );
        let path = dir.join(format!("{classification}.json"));
        std::fs::write(&path, json).unwrap();
        path.to_string_lossy().into_owned()
    }

    struct FakeVerifier {
        result: Result<String, String>,
        calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeVerifier {
        fn ok() -> Self {
            Self {
                result: Ok("signed".to_owned()),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn failing() -> Self {
            Self {
                result: Err("no attestations found".to_owned()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl SignatureVerifier for FakeVerifier {
        fn verify(&self, manifest: &str, repo: &str) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((manifest.to_owned(), repo.to_owned()));
            self.result.clone()
        }
    }

    // ── digest comparison ───────────────────────────────────────────────────

    #[test]
    fn a_full_digest_matches_itself_case_insensitively() {
        let d = "a".repeat(64);
        assert!(digest_matches(&d, &d));
        assert!(digest_matches(&d, &d.to_uppercase()));
    }

    #[test]
    fn a_long_enough_prefix_matches() {
        let d = "0123456789abcdef".to_owned() + &"f".repeat(48);
        assert!(digest_matches(&d, "0123456789abcdef"));
    }

    #[test]
    fn a_too_short_prefix_never_matches() {
        let d = "0123456789abcdef".to_owned() + &"f".repeat(48);
        assert!(!digest_matches(&d, "0123456789abcde"));
        assert!(!digest_matches(&d, "01"));
    }

    #[test]
    fn a_non_prefix_does_not_match() {
        let d = "0123456789abcdef".to_owned() + &"f".repeat(48);
        assert!(!digest_matches(&d, "fedcba9876543210"));
    }

    #[test]
    fn a_non_hex_string_never_matches() {
        let d = "0".repeat(64);
        assert!(!digest_matches(&d, "zzzzzzzzzzzzzzzz"));
    }

    // ── verification ────────────────────────────────────────────────────────

    #[test]
    fn a_genuine_manifest_with_the_acknowledged_digest_and_a_signature_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), "gated");
        let digest = manifest_digest(&path).unwrap();
        let verifier = FakeVerifier::ok();
        let report = verify_with(
            &VerifyOptions {
                manifest: &path,
                expect_digest: Some(&digest),
                repo: Some("acme/app"),
                skip_signature: false,
            },
            &verifier,
        )
        .unwrap();
        assert!(report.passed(), "{report:?}");
        assert_eq!(
            verifier.calls.borrow().as_slice(),
            &[(path, "acme/app".to_owned())]
        );
    }

    #[test]
    fn a_tampered_manifest_fails_the_digest_check() {
        let dir = tempfile::tempdir().unwrap();
        let genuine = write_manifest(dir.path(), "gated");
        let digest = manifest_digest(&genuine).unwrap();
        // Someone edits the shipped manifest to claim the route is public.
        let tampered = write_manifest(dir.path(), "public");
        let report = verify_with(
            &VerifyOptions {
                manifest: &tampered,
                expect_digest: Some(&digest),
                repo: Some("acme/app"),
                skip_signature: false,
            },
            &FakeVerifier::ok(),
        )
        .unwrap();
        assert!(!report.passed(), "a tampered manifest must not verify");
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "acknowledged-posture" && !c.passed)
        );
    }

    #[test]
    fn a_failed_signature_fails_the_run_even_when_the_digest_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), "gated");
        let digest = manifest_digest(&path).unwrap();
        let report = verify_with(
            &VerifyOptions {
                manifest: &path,
                expect_digest: Some(&digest),
                repo: Some("acme/app"),
                skip_signature: false,
            },
            &FakeVerifier::failing(),
        )
        .unwrap();
        assert!(!report.passed());
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "signature" && !c.passed)
        );
    }

    #[test]
    fn skipping_the_signature_is_recorded_as_waived_not_as_passed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), "gated");
        let digest = manifest_digest(&path).unwrap();
        let verifier = FakeVerifier::ok();
        let report = verify_with(
            &VerifyOptions {
                manifest: &path,
                expect_digest: Some(&digest),
                repo: None,
                skip_signature: true,
            },
            &verifier,
        )
        .unwrap();
        assert!(report.passed());
        let signature = report
            .checks
            .iter()
            .find(|c| c.name == "signature")
            .expect("the signature check is always reported, even when waived");
        assert!(signature.waived);
        assert!(!signature.passed);
        assert!(
            verifier.calls.borrow().is_empty(),
            "a waived check runs nothing"
        );
    }

    #[test]
    fn a_missing_repo_fails_the_signature_check_rather_than_skipping_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(dir.path(), "gated");
        let report = verify_with(
            &VerifyOptions {
                manifest: &path,
                expect_digest: None,
                repo: None,
                skip_signature: false,
            },
            &FakeVerifier::ok(),
        )
        .unwrap();
        assert!(
            !report.passed(),
            "no --repo and no --skip-signature must not silently pass"
        );
    }

    #[test]
    fn a_missing_manifest_is_an_error_not_a_pass() {
        let report = verify_with(
            &VerifyOptions {
                manifest: "/nonexistent/posture.json",
                expect_digest: None,
                repo: Some("acme/app"),
                skip_signature: true,
            },
            &FakeVerifier::ok(),
        );
        assert!(matches!(report, Err(ManifestError::Io { .. })));
    }

    #[test]
    fn the_gh_invocation_is_the_one_the_supply_chain_guide_documents() {
        let (program, args) = gh_verify_command("posture.json", "acme/app");
        assert_eq!(program, "gh");
        assert_eq!(
            args,
            vec![
                "attestation",
                "verify",
                "posture.json",
                "--repo",
                "acme/app"
            ]
        );
    }
}
