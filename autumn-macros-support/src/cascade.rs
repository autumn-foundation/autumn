//! Const items that publish which models a delete also writes.
//!
//! A repository and a model publish `CASCADE_LEVELS` consts. Level `k` lists
//! the models reachable in at most `k + 1` steps through non-`restrict`
//! `dependent` legs. Level `k` reads only level `k - 1` of other types. A
//! cascade that loops can never form a const cycle.
//!
//! Each level also says whether the cascade goes deeper than it covers.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

/// How many levels a type publishes. A deeper or cyclic cascade is reported as
/// `more`, and the grant check refuses it.
pub const CASCADE_LEVELS: usize = 4;

/// One non-`restrict` `dependent` leg of a delete.
pub struct CascadeLeg {
    /// Expression for the child model's name, as a `&str`.
    pub model: TokenStream,
    /// Path to the child repository, which publishes the level consts.
    pub child: TokenStream,
    /// Whether the child runs its own cascade (`destroy`).
    pub destroy: bool,
}

/// The `ANY` const: a literal saying whether the type has any cascade leg. It
/// refers to no other type, so level 0 can read it from a child.
#[must_use]
pub fn any_const(legs: &[CascadeLeg]) -> TokenStream {
    let any = !legs.is_empty();
    quote! { #any }
}

/// The name of level `k` on a repository.
#[must_use]
pub fn level_name(k: usize) -> proc_macro2::Ident {
    format_ident!("__AUTUMN_CASCADE_{}", k)
}

/// The expression for level `k` of a type with these `legs`.
#[must_use]
pub fn cascade_level(legs: &[CascadeLeg], k: usize) -> TokenStream {
    let mut parts: Vec<TokenStream> = Vec::new();
    let mut more: Vec<TokenStream> = Vec::new();
    for leg in legs {
        let model = &leg.model;
        parts.push(quote! { &[#model] });
        if !leg.destroy {
            continue;
        }
        let child = &leg.child;
        if k == 0 {
            // A literal, so level 0 holds no reference to another level 0.
            more.push(quote! { #child::__AUTUMN_CASCADE_ANY });
        } else {
            let below = level_name(k - 1);
            parts.push(quote! { #child::#below.models });
            more.push(quote! { #child::#below.more });
        }
    }
    if parts.is_empty() {
        return quote! { ::autumn_web::agent_authority::CascadeLevel::NONE };
    }
    let more = if more.is_empty() {
        quote! { false }
    } else {
        quote! { #(#more)||* }
    };
    quote! {{
        const PARTS: &[&[&::core::primitive::str]] = &[#(#parts),*];
        const N: usize = ::autumn_web::agent_authority::cascade_len(PARTS);
        const JOINED: [&::core::primitive::str; N] =
            ::autumn_web::agent_authority::cascade_union::<N>(PARTS);
        ::autumn_web::agent_authority::CascadeLevel {
            models: &JOINED,
            more: #more,
        }
    }}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(model: &str, child: &str, destroy: bool) -> CascadeLeg {
        let child: TokenStream = child.parse().unwrap();
        CascadeLeg {
            model: quote! { #model },
            child,
            destroy,
        }
    }

    #[test]
    fn no_legs_publish_the_empty_level() {
        let text = cascade_level(&[], 2).to_string();
        assert!(text.contains("CascadeLevel :: NONE"), "{text}");
    }

    #[test]
    fn level_zero_reads_no_other_level() {
        let text = cascade_level(&[leg("Comment", "PgCommentRepository", true)], 0).to_string();
        assert!(text.contains("\"Comment\""), "{text}");
        assert!(!text.contains("__AUTUMN_CASCADE_1"), "{text}");
        assert!(
            text.contains("PgCommentRepository :: __AUTUMN_CASCADE_ANY"),
            "{text}"
        );
    }

    #[test]
    fn a_level_reads_only_the_level_below() {
        let text = cascade_level(&[leg("Comment", "PgCommentRepository", true)], 3).to_string();
        assert!(text.contains("PgCommentRepository :: __AUTUMN_CASCADE_2 . models"));
        assert!(text.contains("PgCommentRepository :: __AUTUMN_CASCADE_2 . more"));
        assert!(!text.contains("__AUTUMN_CASCADE_3"), "{text}");
    }

    #[test]
    fn a_leg_that_does_not_destroy_adds_its_model_only() {
        let text = cascade_level(&[leg("Like", "PgLikeRepository", false)], 2).to_string();
        assert!(text.contains("\"Like\""), "{text}");
        assert!(!text.contains("PgLikeRepository"), "{text}");
        assert!(text.contains("more : false"), "{text}");
    }
}
