//! `#[obligation]` attribute macro (issue #1826).
//!
//! It keeps the struct and adds one method:
//!
//! ```ignore
//! #[obligation(name = first_response, within = "2 business days", starts = opened_at)]
//! pub struct Ticket { pub id: i64, pub opened_at: DateTime<Utc> }
//!
//! // expands to the struct and:
//! impl Ticket {
//!     pub fn first_response_obligation(&self) -> ::autumn_web::sla::Obligation { … }
//! }
//! ```
//!
//! The macro parses `within` at compile time. The grammar is the same as
//! `BusinessDuration::from_str` in `autumn_web::sla`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::ext::IdentExt as _;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, ItemStruct, LitStr, Token};

/// Parsed `#[obligation(...)]` arguments.
struct ObligationArgs {
    name: Ident,
    within: (u32, u64),
    calendar: Option<LitStr>,
    starts: Ident,
    met: Option<Ident>,
    zone: Option<Ident>,
    subject: Option<Ident>,
}

impl Parse for ObligationArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut within = None;
        let mut calendar = None;
        let mut starts = None;
        let mut met = None;
        let mut zone = None;
        let mut subject = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let duplicate = || syn::Error::new_spanned(&key, format!("duplicate `{key}`"));
            match key.to_string().as_str() {
                "within" => {
                    let lit: LitStr = input.parse()?;
                    let parsed = parse_duration(&lit.value()).ok_or_else(|| {
                        syn::Error::new_spanned(
                            &lit,
                            "invalid business duration: use a form such as \"2 business days\"",
                        )
                    })?;
                    if within.replace(parsed).is_some() {
                        return Err(duplicate());
                    }
                }
                "calendar" => {
                    if calendar.replace(input.parse::<LitStr>()?).is_some() {
                        return Err(duplicate());
                    }
                }
                "name" | "starts" | "met" | "zone" | "subject" => {
                    let field: Ident = input.parse()?;
                    let slot = match key.to_string().as_str() {
                        "name" => &mut name,
                        "starts" => &mut starts,
                        "met" => &mut met,
                        "zone" => &mut zone,
                        _ => &mut subject,
                    };
                    if slot.replace(field).is_some() {
                        return Err(duplicate());
                    }
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        "unknown argument: use `name`, `within`, `calendar`, `starts`, `met`, `zone` or `subject`",
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        let missing = |what: &str| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                format!("`#[obligation]` needs `{what}`"),
            )
        };
        Ok(Self {
            name: name.ok_or_else(|| missing("name = <ident>"))?,
            within: within.ok_or_else(|| missing("within = \"...\""))?,
            starts: starts.ok_or_else(|| missing("starts = <field>"))?,
            calendar,
            met,
            zone,
            subject,
        })
    }
}

/// Parse `<count> [business] <unit>` parts to `(days, seconds)`. A comma,
/// `and`, or both join two parts.
///
/// Keep in step with `parse` in `autumn/src/sla/duration.rs`.
fn parse_duration(text: &str) -> Option<(u32, u64)> {
    let lower = text.to_ascii_lowercase().replace(',', " , ");
    let mut words = lower.split_whitespace().peekable();
    let (mut days, mut secs) = (0_u32, 0_u64);
    loop {
        let count = words.next()?;
        if !count.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let count: u64 = count.parse().ok()?;
        words.next_if_eq(&"business");
        match words.next()? {
            "day" | "days" => days = days.checked_add(u32::try_from(count).ok()?)?,
            "hour" | "hours" => secs = secs.checked_add(count.checked_mul(3_600)?)?,
            "minute" | "minutes" => secs = secs.checked_add(count.checked_mul(60)?)?,
            "second" | "seconds" => secs = secs.checked_add(count)?,
            _ => return None,
        }
        if words.peek().is_none() {
            return Some((days, secs));
        }
        let comma = words.next_if_eq(&",").is_some();
        let and = words.next_if_eq(&"and").is_some();
        if !comma && !and {
            return None;
        }
    }
}

pub fn obligation_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args: ObligationArgs = match syn::parse2(attr) {
        Ok(args) => args,
        Err(err) => return err.to_compile_error(),
    };
    let item: ItemStruct = match syn::parse2(item) {
        Ok(item) => item,
        Err(err) => {
            return syn::Error::new(err.span(), "`#[obligation]` applies to a struct")
                .to_compile_error();
        }
    };

    let ident = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();
    let vis = &item.vis;
    let name = args.name.unraw().to_string();
    let method = format_ident!("{}_obligation", name);
    let prefix = autumn_macros_support::naming::pascal_to_snake(&ident.to_string());
    let subject = args.subject.unwrap_or_else(|| format_ident!("id"));
    let (days, secs) = args.within;
    let calendar = args.calendar.map_or_else(
        || quote!(::autumn_web::sla::Obligation::DEFAULT_CALENDAR),
        |c| quote!(#c),
    );
    let starts = &args.starts;
    let met = args
        .met
        .map(|field| quote!(.met_at(::std::clone::Clone::clone(&self.#field))));
    let zone = args.zone.map(|field| quote!(.zone_from(&self.#field)));
    let doc = format!("The `{name}` obligation of this value (see `#[obligation]`).");

    quote! {
        #item

        impl #impl_generics #ident #ty_generics #where_clause {
            #[doc = #doc]
            #[must_use]
            #vis fn #method(&self) -> ::autumn_web::sla::Obligation {
                ::autumn_web::sla::Obligation::new(
                    #name,
                    ::std::format!("{}:{}", #prefix, self.#subject),
                )
                .within(::autumn_web::sla::BusinessDuration::from_parts(#days, #secs))
                .calendar(#calendar)
                .starting_at(::std::clone::Clone::clone(&self.#starts))
                #met
                #zone
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn parses_the_runtime_grammar() {
        assert_eq!(parse_duration("2 business days"), Some((2, 0)));
        assert_eq!(parse_duration("1 day, 4 hours"), Some((1, 14_400)));
        assert_eq!(
            parse_duration("30 business minutes and 5 seconds"),
            Some((0, 1_805))
        );
        assert_eq!(parse_duration("2 fortnights"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("2"), None);
        assert_eq!(parse_duration("+2 days"), None);
        assert_eq!(parse_duration("and 2 and days"), None);
        assert_eq!(parse_duration("1 day 2 hours"), None);
        assert_eq!(parse_duration("1 day,"), None);
    }
}
