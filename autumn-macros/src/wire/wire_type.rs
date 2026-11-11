//! A field's wire type: read off its Rust type, or set by `#[wire(type = …)]`.
//!
//! The map knows a type by its last name, as the export knows a leaf. A
//! type it does not know is a nested `WireShape` type: an object, by the path
//! as written. The export changes that path to the type's id.

use autumn_macros_support::wire::{KEPT_FIELD_NUMBERS, MAX_FIELD_NUMBER, format};
use quote::quote;
use syn::spanned::Spanned as _;

use crate::wire::ir::WireType;

/// The types `#[wire(type = …)]` takes.
const OVERRIDE_TYPES: &str = "`string`, `integer`, `number`, `boolean`, `any`";

/// What `#[wire(…)]` on one field says.
#[derive(Debug, Default)]
pub struct WireAttrs {
    /// `#[wire(opaque)]`.
    pub opaque: bool,
    /// `#[wire(type = …)]`, with its `format` and `nullable`.
    pub wire_type: Option<WireType>,
    /// `#[wire(tag = N)]`: the protobuf field number, with its span.
    pub tag: Option<(u32, proc_macro2::Span)>,
}

/// Parse every `#[wire(…)]` on a field.
///
/// # Errors
/// An unknown argument or type, a bad `format`, a `format` or `nullable`
/// with no `type`, or a key set twice.
pub fn wire_attrs(field: &syn::Field) -> Result<WireAttrs, syn::Error> {
    let mut opaque = false;
    let mut ty: Option<syn::LitStr> = None;
    let mut fmt: Option<syn::LitStr> = None;
    let mut nullable: Option<proc_macro2::Span> = None;
    let mut tag: Option<(u32, proc_macro2::Span)> = None;
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("wire")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("opaque") {
                opaque = true;
            } else if meta.path.is_ident("type") {
                set_once(&mut ty, meta.value()?.parse()?, "type")?;
            } else if meta.path.is_ident("format") {
                set_once(&mut fmt, meta.value()?.parse()?, "format")?;
            } else if meta.path.is_ident("nullable") {
                nullable = Some(meta.path.span());
            } else if meta.path.is_ident("tag") {
                let lit: syn::Lit = meta.value()?.parse()?;
                let number = checked_tag(&lit)?;
                if tag.is_some() {
                    return Err(syn::Error::new(lit.span(), "`tag` is set twice"));
                }
                tag = Some((number, lit.span()));
            } else {
                return Err(meta.error(
                    "unknown `wire` argument; expected `opaque`, `type`, `format`, `nullable` \
                     or `tag`",
                ));
            }
            Ok(())
        })?;
    }
    let Some(ty) = ty else {
        if let Some(fmt) = fmt {
            return Err(syn::Error::new(fmt.span(), "`format` needs `type`"));
        }
        if let Some(span) = nullable {
            return Err(syn::Error::new(span, "`nullable` needs `type`"));
        }
        return Ok(WireAttrs {
            opaque,
            wire_type: None,
            tag,
        });
    };
    let kind = ty.value();
    if !matches!(
        kind.as_str(),
        "string" | "integer" | "number" | "boolean" | "any"
    ) {
        return Err(syn::Error::new(
            ty.span(),
            format!("unknown wire type `{kind}`; expected one of {OVERRIDE_TYPES}"),
        ));
    }
    let format = match fmt {
        Some(lit) if !matches!(kind.as_str(), "string" | "integer") => {
            return Err(syn::Error::new(
                lit.span(),
                "`format` applies to `string` and `integer` only",
            ));
        }
        Some(lit) => Some(checked_format(&lit)?),
        None => None,
    };
    let base = match kind.as_str() {
        "string" => WireType::String { format },
        // A custom serializer usually writes an `i64`.
        "integer" => WireType::Integer {
            bits: 64,
            signed: true,
            format,
        },
        "number" => WireType::Number,
        "boolean" => WireType::Boolean,
        _ => WireType::Any,
    };
    let wire_type = if nullable.is_some() {
        WireType::nullable(base)
    } else {
        base
    };
    Ok(WireAttrs {
        opaque,
        wire_type: Some(wire_type),
        tag,
    })
}

/// The field number in `#[wire(tag = N)]`, if protobuf allows it.
fn checked_tag(lit: &syn::Lit) -> syn::Result<u32> {
    let syn::Lit::Int(int) = lit else {
        return Err(syn::Error::new(
            lit.span(),
            "`tag` is an integer, such as `tag = 3`",
        ));
    };
    let number = int
        .base10_parse::<u64>()
        .ok()
        .filter(|n| (1..=u64::from(MAX_FIELD_NUMBER)).contains(n))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            syn::Error::new(
                int.span(),
                format!("a protobuf field number is 1 to {MAX_FIELD_NUMBER}"),
            )
        })?;
    if KEPT_FIELD_NUMBERS.contains(&number) {
        return Err(syn::Error::new(
            int.span(),
            "protobuf keeps field numbers 19000 to 19999 for its own use",
        ));
    }
    Ok(number)
}

/// Store `value` in `slot`, or refuse a key that is set twice.
fn set_once(slot: &mut Option<syn::LitStr>, value: syn::LitStr, key: &str) -> syn::Result<()> {
    if slot.is_some() {
        return Err(syn::Error::new(
            value.span(),
            format!("`{key}` is set twice"),
        ));
    }
    *slot = Some(value);
    Ok(())
}

/// The format text, if it is lowercase words joined by `-`.
fn checked_format(lit: &syn::LitStr) -> syn::Result<String> {
    let text = lit.value();
    let good = !text.is_empty()
        && text.split('-').all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    if good {
        Ok(text)
    } else {
        Err(syn::Error::new(
            lit.span(),
            "a format is lowercase words joined by `-`, such as `date-time`",
        ))
    }
}

/// The wire type of a Rust type.
///
/// `own` is the name of the type that holds the field: `Self` names it. In an
/// opaque field, a type the map does not know is `Any`, not an object.
#[must_use]
pub fn wire_type_of(ty: &syn::Type, own: &str, opaque: bool) -> WireType {
    Mapper { own, opaque }.map(ty)
}

/// The context of one field's map.
struct Mapper<'a> {
    own: &'a str,
    opaque: bool,
}

impl Mapper<'_> {
    fn map(&self, ty: &syn::Type) -> WireType {
        match ty {
            syn::Type::Paren(t) => self.map(&t.elem),
            syn::Type::Group(t) => self.map(&t.elem),
            syn::Type::Reference(t) => self.map(&t.elem),
            syn::Type::Slice(t) => WireType::array(self.map(&t.elem)),
            syn::Type::Array(t) => WireType::array(self.map(&t.elem)),
            syn::Type::Path(t) if t.qself.is_none() => self.path(&t.path),
            // A tuple is an array of mixed types, and `()` is `null`. The map
            // cannot read other forms.
            _ => WireType::Any,
        }
    }

    fn path(&self, path: &syn::Path) -> WireType {
        let Some(last) = path.segments.last() else {
            return WireType::Any;
        };
        let name = last.ident.to_string();
        let args = type_args(&last.arguments);
        let arg = |i: usize| args.get(i).map_or(WireType::Any, |t| self.map(t));
        if let Some(scalar) = scalar(&name) {
            return scalar;
        }
        match name.as_str() {
            "Self" if self.opaque => WireType::Any,
            "Self" => WireType::object(self.own),
            // chrono's `DateTime<Tz>`. A `DateTime` with no time zone is
            // another crate's type, with another form.
            "DateTime" if args.len() == 1 => WireType::string_in(format::DATE_TIME),
            "NonZero" => match arg(0) {
                integer @ WireType::Integer { .. } => integer,
                _ => WireType::Any,
            },
            "Option" => WireType::nullable(arg(0)),
            // serde writes these as the value they hold.
            "Box" | "Rc" | "Arc" | "Cow" | "Cell" | "RefCell" | "Reverse" | "Wrapping" => arg(0),
            // `SmallVec<[T; N]>`, or `SmallVec<T, N>`.
            "SmallVec" => match args.first() {
                Some(array @ syn::Type::Array(_)) => self.map(array),
                _ => WireType::array(arg(0)),
            },
            "Vec" | "VecDeque" | "LinkedList" | "BinaryHeap" | "HashSet" | "BTreeSet"
            | "IndexSet" => WireType::array(arg(0)),
            "HashMap" | "BTreeMap" | "IndexMap" | "Map" => match (arg(0), arg(1)) {
                // JSON keys are strings. serde writes an integer key as one.
                (WireType::String { .. } | WireType::Integer { .. }, value) => WireType::map(value),
                _ => WireType::Any,
            },
            _ if autumn_macros_support::wire::is_known_leaf(&name) => WireType::Any,
            _ if self.opaque => WireType::Any,
            _ => WireType::object(&render_path(path)),
        }
    }
}

/// The wire type of a type with no type arguments, by its last name.
///
/// A `time` crate type is not here: its JSON form depends on a feature of
/// that crate, which the macro cannot see.
fn scalar(name: &str) -> Option<WireType> {
    let integer = |bits, signed| Some(WireType::integer(bits, signed));
    let text = |format: &str| Some(WireType::string_in(format));
    match name {
        "String" | "str" | "char" | "PathBuf" | "Path" | "SocketAddr" => Some(WireType::string()),
        "bool" => Some(WireType::Boolean),
        "f32" | "f64" | "Number" => Some(WireType::Number),
        "i8" | "NonZeroI8" => integer(8, true),
        "i16" | "NonZeroI16" => integer(16, true),
        "i32" | "NonZeroI32" => integer(32, true),
        "i64" | "NonZeroI64" | "isize" | "NonZeroIsize" => integer(64, true),
        "i128" | "NonZeroI128" => integer(128, true),
        "u8" | "NonZeroU8" => integer(8, false),
        "u16" | "NonZeroU16" => integer(16, false),
        "u32" | "NonZeroU32" => integer(32, false),
        "u64" | "NonZeroU64" | "usize" | "NonZeroUsize" => integer(64, false),
        "u128" | "NonZeroU128" => integer(128, false),
        // chrono writes ISO 8601 text.
        "NaiveDateTime" => text(format::LOCAL_DATE_TIME),
        "NaiveDate" => text(format::DATE),
        "NaiveTime" => text(format::LOCAL_TIME),
        "Uuid" => text(format::UUID),
        "Decimal" | "BigDecimal" => text(format::DECIMAL),
        "Url" => text(format::URI),
        "IpAddr" => text(format::IP),
        "Ipv4Addr" => text(format::IPV4),
        "Ipv6Addr" => text(format::IPV6),
        // serde_json writes bytes as an array of numbers.
        "Bytes" | "ByteBuf" => Some(WireType::array(WireType::integer(8, false))),
        _ => None,
    }
}

/// The type arguments of a path segment. Lifetimes and consts are left out.
fn type_args(args: &syn::PathArguments) -> Vec<&syn::Type> {
    match args {
        syn::PathArguments::AngleBracketed(args) => args
            .args
            .iter()
            .filter_map(|arg| match arg {
                syn::GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A path as written, with no type arguments: `shared::Item`, `::a::B`.
///
/// The export finds the type by this text, as it does for a path in `ty`.
fn render_path(path: &syn::Path) -> String {
    let segments: Vec<String> = path
        .segments
        .iter()
        .map(|s| {
            let ident = &s.ident;
            quote!(#ident).to_string()
        })
        .collect();
    let lead = if path.leading_colon.is_some() {
        "::"
    } else {
        ""
    };
    format!("{lead}{}", segments.join("::"))
}
