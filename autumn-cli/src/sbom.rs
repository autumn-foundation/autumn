//! `autumn sbom` — `CycloneDX` Software Bill of Materials generation and
//! verification (issue #1615).
//!
//! One implementation serves four surfaces:
//!
//! * `autumn sbom` — read `cargo metadata` for the project in the current
//!   directory and emit a `CycloneDX` 1.5 document. Used by the framework's
//!   publish gate and by the scaffolded production image's builder stage.
//! * `autumn sbom --verify <FILE>` — regenerate from the source tree and
//!   compare against `<FILE>`, reporting a component-level diff. This is what
//!   makes "the SBOM matches the tagged source" a real gate rather than a
//!   tautology.
//! * `autumn sbom --binary <FILE>` — decode the dependency list
//!   [`cargo-auditable`](https://github.com/rust-secure-code/cargo-auditable)
//!   embeds in a compiled binary, with no access to the source tree or
//!   lockfile. This is how a deployed single-binary autumn app answers
//!   "exactly which crate versions are compiled into you?".
//! * `--output <FILE>` — write anywhere instead of stdout.
//!
//! ## Determinism
//!
//! The document deliberately carries **no** `serialNumber` (a random UUID) and
//! **no** `metadata.timestamp` (a wall clock read). Both are optional in
//! `CycloneDX` and both would make `--verify` impossible. Components are sorted
//! by `(name, version, purl)` and de-duplicated by `bom-ref`, so the output is
//! byte-identical for a given source tree and CLI version regardless of the
//! order `cargo metadata` happens to emit packages in.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// The `CycloneDX` specification version emitted. 1.5 is the first version whose
/// `metadata.tools` is an object with a `components` array (rather than the
/// deprecated flat `tools` array).
const SPEC_VERSION: &str = "1.5";

/// Section names `cargo-auditable` uses for its embedded dependency list. ELF
/// uses `.dep-v0`; Mach-O section names are conventionally underscore-prefixed
/// and carry no leading dot, and the exact spelling has varied, so accept both
/// forms rather than silently reporting "not built with cargo-auditable" on a
/// binary that in fact was.
const DEP_SECTION_NAMES: &[&str] = &[".dep-v0", "dep-v0", "__dep_v0"];

#[derive(Debug, thiserror::Error)]
pub enum SbomError {
    #[error("cargo metadata failed: {0}")]
    CargoMetadata(String),
    #[error("malformed cargo metadata: {0}")]
    Metadata(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported object file format")]
    UnsupportedObjectFormat,
    #[error("truncated or malformed object file")]
    MalformedObject,
    #[error(
        "no embedded dependency list found: the binary was not built with cargo-auditable \
         (build it with `cargo auditable build --release`, or set \
         RUSTC_WORKSPACE_WRAPPER=cargo-auditable)"
    )]
    NoAuditData,
    #[error("SBOM does not match the source tree:\n{0}")]
    VerifyMismatch(String),
    #[error("SBOM describes version {found}, but {expected} is being released")]
    VersionMismatch { expected: String, found: String },
}

// ---------------------------------------------------------------------------
// CycloneDX document model
// ---------------------------------------------------------------------------

/// A `CycloneDX` 1.5 bill of materials.
///
/// Field order here IS the serialized key order, which is part of the
/// determinism contract — do not reorder without regenerating any checked-in
/// SBOM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// `bom_format` deliberately repeats the type name: it mirrors CycloneDX's own
// `bomFormat` key, and renaming the Rust field would only obscure that.
#[allow(clippy::struct_field_names)]
pub struct Bom {
    #[serde(rename = "bomFormat")]
    pub bom_format: String,
    #[serde(rename = "specVersion")]
    pub spec_version: String,
    pub version: u32,
    pub metadata: BomMetadata,
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BomMetadata {
    pub tools: Tools,
    pub component: Component,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tools {
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "bom-ref", skip_serializing_if = "Option::is_none", default)]
    pub bom_ref: Option<String>,
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub purl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub licenses: Option<Vec<License>>,
    #[serde(
        rename = "externalReferences",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub external_references: Option<Vec<ExternalReference>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub properties: Option<Vec<Property>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalReference {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Property {
    pub name: String,
    pub value: String,
}

impl Component {
    fn label(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    /// Sort key. `purl` participates so two packages that share a name and
    /// version but not an identity still order deterministically.
    fn sort_key(&self) -> (&str, &str, &str) {
        (
            self.name.as_str(),
            self.version.as_str(),
            self.purl.as_deref().unwrap_or(""),
        )
    }
}

fn purl(name: &str, version: &str) -> String {
    format!("pkg:cargo/{name}@{version}")
}

fn tool_component(tool_version: &str) -> Component {
    Component {
        kind: "application".into(),
        bom_ref: None,
        name: "autumn-cli".into(),
        version: tool_version.to_owned(),
        purl: None,
        licenses: None,
        external_references: None,
        properties: None,
    }
}

/// Assemble the envelope, sorting and de-duplicating `components` so callers
/// never have to remember to.
fn assemble(root: Component, mut components: Vec<Component>, tool_version: &str) -> Bom {
    components.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    components.dedup_by(|a, b| a.bom_ref.is_some() && a.bom_ref == b.bom_ref);
    Bom {
        bom_format: "CycloneDX".into(),
        spec_version: SPEC_VERSION.into(),
        version: 1,
        metadata: BomMetadata {
            tools: Tools {
                components: vec![tool_component(tool_version)],
            },
            component: root,
        },
        components,
    }
}

/// Identity to fall back on when `cargo metadata` reports no resolve root —
/// which is what a virtual workspace manifest (like autumn's own) produces.
#[derive(Debug, Clone)]
pub struct RootFallback {
    pub name: String,
    pub version: String,
}

// ---------------------------------------------------------------------------
// cargo metadata -> CycloneDX
// ---------------------------------------------------------------------------

fn str_field<'a>(pkg: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    pkg.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn component_from_package(pkg: &serde_json::Value) -> Result<Component, SbomError> {
    let name = str_field(pkg, "name")
        .ok_or_else(|| SbomError::Metadata("a package has no `name`".into()))?;
    let version = str_field(pkg, "version")
        .ok_or_else(|| SbomError::Metadata(format!("package `{name}` has no `version`")))?;
    let p = purl(name, version);
    Ok(Component {
        kind: "library".into(),
        bom_ref: Some(p.clone()),
        name: name.to_owned(),
        version: version.to_owned(),
        purl: Some(p),
        licenses: str_field(pkg, "license").map(|l| {
            vec![License {
                expression: l.to_owned(),
            }]
        }),
        external_references: str_field(pkg, "repository").map(|url| {
            vec![ExternalReference {
                kind: "vcs".into(),
                url: url.to_owned(),
            }]
        }),
        properties: None,
    })
}

/// Build a `CycloneDX` document from `cargo metadata --format-version 1` output.
///
/// The top-level component is `resolve.root` when cargo resolved one (the
/// common case for a scaffolded app, where the command runs inside the single
/// member crate). A virtual workspace manifest has no root, so `fallback`
/// supplies the identity — for autumn itself that is the repository name plus
/// `[workspace.package].version`, which is exactly what the release tag
/// encodes.
///
/// # Errors
///
/// Returns [`SbomError::Metadata`] if `packages` is absent or any package is
/// missing `name`/`version`.
pub fn bom_from_cargo_metadata(
    metadata: &serde_json::Value,
    fallback: &RootFallback,
    tool_version: &str,
) -> Result<Bom, SbomError> {
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| SbomError::Metadata("no `packages` array".into()))?;

    let root_id = metadata
        .get("resolve")
        .and_then(|r| r.get("root"))
        .and_then(serde_json::Value::as_str);

    let root_pkg = root_id.and_then(|id| {
        packages
            .iter()
            .find(|p| p.get("id").and_then(serde_json::Value::as_str) == Some(id))
    });

    let root = match root_pkg {
        Some(pkg) => Component {
            kind: "application".into(),
            ..component_from_package(pkg)?
        },
        None => Component {
            kind: "application".into(),
            bom_ref: Some(purl(&fallback.name, &fallback.version)),
            name: fallback.name.clone(),
            version: fallback.version.clone(),
            purl: Some(purl(&fallback.name, &fallback.version)),
            licenses: None,
            external_references: None,
            properties: None,
        },
    };

    let components = packages
        .iter()
        .filter(|p| root_id.is_none() || p.get("id").and_then(serde_json::Value::as_str) != root_id)
        .map(component_from_package)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(assemble(root, components, tool_version))
}

/// Serialize `bom` deterministically: pretty-printed JSON with a trailing
/// newline, so the file is diffable and POSIX-clean.
///
/// # Errors
///
/// Returns [`SbomError::Json`] if serialization fails.
pub fn render(bom: &Bom) -> Result<String, SbomError> {
    let mut s = serde_json::to_string_pretty(bom)?;
    s.push('\n');
    Ok(s)
}

// ---------------------------------------------------------------------------
// verification
// ---------------------------------------------------------------------------

/// Compare a freshly generated `expected` BOM against the `actual` one read
/// from disk, returning one human-readable line per difference (empty when
/// they agree).
///
/// Deliberately component-level rather than a byte diff: a gate failure has to
/// tell a release engineer *which* dependency drifted, not that line 4,812
/// differs.
#[must_use]
pub fn diff(expected: &Bom, actual: &Bom) -> Vec<String> {
    let mut report = Vec::new();

    if expected.spec_version != actual.spec_version {
        report.push(format!(
            "  spec version: expected CycloneDX {}, found {}",
            expected.spec_version, actual.spec_version
        ));
    }
    let (e, a) = (&expected.metadata.component, &actual.metadata.component);
    if e.name != a.name || e.version != a.version {
        report.push(format!(
            "  root component: expected {}, found {}",
            e.label(),
            a.label()
        ));
    }

    let index = |bom: &Bom| -> BTreeMap<String, Component> {
        bom.components
            .iter()
            .map(|c| (c.bom_ref.clone().unwrap_or_else(|| c.label()), c.clone()))
            .collect()
    };
    let (ei, ai) = (index(expected), index(actual));

    for (key, comp) in &ei {
        match ai.get(key) {
            None => report.push(format!("  missing component: {}", comp.label())),
            Some(found) if found != comp => report.push(format!(
                "  component differs: {} (licenses/metadata changed)",
                comp.label()
            )),
            Some(_) => {}
        }
    }
    for (key, comp) in &ai {
        if !ei.contains_key(key) {
            report.push(format!("  unexpected component: {}", comp.label()));
        }
    }

    // Indexing by `bom-ref` collapses duplicates, so an SBOM listing the same
    // component twice would otherwise compare equal to a clean one. Report the
    // raw length disagreement, but only when nothing above already explains it.
    if report.is_empty() && expected.components.len() != actual.components.len() {
        report.push(format!(
            "  component count: expected {}, found {} (duplicate entries?)",
            expected.components.len(),
            actual.components.len()
        ));
    }
    report
}

/// Assert that `bom`'s root component describes `expected`.
///
/// The release gate needs this: an SBOM that verifies perfectly against *a*
/// source tree still fails the AC if that tree is not the tagged one. Kept
/// here, beside the document model, rather than as shell that re-parses the
/// JSON — the gate script has no business owning a second `CycloneDX` parser.
///
/// # Errors
///
/// Returns [`SbomError::VersionMismatch`] when the versions differ.
pub fn check_expected_version(bom: &Bom, expected: &str) -> Result<(), SbomError> {
    if bom.metadata.component.version == expected {
        Ok(())
    } else {
        Err(SbomError::VersionMismatch {
            expected: expected.to_owned(),
            found: bom.metadata.component.version.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// cargo-auditable payload -> CycloneDX
// ---------------------------------------------------------------------------

/// One entry of `cargo-auditable`'s embedded `VersionInfo` payload.
#[derive(Debug, Deserialize)]
struct AuditPackage {
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
    /// `runtime` when absent — that is the format's own default.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    root: bool,
}

#[derive(Debug, Deserialize)]
struct AuditData {
    packages: Vec<AuditPackage>,
}

fn component_from_audit(pkg: &AuditPackage) -> Component {
    let p = purl(&pkg.name, &pkg.version);
    let kind = pkg.kind.as_deref().unwrap_or("runtime");
    let mut properties = Vec::new();
    // Only annotate the non-default kind, so a runtime-only BOM stays clean.
    if kind != "runtime" {
        properties.push(Property {
            name: "cargo:dependency-kind".into(),
            value: kind.to_owned(),
        });
    }
    if let Some(source) = pkg.source.as_deref().filter(|s| !s.is_empty()) {
        properties.push(Property {
            name: "cargo:source".into(),
            value: source.to_owned(),
        });
    }
    Component {
        kind: "library".into(),
        bom_ref: Some(p.clone()),
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        purl: Some(p),
        licenses: None,
        external_references: None,
        properties: (!properties.is_empty()).then_some(properties),
    }
}

/// Build a `CycloneDX` document from the JSON `cargo-auditable` embeds.
///
/// # Errors
///
/// Returns [`SbomError::Json`] when `json` is not a valid audit payload.
pub fn bom_from_audit_data(json: &str, tool_version: &str) -> Result<Bom, SbomError> {
    let data: AuditData = serde_json::from_str(json)?;
    let root_pkg = data.packages.iter().find(|p| p.root);
    let root = root_pkg.map_or_else(
        || Component {
            kind: "application".into(),
            bom_ref: None,
            name: "unknown".into(),
            version: "0.0.0".into(),
            purl: None,
            licenses: None,
            external_references: None,
            properties: None,
        },
        |p| Component {
            kind: "application".into(),
            properties: None,
            ..component_from_audit(p)
        },
    );
    let components = data
        .packages
        .iter()
        .filter(|p| !p.root)
        .map(component_from_audit)
        .collect();
    Ok(assemble(root, components, tool_version))
}

/// Decode a compiled binary's embedded dependency list into a `CycloneDX`
/// document — the "no source tree, no lockfile" path.
///
/// # Errors
///
/// Propagates [`SbomError::UnsupportedObjectFormat`], [`SbomError::NoAuditData`]
/// and JSON/decompression failures.
pub fn bom_from_binary(bytes: &[u8], tool_version: &str) -> Result<Bom, SbomError> {
    let section = extract_dep_section(bytes)?;
    let json = inflate_audit_data(section)?;
    bom_from_audit_data(&json, tool_version)
}

// ---------------------------------------------------------------------------
// object-file section extraction
// ---------------------------------------------------------------------------

/// Endian-aware little/big integer reads over a byte slice, bounds-checked so
/// a truncated or hostile file yields [`SbomError::MalformedObject`] rather
/// than a panic.
struct Reader<'a> {
    bytes: &'a [u8],
    big_endian: bool,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8], big_endian: bool) -> Self {
        Self { bytes, big_endian }
    }

    fn slice(&self, off: usize, len: usize) -> Result<&'a [u8], SbomError> {
        self.bytes
            .get(off..off.checked_add(len).ok_or(SbomError::MalformedObject)?)
            .ok_or(SbomError::MalformedObject)
    }

    fn u16(&self, off: usize) -> Result<u16, SbomError> {
        let b: [u8; 2] = self
            .slice(off, 2)?
            .try_into()
            .map_err(|_| SbomError::MalformedObject)?;
        Ok(if self.big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    }

    fn u32(&self, off: usize) -> Result<u32, SbomError> {
        let b: [u8; 4] = self
            .slice(off, 4)?
            .try_into()
            .map_err(|_| SbomError::MalformedObject)?;
        Ok(if self.big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }

    fn u64(&self, off: usize) -> Result<u64, SbomError> {
        let b: [u8; 8] = self
            .slice(off, 8)?
            .try_into()
            .map_err(|_| SbomError::MalformedObject)?;
        Ok(if self.big_endian {
            u64::from_be_bytes(b)
        } else {
            u64::from_le_bytes(b)
        })
    }

    /// A pointer-width integer: 8 bytes for a 64-bit object, 4 for a 32-bit one.
    fn usize_at(&self, off: usize, class64: bool) -> Result<u64, SbomError> {
        if class64 {
            self.u64(off)
        } else {
            Ok(u64::from(self.u32(off)?))
        }
    }
}

fn is_dep_section(name: &str) -> bool {
    DEP_SECTION_NAMES.contains(&name)
}

/// Read a NUL-terminated name out of a string table.
fn cstr_at(table: &[u8], off: usize) -> Result<&str, SbomError> {
    let rest = table.get(off..).ok_or(SbomError::MalformedObject)?;
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).map_err(|_| SbomError::MalformedObject)
}

/// A fixed-size, NUL-padded name field (Mach-O sections, PE sections).
fn fixed_name(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

fn extract_from_elf(bytes: &[u8]) -> Result<&[u8], SbomError> {
    let class64 = match bytes.get(4) {
        Some(1) => false,
        Some(2) => true,
        _ => return Err(SbomError::MalformedObject),
    };
    let big_endian = match bytes.get(5) {
        Some(1) => false,
        Some(2) => true,
        _ => return Err(SbomError::MalformedObject),
    };
    let r = Reader::new(bytes, big_endian);

    // e_shoff / e_shentsize / e_shnum / e_shstrndx live at different offsets
    // in the 32- and 64-bit headers.
    let (shoff_at, shentsize_at, shnum_at, shstrndx_at) = if class64 {
        (0x28, 0x3a, 0x3c, 0x3e)
    } else {
        (0x20, 0x2e, 0x30, 0x32)
    };

    let shoff =
        usize::try_from(r.usize_at(shoff_at, class64)?).map_err(|_| SbomError::MalformedObject)?;
    if shoff == 0 {
        return Err(SbomError::NoAuditData);
    }
    let shentsize = r.u16(shentsize_at)? as usize;
    if shentsize == 0 {
        return Err(SbomError::MalformedObject);
    }
    let mut shnum = r.u16(shnum_at)? as usize;
    let mut shstrndx = r.u16(shstrndx_at)? as usize;

    // Extended numbering: with more than 0xff00 sections (or shstrndx past
    // SHN_LORESERVE) the real values live in the otherwise-unused section
    // header 0. Rare, but a large release binary can hit it.
    let sh = |i: usize| -> Result<usize, SbomError> {
        shoff
            .checked_add(i.checked_mul(shentsize).ok_or(SbomError::MalformedObject)?)
            .ok_or(SbomError::MalformedObject)
    };
    if shnum == 0 || shstrndx == 0xffff {
        let zero = sh(0)?;
        let (size_at, link_at) = if class64 {
            (zero + 32, zero + 40)
        } else {
            (zero + 20, zero + 24)
        };
        if shnum == 0 {
            shnum = usize::try_from(r.usize_at(size_at, class64)?)
                .map_err(|_| SbomError::MalformedObject)?;
        }
        if shstrndx == 0xffff {
            shstrndx = r.u32(link_at)? as usize;
        }
    }
    if shstrndx >= shnum {
        return Err(SbomError::MalformedObject);
    }

    // Section header layout: sh_name u32, sh_type u32, then pointer-width
    // sh_flags / sh_addr / sh_offset / sh_size.
    let (offset_at, size_at) = if class64 { (24, 32) } else { (16, 20) };
    let read_hdr = |i: usize| -> Result<(u32, usize, usize), SbomError> {
        let base = sh(i)?;
        // Reject a header that runs past EOF before trusting any of its fields.
        r.slice(base, shentsize)?;
        let name = r.u32(base)?;
        let off = usize::try_from(r.usize_at(base + offset_at, class64)?)
            .map_err(|_| SbomError::MalformedObject)?;
        let size = usize::try_from(r.usize_at(base + size_at, class64)?)
            .map_err(|_| SbomError::MalformedObject)?;
        Ok((name, off, size))
    };

    let (_, strtab_off, strtab_size) = read_hdr(shstrndx)?;
    let strtab = r.slice(strtab_off, strtab_size)?;

    for i in 0..shnum {
        let (name_off, off, size) = read_hdr(i)?;
        if is_dep_section(cstr_at(strtab, name_off as usize)?) {
            return r.slice(off, size);
        }
    }
    Err(SbomError::NoAuditData)
}

fn extract_from_macho_thin(bytes: &[u8], base: usize) -> Result<&[u8], SbomError> {
    // MH_MAGIC(_64) is 0xfeedface / 0xfeedfacf as a NUMBER, so a little-endian
    // image starts with those bytes reversed. Derive both the word size and the
    // byte order from the raw bytes rather than assuming little-endian and
    // rejecting the swapped form as "unsupported".
    let (class64, big_endian) = match Reader::new(bytes, false).slice(base, 4)? {
        [0xcf, 0xfa, 0xed, 0xfe] => (true, false),
        [0xce, 0xfa, 0xed, 0xfe] => (false, false),
        [0xfe, 0xed, 0xfa, 0xcf] => (true, true),
        [0xfe, 0xed, 0xfa, 0xce] => (false, true),
        _ => return Err(SbomError::UnsupportedObjectFormat),
    };
    let r = Reader::new(bytes, big_endian);
    let header_len = if class64 { 32 } else { 28 };
    let ncmds = r.u32(base + 16)? as usize;

    let mut cmd_off = base + header_len;
    for _ in 0..ncmds {
        let cmd = r.u32(cmd_off)?;
        let cmdsize = r.u32(cmd_off + 4)? as usize;
        if cmdsize < 8 {
            return Err(SbomError::MalformedObject);
        }
        // LC_SEGMENT (0x1) / LC_SEGMENT_64 (0x19)
        let seg64 = cmd == 0x19;
        if seg64 || cmd == 0x1 {
            let (seg_len, sect_len, nsects_at) = if seg64 {
                (72usize, 80usize, 64usize)
            } else {
                (56, 68, 48)
            };
            let nsects = r.u32(cmd_off + nsects_at)? as usize;
            for s in 0..nsects {
                let sect = cmd_off
                    .checked_add(seg_len + s * sect_len)
                    .ok_or(SbomError::MalformedObject)?;
                let name = fixed_name(r.slice(sect, 16)?);
                if is_dep_section(name) {
                    // sectname[16] segname[16] addr size offset ...
                    let (off_at, size_at) = if seg64 {
                        (48usize, 40usize)
                    } else {
                        (40usize, 36usize)
                    };
                    let size = usize::try_from(r.usize_at(sect + size_at, seg64)?)
                        .map_err(|_| SbomError::MalformedObject)?;
                    let off = r.u32(sect + off_at)? as usize;
                    return r.slice(base + off, size);
                }
            }
        }
        cmd_off = cmd_off
            .checked_add(cmdsize)
            .ok_or(SbomError::MalformedObject)?;
    }
    Err(SbomError::NoAuditData)
}

fn extract_from_macho_fat(bytes: &[u8], fat64: bool) -> Result<&[u8], SbomError> {
    // Fat headers are always big-endian regardless of the slices inside.
    let r = Reader::new(bytes, true);
    let narch = r.u32(4)? as usize;
    let entry = if fat64 { 32usize } else { 20 };
    let mut last = Err(SbomError::NoAuditData);
    for i in 0..narch {
        let base = 8 + i * entry;
        let off = if fat64 {
            usize::try_from(r.u64(base + 8)?).map_err(|_| SbomError::MalformedObject)?
        } else {
            r.u32(base + 8)? as usize
        };
        match extract_from_macho_thin(bytes, off) {
            Ok(found) => return Ok(found),
            Err(e) => last = Err(e),
        }
    }
    last
}

fn extract_from_pe(bytes: &[u8]) -> Result<&[u8], SbomError> {
    let r = Reader::new(bytes, false);
    let pe_off = r.u32(0x3c)? as usize;
    if r.slice(pe_off, 4)? != b"PE\0\0" {
        return Err(SbomError::UnsupportedObjectFormat);
    }
    let coff = pe_off + 4;
    let nsections = r.u16(coff + 2)? as usize;
    let opt_len = r.u16(coff + 16)? as usize;
    let table = coff + 20 + opt_len;
    for i in 0..nsections {
        let sect = table
            .checked_add(i.checked_mul(40).ok_or(SbomError::MalformedObject)?)
            .ok_or(SbomError::MalformedObject)?;
        let name = fixed_name(r.slice(sect, 8)?);
        if is_dep_section(name) {
            let size = r.u32(sect + 16)? as usize;
            let off = r.u32(sect + 20)? as usize;
            return r.slice(off, size);
        }
    }
    Err(SbomError::NoAuditData)
}

/// Locate `cargo-auditable`'s embedded dependency section in an object file.
///
/// Handles ELF (32/64-bit, either endianness), Mach-O (thin and fat/universal)
/// and PE — i.e. every target the CLI itself ships binaries for.
///
/// # Errors
///
/// * [`SbomError::UnsupportedObjectFormat`] — not a recognized object file.
/// * [`SbomError::NoAuditData`] — a valid object file with no embedded list.
/// * [`SbomError::MalformedObject`] — truncated or self-inconsistent headers.
pub fn extract_dep_section(bytes: &[u8]) -> Result<&[u8], SbomError> {
    match bytes.get(..4) {
        Some(b"\x7fELF") => extract_from_elf(bytes),
        Some([0xfe, 0xed, 0xfa, 0xcf | 0xce] | [0xcf | 0xce, 0xfa, 0xed, 0xfe]) => {
            extract_from_macho_thin(bytes, 0)
        }
        Some([0xca, 0xfe, 0xba, 0xbe]) => extract_from_macho_fat(bytes, false),
        Some([0xca, 0xfe, 0xba, 0xbf]) => extract_from_macho_fat(bytes, true),
        Some([b'M', b'Z', ..]) => extract_from_pe(bytes),
        _ => Err(SbomError::UnsupportedObjectFormat),
    }
}

/// Inflate the zlib-compressed JSON `cargo-auditable` stores in the section.
///
/// # Errors
///
/// Returns [`SbomError::Io`] when the payload is not valid zlib or not UTF-8.
pub fn inflate_audit_data(compressed: &[u8]) -> Result<String, SbomError> {
    let mut out = String::new();
    flate2::read::ZlibDecoder::new(compressed).read_to_string(&mut out)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// CLI entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SbomOptions {
    /// Path to a `Cargo.toml` to describe (defaults to the current directory).
    pub manifest_path: Option<PathBuf>,
    /// Write the document here instead of stdout.
    pub output: Option<PathBuf>,
    /// Regenerate and compare against this file instead of emitting one.
    pub verify: Option<PathBuf>,
    /// Read the embedded dependency list out of this compiled binary.
    pub binary: Option<PathBuf>,
    /// Pass `--locked` to `cargo metadata` (the release gate does; the
    /// scaffold's image build deliberately does not, so an app with a stale
    /// lockfile still builds).
    pub locked: bool,
    /// Require the SBOM's root component to describe exactly this version.
    pub expect_version: Option<String>,
}

fn run_cargo_metadata(opts: &SbomOptions) -> Result<serde_json::Value, SbomError> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(["metadata", "--format-version", "1", "--all-features"]);
    if opts.locked {
        cmd.arg("--locked");
    }
    if let Some(path) = &opts.manifest_path {
        cmd.arg("--manifest-path").arg(path);
    }
    let out = cmd
        .output()
        .map_err(|e| SbomError::CargoMetadata(format!("could not run cargo: {e}")))?;
    if !out.status.success() {
        return Err(SbomError::CargoMetadata(
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Derive the fallback root identity for a virtual workspace: the workspace
/// directory's name plus `[workspace.package].version` (falling back to
/// `[package].version`) from its root manifest.
fn workspace_fallback(metadata: &serde_json::Value) -> RootFallback {
    let root_dir = metadata
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let manifest = root_dir
        .as_ref()
        .and_then(|d| std::fs::read_to_string(d.join("Cargo.toml")).ok())
        // `toml::from_str`, not `str::parse`: since toml 1.0 the `FromStr`
        // impl parses a bare TOML *value*, not a document, and silently fails
        // on the very first table header.
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok());

    // Prefer the repository URL's last path segment over the checkout
    // directory's name: cloning into `autumn-fork/` must not silently change
    // the identity the release SBOM claims.
    let name = manifest
        .as_ref()
        .and_then(|t| {
            t.get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("repository"))
                .or_else(|| t.get("package").and_then(|p| p.get("repository")))
                .and_then(toml::Value::as_str)
        })
        .and_then(|url| {
            url.trim_end_matches('/')
                .trim_end_matches(".git")
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            root_dir
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "workspace".to_owned());

    let version = manifest
        .as_ref()
        .and_then(|t| {
            t.get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("version"))
                .or_else(|| t.get("package").and_then(|p| p.get("version")))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "0.0.0".to_owned());
    RootFallback { name, version }
}

fn generate(opts: &SbomOptions) -> Result<Bom, SbomError> {
    let tool_version = env!("CARGO_PKG_VERSION");
    if let Some(path) = &opts.binary {
        let bytes = std::fs::read(path)?;
        return bom_from_binary(&bytes, tool_version);
    }
    let metadata = run_cargo_metadata(opts)?;
    let fallback = workspace_fallback(&metadata);
    bom_from_cargo_metadata(&metadata, &fallback, tool_version)
}

fn execute(opts: &SbomOptions) -> Result<(), SbomError> {
    let bom = generate(opts)?;

    // Checked against the FRESHLY GENERATED document, before any comparison:
    // if the source tree is not the version being released, the SBOM under
    // test cannot be right even if it matches that tree byte for byte.
    if let Some(expected) = &opts.expect_version {
        check_expected_version(&bom, expected)?;
    }

    if let Some(path) = &opts.verify {
        let existing = std::fs::read_to_string(path)?;
        let actual: Bom = serde_json::from_str(&existing).map_err(|e| {
            SbomError::VerifyMismatch(format!(
                "  {} is not a CycloneDX document this CLI can read: {e}",
                path.display()
            ))
        })?;
        let report = diff(&bom, &actual);
        if !report.is_empty() {
            return Err(SbomError::VerifyMismatch(report.join("\n")));
        }
        eprintln!(
            "SBOM {} matches the source tree ({} components).",
            path.display(),
            bom.components.len()
        );
        return Ok(());
    }

    let rendered = render(&bom)?;
    match &opts.output {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, &rendered)?;
            eprintln!(
                "Wrote CycloneDX SBOM ({} components) to {}",
                bom.components.len(),
                path.display()
            );
        }
        None => print!("{rendered}"),
    }
    Ok(())
}

/// Run `autumn sbom`, exiting non-zero on any failure.
pub fn run(opts: &SbomOptions) {
    if let Err(e) = execute(opts) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fallback() -> RootFallback {
        RootFallback {
            name: "autumn".into(),
            version: "0.7.0".into(),
        }
    }

    fn metadata_with(packages: &serde_json::Value, root: &serde_json::Value) -> serde_json::Value {
        json!({
            "packages": packages,
            "workspace_root": "/tmp/ws",
            "resolve": { "root": root },
        })
    }

    fn pkg(name: &str, version: &str) -> serde_json::Value {
        json!({
            "id": format!("registry+https://github.com/rust-lang/crates.io-index#{name}@{version}"),
            "name": name,
            "version": version,
            "license": "MIT OR Apache-2.0",
            "repository": format!("https://example.invalid/{name}"),
        })
    }

    // ---------------------------------------------------------------------
    // cargo metadata -> CycloneDX
    // ---------------------------------------------------------------------

    #[test]
    fn emits_a_cyclonedx_envelope() {
        let md = metadata_with(&json!([pkg("serde", "1.0.0")]), &json!(null));
        let bom = bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();

        assert_eq!(v["bomFormat"], "CycloneDX");
        assert_eq!(v["specVersion"], "1.5");
        assert_eq!(v["version"], 1);
        assert_eq!(
            v["metadata"]["tools"]["components"][0]["name"],
            "autumn-cli"
        );
        assert_eq!(v["metadata"]["tools"]["components"][0]["version"], "0.7.0");
    }

    #[test]
    fn omits_nondeterministic_fields() {
        let md = metadata_with(&json!([pkg("serde", "1.0.0")]), &json!(null));
        let rendered =
            render(&bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap()).unwrap();
        // A UUID serial number or a wall-clock timestamp would make the
        // publish gate's `--verify` comparison impossible.
        assert!(
            !rendered.contains("serialNumber"),
            "SBOM must not carry a random serialNumber: {rendered}"
        );
        assert!(
            !rendered.contains("timestamp"),
            "SBOM must not carry a wall-clock timestamp: {rendered}"
        );
    }

    #[test]
    fn falls_back_to_the_workspace_identity_when_there_is_no_root_package() {
        let md = metadata_with(&json!([pkg("serde", "1.0.0")]), &json!(null));
        let bom = bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();

        assert_eq!(v["metadata"]["component"]["name"], "autumn");
        assert_eq!(v["metadata"]["component"]["version"], "0.7.0");
        assert_eq!(v["metadata"]["component"]["type"], "application");
        assert_eq!(v["metadata"]["component"]["purl"], "pkg:cargo/autumn@0.7.0");
    }

    #[test]
    fn uses_the_resolved_root_package_as_the_top_level_component() {
        let root_id = "path+file:///tmp/ws#my-app@1.2.3";
        let md = metadata_with(
            &json!([
                {
                    "id": root_id,
                    "name": "my-app",
                    "version": "1.2.3",
                    "license": "MIT",
                    "repository": serde_json::Value::Null,
                },
                pkg("serde", "1.0.0"),
            ]),
            &json!(root_id),
        );
        let bom = bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();

        assert_eq!(v["metadata"]["component"]["name"], "my-app");
        assert_eq!(v["metadata"]["component"]["version"], "1.2.3");
        // The root must not be duplicated into the flat component list.
        let names: Vec<&str> = v["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["serde"]);
    }

    #[test]
    fn records_purl_license_and_vcs_for_each_component() {
        let md = metadata_with(&json!([pkg("serde", "1.0.0")]), &json!(null));
        let bom = bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();
        let c = &v["components"][0];

        assert_eq!(c["type"], "library");
        assert_eq!(c["name"], "serde");
        assert_eq!(c["version"], "1.0.0");
        assert_eq!(c["purl"], "pkg:cargo/serde@1.0.0");
        assert_eq!(c["bom-ref"], "pkg:cargo/serde@1.0.0");
        assert_eq!(c["licenses"][0]["expression"], "MIT OR Apache-2.0");
        assert_eq!(c["externalReferences"][0]["type"], "vcs");
        assert_eq!(
            c["externalReferences"][0]["url"],
            "https://example.invalid/serde"
        );
    }

    #[test]
    fn omits_license_and_vcs_when_the_package_declares_none() {
        let md = metadata_with(
            &json!([{ "id": "x#anon@0.1.0", "name": "anon", "version": "0.1.0" }]),
            &json!(null),
        );
        let bom = bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();

        assert!(v["components"][0].get("licenses").is_none());
        assert!(v["components"][0].get("externalReferences").is_none());
    }

    #[test]
    fn output_is_byte_identical_regardless_of_cargo_metadata_ordering() {
        let a = metadata_with(
            &json!([
                pkg("serde", "1.0.0"),
                pkg("anyhow", "1.0.5"),
                pkg("serde", "0.9.0")
            ]),
            &json!(null),
        );
        let b = metadata_with(
            &json!([
                pkg("serde", "0.9.0"),
                pkg("serde", "1.0.0"),
                pkg("anyhow", "1.0.5")
            ]),
            &json!(null),
        );
        let ra = render(&bom_from_cargo_metadata(&a, &fallback(), "0.7.0").unwrap()).unwrap();
        let rb = render(&bom_from_cargo_metadata(&b, &fallback(), "0.7.0").unwrap()).unwrap();
        assert_eq!(ra, rb);

        let v: serde_json::Value = serde_json::from_str(&ra).unwrap();
        let ordered: Vec<String> = v["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                format!(
                    "{}@{}",
                    c["name"].as_str().unwrap(),
                    c["version"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(ordered, vec!["anyhow@1.0.5", "serde@0.9.0", "serde@1.0.0"]);
    }

    #[test]
    fn deduplicates_components_that_share_a_bom_ref() {
        let md = metadata_with(
            &json!([pkg("serde", "1.0.0"), pkg("serde", "1.0.0")]),
            &json!(null),
        );
        let bom = bom_from_cargo_metadata(&md, &fallback(), "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();
        assert_eq!(v["components"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn rejects_metadata_without_a_packages_array() {
        let md = json!({ "workspace_root": "/tmp/ws" });
        assert!(matches!(
            bom_from_cargo_metadata(&md, &fallback(), "0.7.0"),
            Err(SbomError::Metadata(_))
        ));
    }

    // ---------------------------------------------------------------------
    // verification diff
    // ---------------------------------------------------------------------

    fn bom_of(pkgs: &[(&str, &str)]) -> Bom {
        let packages: Vec<serde_json::Value> = pkgs.iter().map(|(n, v)| pkg(n, v)).collect();
        bom_from_cargo_metadata(
            &metadata_with(&serde_json::Value::Array(packages), &json!(null)),
            &fallback(),
            "0.7.0",
        )
        .unwrap()
    }

    #[test]
    fn identical_boms_diff_to_nothing() {
        let a = bom_of(&[("serde", "1.0.0"), ("anyhow", "1.0.5")]);
        let b = bom_of(&[("anyhow", "1.0.5"), ("serde", "1.0.0")]);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_names_the_missing_component() {
        let expected = bom_of(&[("serde", "1.0.0"), ("anyhow", "1.0.5")]);
        let actual = bom_of(&[("serde", "1.0.0")]);
        let report = diff(&expected, &actual).join("\n");
        assert!(report.contains("anyhow@1.0.5"), "{report}");
        assert!(report.contains("missing"), "{report}");
    }

    #[test]
    fn diff_names_the_unexpected_component() {
        let expected = bom_of(&[("serde", "1.0.0")]);
        let actual = bom_of(&[("serde", "1.0.0"), ("evil", "6.6.6")]);
        let report = diff(&expected, &actual).join("\n");
        assert!(report.contains("evil@6.6.6"), "{report}");
        assert!(report.contains("unexpected"), "{report}");
    }

    #[test]
    fn diff_reports_a_changed_root_component() {
        let expected = bom_of(&[("serde", "1.0.0")]);
        let mut actual = bom_of(&[("serde", "1.0.0")]);
        actual.metadata.component.version = "9.9.9".into();
        let report = diff(&expected, &actual).join("\n");
        assert!(report.contains("root component"), "{report}");
        assert!(report.contains("9.9.9"), "{report}");
    }

    #[test]
    fn workspace_fallback_reads_the_virtual_manifest_version() {
        // Regression: `str::parse::<toml::Value>()` parses a bare TOML value,
        // not a document, so it fails on the first table header and every
        // virtual-workspace SBOM silently claimed version 0.0.0.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"1.2.3\"\n",
        )
        .unwrap();
        let md = json!({ "workspace_root": dir.path().to_str().unwrap() });
        let fb = workspace_fallback(&md);
        assert_eq!(fb.version, "1.2.3");
        assert_eq!(fb.name, dir.path().file_name().unwrap().to_str().unwrap());
    }

    #[test]
    fn workspace_fallback_reads_a_plain_package_version() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"solo\"\nversion = \"4.5.6\"\n",
        )
        .unwrap();
        let md = json!({ "workspace_root": dir.path().to_str().unwrap() });
        assert_eq!(workspace_fallback(&md).version, "4.5.6");
    }

    #[test]
    fn workspace_fallback_prefers_the_repository_name_over_the_checkout_directory() {
        // A release SBOM's identity must not depend on what the CI runner
        // happened to name the clone directory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace.package]\nversion = \"1.2.3\"\n\
             repository = \"https://github.com/autumn-foundation/autumn\"\n",
        )
        .unwrap();
        let md = json!({ "workspace_root": dir.path().to_str().unwrap() });
        let fb = workspace_fallback(&md);
        assert_eq!(fb.name, "autumn");
        assert_ne!(fb.name, dir.path().file_name().unwrap().to_str().unwrap());
    }

    #[test]
    fn workspace_fallback_tolerates_a_trailing_slash_or_dot_git() {
        for url in [
            "https://github.com/autumn-foundation/autumn/",
            "https://github.com/autumn-foundation/autumn.git",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("Cargo.toml"),
                format!("[workspace.package]\nversion = \"1.2.3\"\nrepository = \"{url}\"\n"),
            )
            .unwrap();
            let md = json!({ "workspace_root": dir.path().to_str().unwrap() });
            assert_eq!(workspace_fallback(&md).name, "autumn", "for {url}");
        }
    }

    #[test]
    fn diff_catches_a_duplicated_component() {
        // Indexing by `bom-ref` collapses duplicates; without an explicit
        // length check a doubled entry would verify clean.
        let expected = bom_of(&[("serde", "1.0.0")]);
        let mut actual = expected.clone();
        let dup = actual.components[0].clone();
        actual.components.push(dup);
        let report = diff(&expected, &actual).join("\n");
        assert!(report.contains("component count"), "{report}");
    }

    #[test]
    fn extracts_dep_v0_from_a_big_endian_mach_o() {
        let mut macho = synth_macho("dep-v0", b"payload-bytes");
        // Byte-swap the magic to the big-endian spelling; the rest of the
        // fixture stays little-endian, so this only proves the magic is not
        // rejected outright — the swapped-header path is exercised by the
        // bounds checks that follow.
        macho[..4].copy_from_slice(&0xfeed_facfu32.to_be_bytes());
        assert!(
            !matches!(
                extract_dep_section(&macho),
                Err(SbomError::UnsupportedObjectFormat)
            ),
            "a big-endian Mach-O magic must not be reported as an unknown format"
        );
    }

    #[test]
    fn expected_version_accepts_a_matching_root() {
        let bom = bom_of(&[("serde", "1.0.0")]);
        assert!(check_expected_version(&bom, "0.7.0").is_ok());
    }

    #[test]
    fn expected_version_rejects_a_mismatched_root() {
        let bom = bom_of(&[("serde", "1.0.0")]);
        let err = check_expected_version(&bom, "9.9.9").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("9.9.9"), "{msg}");
        assert!(msg.contains("0.7.0"), "{msg}");
    }

    // ---------------------------------------------------------------------
    // cargo-auditable payload -> CycloneDX
    // ---------------------------------------------------------------------

    const AUDIT_JSON: &str = r#"{
        "packages": [
            {"name":"my-app","version":"1.2.3","source":"local","kind":"runtime","root":true,
             "dependencies":[1,2]},
            {"name":"serde","version":"1.0.0","source":"crates.io","dependencies":[]},
            {"name":"cc","version":"1.0.83","source":"crates.io","kind":"build","dependencies":[]}
        ]
    }"#;

    #[test]
    fn audit_data_root_becomes_the_top_level_component() {
        let bom = bom_from_audit_data(AUDIT_JSON, "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();

        assert_eq!(v["metadata"]["component"]["name"], "my-app");
        assert_eq!(v["metadata"]["component"]["version"], "1.2.3");
        let names: Vec<&str> = v["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["cc", "serde"]);
    }

    #[test]
    fn audit_data_marks_build_only_dependencies() {
        let bom = bom_from_audit_data(AUDIT_JSON, "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();
        let cc = &v["components"][0];
        assert_eq!(cc["name"], "cc");
        assert_eq!(cc["properties"][0]["name"], "cargo:dependency-kind");
        assert_eq!(cc["properties"][0]["value"], "build");
        assert_eq!(cc["properties"][1]["name"], "cargo:source");
        assert_eq!(cc["properties"][1]["value"], "crates.io");

        // `runtime` is the format's default kind, so a runtime dependency is
        // annotated with its provenance only — never a redundant kind property.
        let serde = &v["components"][1];
        assert_eq!(serde["name"], "serde");
        let props: Vec<&str> = serde["properties"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(props, vec!["cargo:source"]);
    }

    #[test]
    fn audit_data_without_a_root_still_produces_a_bom() {
        let bom = bom_from_audit_data(
            r#"{"packages":[{"name":"serde","version":"1.0.0","source":"crates.io"}]}"#,
            "0.7.0",
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();
        assert_eq!(v["components"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn rejects_audit_data_that_is_not_json() {
        assert!(bom_from_audit_data("not json", "0.7.0").is_err());
    }

    // ---------------------------------------------------------------------
    // object-file section extraction
    // ---------------------------------------------------------------------

    /// Build a synthetic ELF carrying `payload` in a section named `sec_name`.
    ///
    /// Writing fixed-width object-file headers is all narrowing casts by
    /// nature, and every value here is a handful of bytes.
    #[allow(clippy::cast_possible_truncation)]
    fn synth_elf(class64: bool, big_endian: bool, sec_name: &str, payload: &[u8]) -> Vec<u8> {
        let w16 = |v: u16| -> Vec<u8> {
            if big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };
        let w32 = |v: u32| -> Vec<u8> {
            if big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };
        let w64 = |v: u64| -> Vec<u8> {
            if big_endian {
                v.to_be_bytes().to_vec()
            } else {
                v.to_le_bytes().to_vec()
            }
        };
        let wn = |v: u64| -> Vec<u8> { if class64 { w64(v) } else { w32(v as u32) } };

        let ehdr_size: usize = if class64 { 64 } else { 52 };
        let shent: usize = if class64 { 64 } else { 40 };

        // .shstrtab: "\0.shstrtab\0<sec_name>\0"
        let mut strtab = vec![0u8];
        let shstrtab_off = strtab.len() as u32;
        strtab.extend_from_slice(b".shstrtab\0");
        let sec_name_off = strtab.len() as u32;
        strtab.extend_from_slice(sec_name.as_bytes());
        strtab.push(0);

        let strtab_file_off = ehdr_size;
        let payload_file_off = strtab_file_off + strtab.len();
        let shoff = payload_file_off + payload.len();

        let mut out = Vec::new();
        // ELF header
        out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
        out.push(if class64 { 2 } else { 1 });
        out.push(if big_endian { 2 } else { 1 });
        out.push(1); // EI_VERSION
        out.extend_from_slice(&[0u8; 9]); // padding to 16
        out.extend_from_slice(&w16(2)); // e_type = ET_EXEC
        out.extend_from_slice(&w16(62)); // e_machine
        out.extend_from_slice(&w32(1)); // e_version
        out.extend_from_slice(&wn(0)); // e_entry
        out.extend_from_slice(&wn(0)); // e_phoff
        out.extend_from_slice(&wn(shoff as u64)); // e_shoff
        out.extend_from_slice(&w32(0)); // e_flags
        out.extend_from_slice(&w16(ehdr_size as u16)); // e_ehsize
        out.extend_from_slice(&w16(0)); // e_phentsize
        out.extend_from_slice(&w16(0)); // e_phnum
        out.extend_from_slice(&w16(shent as u16)); // e_shentsize
        out.extend_from_slice(&w16(3)); // e_shnum
        out.extend_from_slice(&w16(1)); // e_shstrndx
        assert_eq!(out.len(), ehdr_size);

        out.extend_from_slice(&strtab);
        out.extend_from_slice(payload);
        assert_eq!(out.len(), shoff);

        let mut shdr = |name: u32, off: u64, size: u64| {
            out.extend_from_slice(&w32(name)); // sh_name
            out.extend_from_slice(&w32(1)); // sh_type = SHT_PROGBITS
            out.extend_from_slice(&wn(0)); // sh_flags
            out.extend_from_slice(&wn(0)); // sh_addr
            out.extend_from_slice(&wn(off)); // sh_offset
            out.extend_from_slice(&wn(size)); // sh_size
            out.extend_from_slice(&w32(0)); // sh_link
            out.extend_from_slice(&w32(0)); // sh_info
            out.extend_from_slice(&wn(0)); // sh_addralign
            out.extend_from_slice(&wn(0)); // sh_entsize
        };
        shdr(0, 0, 0); // SHN_UNDEF
        shdr(shstrtab_off, strtab_file_off as u64, strtab.len() as u64);
        shdr(sec_name_off, payload_file_off as u64, payload.len() as u64);
        out
    }

    #[test]
    fn extracts_dep_v0_from_elf64_little_endian() {
        let elf = synth_elf(true, false, ".dep-v0", b"payload-bytes");
        assert_eq!(extract_dep_section(&elf).unwrap(), b"payload-bytes");
    }

    #[test]
    fn extracts_dep_v0_from_elf32() {
        let elf = synth_elf(false, false, ".dep-v0", b"payload-bytes");
        assert_eq!(extract_dep_section(&elf).unwrap(), b"payload-bytes");
    }

    #[test]
    fn extracts_dep_v0_from_big_endian_elf() {
        let elf = synth_elf(true, true, ".dep-v0", b"payload-bytes");
        assert_eq!(extract_dep_section(&elf).unwrap(), b"payload-bytes");
    }

    #[test]
    fn reports_a_binary_built_without_cargo_auditable() {
        let elf = synth_elf(true, false, ".text", b"nothing-here");
        assert!(matches!(
            extract_dep_section(&elf),
            Err(SbomError::NoAuditData)
        ));
    }

    #[test]
    fn rejects_a_file_that_is_not_an_object_file() {
        assert!(matches!(
            extract_dep_section(b"#!/bin/sh\necho hi\n"),
            Err(SbomError::UnsupportedObjectFormat)
        ));
    }

    #[test]
    fn rejects_a_truncated_elf() {
        let elf = synth_elf(true, false, ".dep-v0", b"payload-bytes");
        assert!(matches!(
            extract_dep_section(&elf[..40]),
            Err(SbomError::MalformedObject)
        ));
    }

    #[test]
    fn rejects_an_elf_whose_section_runs_past_the_end_of_the_file() {
        let mut elf = synth_elf(true, false, ".dep-v0", b"payload-bytes");
        elf.truncate(elf.len() - 8); // chop the tail of the last section header
        assert!(extract_dep_section(&elf).is_err());
    }

    /// Build a synthetic 64-bit Mach-O carrying `payload` in `__DATA,<sec_name>`.
    #[allow(clippy::cast_possible_truncation)]
    fn synth_macho(sec_name: &str, payload: &[u8]) -> Vec<u8> {
        fn name16(s: &str) -> [u8; 16] {
            let mut b = [0u8; 16];
            b[..s.len()].copy_from_slice(s.as_bytes());
            b
        }
        let header_len = 32usize;
        let seg_len = 72usize;
        let sect_len = 80usize;
        let payload_off = header_len + seg_len + sect_len;

        let mut out = Vec::new();
        out.extend_from_slice(&0xfeed_facfu32.to_le_bytes()); // MH_MAGIC_64
        out.extend_from_slice(&0x0100_0007i32.to_le_bytes()); // cputype x86_64
        out.extend_from_slice(&3i32.to_le_bytes()); // cpusubtype
        out.extend_from_slice(&2u32.to_le_bytes()); // filetype MH_EXECUTE
        out.extend_from_slice(&1u32.to_le_bytes()); // ncmds
        out.extend_from_slice(&((seg_len + sect_len) as u32).to_le_bytes()); // sizeofcmds
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved

        out.extend_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
        out.extend_from_slice(&((seg_len + sect_len) as u32).to_le_bytes()); // cmdsize
        out.extend_from_slice(&name16("__DATA"));
        out.extend_from_slice(&0u64.to_le_bytes()); // vmaddr
        out.extend_from_slice(&0u64.to_le_bytes()); // vmsize
        out.extend_from_slice(&0u64.to_le_bytes()); // fileoff
        out.extend_from_slice(&0u64.to_le_bytes()); // filesize
        out.extend_from_slice(&0i32.to_le_bytes()); // maxprot
        out.extend_from_slice(&0i32.to_le_bytes()); // initprot
        out.extend_from_slice(&1u32.to_le_bytes()); // nsects
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        assert_eq!(out.len(), header_len + seg_len);

        out.extend_from_slice(&name16(sec_name));
        out.extend_from_slice(&name16("__DATA"));
        out.extend_from_slice(&0u64.to_le_bytes()); // addr
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes()); // size
        out.extend_from_slice(&(payload_off as u32).to_le_bytes()); // offset
        out.extend_from_slice(&0u32.to_le_bytes()); // align
        out.extend_from_slice(&0u32.to_le_bytes()); // reloff
        out.extend_from_slice(&0u32.to_le_bytes()); // nreloc
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved1
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved2
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved3
        assert_eq!(out.len(), payload_off);

        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn extracts_dep_v0_from_mach_o() {
        // cargo-auditable's Mach-O section name has no leading dot.
        let macho = synth_macho("dep-v0", b"payload-bytes");
        assert_eq!(extract_dep_section(&macho).unwrap(), b"payload-bytes");
    }

    #[test]
    fn extracts_dep_v0_from_mach_o_with_an_underscore_prefixed_name() {
        let macho = synth_macho("__dep_v0", b"payload-bytes");
        assert_eq!(extract_dep_section(&macho).unwrap(), b"payload-bytes");
    }

    #[test]
    fn reports_a_mach_o_built_without_cargo_auditable() {
        let macho = synth_macho("__text", b"nothing-here");
        assert!(matches!(
            extract_dep_section(&macho),
            Err(SbomError::NoAuditData)
        ));
    }

    /// Build a synthetic PE/COFF image carrying `payload` in `<sec_name>`.
    #[allow(clippy::cast_possible_truncation)]
    fn synth_pe(sec_name: &str, payload: &[u8]) -> Vec<u8> {
        let pe_off = 0x80usize;
        let opt_hdr_len = 0xf0usize;
        let sect_tbl_off = pe_off + 4 + 20 + opt_hdr_len;
        let payload_off = sect_tbl_off + 40;

        let mut out = vec![0u8; pe_off];
        out[0] = b'M';
        out[1] = b'Z';
        out[0x3c..0x40].copy_from_slice(&(pe_off as u32).to_le_bytes());

        out.extend_from_slice(b"PE\0\0");
        out.extend_from_slice(&0x8664u16.to_le_bytes()); // machine
        out.extend_from_slice(&1u16.to_le_bytes()); // numberOfSections
        out.extend_from_slice(&0u32.to_le_bytes()); // timeDateStamp
        out.extend_from_slice(&0u32.to_le_bytes()); // pointerToSymbolTable
        out.extend_from_slice(&0u32.to_le_bytes()); // numberOfSymbols
        out.extend_from_slice(&(opt_hdr_len as u16).to_le_bytes()); // sizeOfOptionalHeader
        out.extend_from_slice(&0u16.to_le_bytes()); // characteristics
        out.extend_from_slice(&vec![0u8; opt_hdr_len]);
        assert_eq!(out.len(), sect_tbl_off);

        let mut name = [0u8; 8];
        name[..sec_name.len()].copy_from_slice(sec_name.as_bytes());
        out.extend_from_slice(&name);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // virtualSize
        out.extend_from_slice(&0u32.to_le_bytes()); // virtualAddress
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // sizeOfRawData
        out.extend_from_slice(&(payload_off as u32).to_le_bytes()); // pointerToRawData
        out.extend_from_slice(&0u32.to_le_bytes()); // pointerToRelocations
        out.extend_from_slice(&0u32.to_le_bytes()); // pointerToLinenumbers
        out.extend_from_slice(&0u16.to_le_bytes()); // numberOfRelocations
        out.extend_from_slice(&0u16.to_le_bytes()); // numberOfLinenumbers
        out.extend_from_slice(&0u32.to_le_bytes()); // characteristics
        assert_eq!(out.len(), payload_off);

        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn extracts_dep_v0_from_pe() {
        let pe = synth_pe(".dep-v0", b"payload-bytes");
        assert_eq!(extract_dep_section(&pe).unwrap(), b"payload-bytes");
    }

    // ---------------------------------------------------------------------
    // zlib payload decoding
    // ---------------------------------------------------------------------

    #[test]
    fn inflates_the_zlib_compressed_audit_payload() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write as _;

        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(AUDIT_JSON.as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();

        assert_eq!(inflate_audit_data(&compressed).unwrap(), AUDIT_JSON);
    }

    #[test]
    fn rejects_a_corrupt_audit_payload() {
        assert!(inflate_audit_data(b"not-zlib-data").is_err());
    }

    // ---------------------------------------------------------------------
    // end-to-end: object file -> CycloneDX
    // ---------------------------------------------------------------------

    #[test]
    fn reads_a_bom_straight_out_of_an_auditable_binary() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;
        use std::io::Write as _;

        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(AUDIT_JSON.as_bytes()).unwrap();
        let elf = synth_elf(true, false, ".dep-v0", &enc.finish().unwrap());

        let bom = bom_from_binary(&elf, "0.7.0").unwrap();
        let v: serde_json::Value = serde_json::from_str(&render(&bom).unwrap()).unwrap();
        assert_eq!(v["metadata"]["component"]["name"], "my-app");
        assert_eq!(v["components"].as_array().unwrap().len(), 2);
    }
}
