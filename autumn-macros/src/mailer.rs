//! `#[mailer]` proc macro implementation.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
    Expr, ExprLit, FnArg, GenericParam, ImplItem, ImplItemFn, ItemImpl, Lit, MetaNameValue, Pat,
    ReturnType, Type,
};

struct MailMethod {
    method: syn::Ident,
    send_method: syn::Ident,
    later_method: syn::Ident,
    vis: syn::Visibility,
    generics: syn::Generics,
    cfg_attrs: Vec<syn::Attribute>,
    args: Vec<(syn::Ident, Type)>,
}

pub fn returns_mail(method: &ImplItemFn) -> bool {
    let ReturnType::Type(_, ty) = &method.sig.output else {
        return false;
    };
    let Type::Path(type_path) = ty.as_ref() else {
        return false;
    };
    if type_path.qself.is_some() {
        return false;
    }

    let segments = type_path
        .path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();

    match segments.as_slice() {
        [mail] => mail == "Mail",
        [autumn_web, mail] => autumn_web == "autumn_web" && mail == "Mail",
        [autumn_web, module, mail] => {
            autumn_web == "autumn_web" && module == "mail" && mail == "Mail"
        }
        _ => false,
    }
}

fn parse_mail_method(method: &ImplItemFn) -> syn::Result<Option<MailMethod>> {
    if !returns_mail(method) {
        return Ok(None);
    }
    let Some(receiver) = method.sig.receiver() else {
        return Ok(None);
    };
    if receiver.reference.is_none() || receiver.mutability.is_some() {
        return Err(syn::Error::new_spanned(
            receiver,
            "#[mailer] template methods must use an `&self` receiver",
        ));
    }
    if method.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            &method.sig.ident,
            "#[mailer] template methods must be synchronous and return Mail",
        ));
    }

    let mut args = Vec::new();
    for arg in &method.sig.inputs {
        match arg {
            FnArg::Receiver(_) => {}
            FnArg::Typed(pat_type) => {
                let Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
                    return Err(syn::Error::new_spanned(
                        &pat_type.pat,
                        "#[mailer] method arguments must be simple identifiers",
                    ));
                };
                args.push((pat_ident.ident.clone(), (*pat_type.ty).clone()));
            }
        }
    }

    let method_name = &method.sig.ident;
    Ok(Some(MailMethod {
        method: method_name.clone(),
        send_method: format_ident!("send_{method_name}"),
        later_method: format_ident!("deliver_later_{method_name}"),
        vis: method.vis.clone(),
        generics: method.sig.generics.clone(),
        cfg_attrs: method
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
            .cloned()
            .collect(),
        args,
    }))
}

/// Parse the optional `list_unsubscribe = "scope"` attribute argument.
fn parse_list_unsubscribe(attr: TokenStream) -> syn::Result<Option<String>> {
    if attr.is_empty() {
        return Ok(None);
    }
    let meta: MetaNameValue = syn::parse2(attr)?;
    if !meta.path.is_ident("list_unsubscribe") {
        return Err(syn::Error::new_spanned(
            &meta.path,
            "unknown #[mailer] argument; expected `list_unsubscribe = \"...\"`",
        ));
    }
    let Expr::Lit(ExprLit {
        lit: Lit::Str(value),
        ..
    }) = &meta.value
    else {
        return Err(syn::Error::new_spanned(
            &meta.value,
            "list_unsubscribe must be a string literal, e.g. list_unsubscribe = \"weekly_digest\"",
        ));
    };
    let scope = value.value();
    if scope.trim().is_empty() {
        return Err(syn::Error::new_spanned(
            value,
            "list_unsubscribe scope must not be empty",
        ));
    }
    Ok(Some(scope))
}

/// Best-effort label for a mailer `Self` type, used for inventory registration.
fn self_ty_label(self_ty: &Type) -> String {
    if let Type::Path(type_path) = self_ty
        && let Some(segment) = type_path.path.segments.last()
    {
        return segment.ident.to_string();
    }
    quote!(#self_ty).to_string()
}

#[allow(clippy::too_many_lines, clippy::option_if_let_else)]
pub fn mailer_macro(attr: TokenStream, item: TokenStream) -> TokenStream {
    let list_unsubscribe = match parse_list_unsubscribe(attr) {
        Ok(scope) => scope,
        Err(err) => return err.to_compile_error(),
    };

    let mut input_impl: ItemImpl = match syn::parse2(item) {
        Ok(item) => item,
        Err(err) => return err.to_compile_error(),
    };

    let self_ty = input_impl.self_ty.clone();
    let input_generics = input_impl.generics.clone();
    let (impl_generics, _ty_generics, where_clause) = input_generics.split_for_impl();
    let impl_cfg_attrs = input_impl
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
        .cloned()
        .collect::<Vec<_>>();
    let mut methods = Vec::new();
    for item in &input_impl.items {
        if let ImplItem::Fn(method) = item {
            match parse_mail_method(method) {
                Ok(Some(method)) => methods.push(method),
                Ok(None) => {}
                Err(err) => return err.to_compile_error(),
            }
        }
    }

    // When the mailer opts into list_unsubscribe, tag the `Mail` returned by the
    // original template methods too — not just the generated send_*/deliver_later_*
    // helpers. The template methods are public, so a direct call
    // (`mailer.digest(to)`) whose result is passed to `Mailer::send` would
    // otherwise send the bulk template with no List-Unsubscribe headers or
    // suppression, even though the inventory descriptor already made startup/doctor
    // report this mailer as compliant. The original body is wrapped in an
    // immediately-invoked closure so an early `return` inside the template is
    // tagged too.
    if let Some(scope) = &list_unsubscribe {
        for item in &mut input_impl.items {
            if let ImplItem::Fn(method) = item
                && returns_mail(method)
                && method
                    .sig
                    .receiver()
                    .is_some_and(|r| r.reference.is_some() && r.mutability.is_none())
            {
                let orig_block = method.block.clone();
                method.block = syn::parse_quote!({
                    #[allow(clippy::redundant_closure_call)]
                    let mut __autumn_list_mail: ::autumn_web::mail::Mail = (|| #orig_block)();
                    __autumn_list_mail.list_unsubscribe =
                        ::core::option::Option::Some(::std::string::String::from(#scope));
                    __autumn_list_mail
                });
            }
        }
    }

    let generated = methods.iter().map(|method| {
        let original = &method.method;
        let send_method = &method.send_method;
        let later_method = &method.later_method;
        let vis = &method.vis;
        let cfg_attrs = &method.cfg_attrs;
        let method_generic_params = &method.generics.params;
        let method_generic_decl = if method_generic_params.is_empty() {
            quote! {}
        } else {
            quote! { <#method_generic_params> }
        };
        let method_generic_call_args = method
            .generics
            .params
            .iter()
            .filter_map(|param| match param {
                GenericParam::Type(param) => {
                    let ident = &param.ident;
                    Some(quote! { #ident })
                }
                GenericParam::Lifetime(_) => None,
                GenericParam::Const(param) => {
                    let ident = &param.ident;
                    Some(quote! { #ident })
                }
            })
            .collect::<Vec<_>>();
        let method_generic_call = if method_generic_call_args.is_empty() {
            quote! {}
        } else {
            quote! { ::<#(#method_generic_call_args),*> }
        };
        let method_where_clause = &method.generics.where_clause;
        let helper_mailer_arg = format_ident!("__autumn_mailer");
        let arg_defs = method.args.iter().map(|(name, ty)| quote! { #name: #ty });
        let arg_defs_later = method.args.iter().map(|(name, ty)| quote! { #name: #ty });
        let arg_names = method.args.iter().map(|(name, _)| quote! { #name });
        let arg_names_later = method.args.iter().map(|(name, _)| quote! { #name });
        // When the mailer opts into list_unsubscribe, tag every built message
        // with the scope so the runtime emits RFC 8058 headers and applies
        // suppression at send time.
        let (bind_send, bind_later) = if let Some(scope) = &list_unsubscribe {
            (
                quote! {
                    let mut mail = self.#original #method_generic_call ( #( #arg_names, )* );
                    mail.list_unsubscribe =
                        ::core::option::Option::Some(::std::string::String::from(#scope));
                },
                quote! {
                    let mut mail = self.#original #method_generic_call ( #( #arg_names_later, )* );
                    mail.list_unsubscribe =
                        ::core::option::Option::Some(::std::string::String::from(#scope));
                },
            )
        } else {
            (
                quote! { let mail = self.#original #method_generic_call ( #( #arg_names, )* ); },
                quote! { let mail = self.#original #method_generic_call ( #( #arg_names_later, )* ); },
            )
        };
        quote! {
            #( #cfg_attrs )*
            #vis async fn #send_method #method_generic_decl (
                &self,
                #helper_mailer_arg: &::autumn_web::mail::Mailer,
                #( #arg_defs, )*
            ) -> ::autumn_web::AutumnResult<()>
            #method_where_clause
            {
                #bind_send
                #helper_mailer_arg
                    .send(mail)
                    .await
                    .map_err(::autumn_web::AutumnError::internal_server_error)
            }

            #( #cfg_attrs )*
            #vis fn #later_method #method_generic_decl (
                &self,
                #helper_mailer_arg: &::autumn_web::mail::Mailer,
                #( #arg_defs_later, )*
            )
            #method_where_clause
            {
                #bind_later
                #helper_mailer_arg.deliver_later(mail);
            }
        }
    });

    // Register the list_unsubscribe declaration so production startup and
    // `autumn doctor` can fail closed when no unsubscribe destination is set.
    // Emit one registration per mail method, carrying that method's `cfg`
    // attributes, so a build that `#[cfg]`-removes every `Mail` method (and thus
    // sends no list mail) registers nothing and does not force unsubscribe config.
    let registrations: Vec<TokenStream> = if let Some(scope) = &list_unsubscribe {
        let mailer_label = self_ty_label(&self_ty);
        methods
            .iter()
            .map(|method| {
                let cfg_attrs = &method.cfg_attrs;
                quote! {
                    #( #impl_cfg_attrs )*
                    #( #cfg_attrs )*
                    ::autumn_web::reexports::inventory::submit! {
                        ::autumn_web::mail::MailerListUnsubscribeDescriptor {
                            mailer: #mailer_label,
                            scope: #scope,
                        }
                    }
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    quote! {
        #input_impl

        #( #impl_cfg_attrs )*
        impl #impl_generics #self_ty #where_clause {
            #( #generated )*
        }

        #( #registrations )*
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::quote;

    #[test]
    fn generates_send_and_later_helpers() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn reset(&self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("send_reset"));
        assert!(rendered.contains("deliver_later_reset"));
    }

    #[test]
    fn preserves_impl_generics_and_where_clause() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl<T> AccountMailer<T>
                where
                    T: Clone,
                {
                    fn reset(&self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("impl < T > AccountMailer < T > where T : Clone"));
        assert!(rendered.contains("send_reset"));
    }

    #[test]
    fn preserves_method_generics_and_where_clause() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn welcome<T>(&self, to: T) -> Mail
                    where
                        T: std::fmt::Display,
                    {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("async fn send_welcome < T >"));
        assert!(rendered.contains("where T : std :: fmt :: Display"));
        assert!(rendered.contains("fn deliver_later_welcome < T >"));
    }

    #[test]
    fn forwards_method_generics_into_helper_calls() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn welcome<T>(&self, to: String) -> Mail
                    where
                        T: Default,
                    {
                        let _ = T::default();
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("self . welcome :: < T >"));
    }

    #[test]
    fn does_not_forward_lifetime_generics_into_helper_calls() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn welcome<'a>(&self, to: &'a str) -> Mail {
                        let _ = to;
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("async fn send_welcome < 'a >"));
        assert!(rendered.contains("self . welcome"));
        assert!(!rendered.contains("self . welcome :: < 'a >"));
    }

    #[test]
    fn uses_non_conflicting_generated_mailer_argument_name() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn welcome(&self, mailer: String) -> Mail {
                        let _ = mailer;
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("__autumn_mailer : & :: autumn_web :: mail :: Mailer"));
        assert!(rendered.contains("self . welcome (mailer ,)"));
        assert!(!rendered.contains("pub async fn send_welcome (& self , mailer : & :: autumn_web :: mail :: Mailer , mailer : String ,)"));
    }

    #[test]
    fn preserves_method_visibility_on_generated_helpers() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    pub(crate) fn welcome(&self, to: String) -> Mail {
                        let _ = to;
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("pub (crate) async fn send_welcome"));
        assert!(rendered.contains("pub (crate) fn deliver_later_welcome"));
        assert!(!rendered.contains("pub async fn send_welcome"));
    }

    #[test]
    fn skips_non_mail_returning_methods() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn helper(&self) -> String {
                        String::new()
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(!rendered.contains("send_helper"));
        assert!(!rendered.contains("deliver_later_helper"));
    }

    #[test]
    fn skips_foreign_mail_returning_methods() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn helper(&self) -> other_crate::Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(!rendered.contains("send_helper"));
        assert!(!rendered.contains("deliver_later_helper"));
    }

    #[test]
    fn supports_fully_qualified_autumn_mail_return_type() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn reset(&self, to: String) -> autumn_web::mail::Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("send_reset"));
        assert!(rendered.contains("deliver_later_reset"));
    }

    #[test]
    fn preserves_cfg_attributes_on_generated_helpers() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    #[cfg(feature = "welcome-mail")]
                    fn welcome(&self, to: String) -> Mail {
                        let _ = to;
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("# [cfg (feature = \"welcome-mail\")] async fn send_welcome"));
        assert!(rendered.contains("# [cfg (feature = \"welcome-mail\")] fn deliver_later_welcome"));
    }

    #[test]
    fn preserves_impl_cfg_attributes_on_generated_helper_impl() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                #[cfg(feature = "welcome-mail")]
                impl AccountMailer {
                    fn welcome(&self, to: String) -> Mail {
                        let _ = to;
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("# [cfg (feature = \"welcome-mail\")] impl AccountMailer"));
    }

    #[test]
    fn rejects_mutable_self_mail_templates() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    fn reset(&mut self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(rendered.contains("template methods must use an `&self` receiver"));
    }

    #[test]
    fn list_unsubscribe_tags_messages_and_registers_descriptor() {
        let out = mailer_macro(
            quote! { list_unsubscribe = "weekly_digest" },
            quote! {
                impl DigestMailer {
                    fn digest(&self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(
            rendered.contains("mail . list_unsubscribe ="),
            "send/later helpers must tag the message with the list scope: {rendered}"
        );
        assert!(rendered.contains("\"weekly_digest\""));
        assert!(
            rendered.contains("MailerListUnsubscribeDescriptor"),
            "must register an inventory descriptor: {rendered}"
        );
        assert!(rendered.contains("inventory :: submit"));
        assert!(rendered.contains("mailer : \"DigestMailer\""));
        // The original template method body is rewritten to tag its returned Mail
        // too, so direct calls (not just the helpers) are compliant.
        assert!(
            rendered.contains("__autumn_list_mail"),
            "template method body must tag its returned Mail: {rendered}"
        );
    }

    #[test]
    fn without_list_unsubscribe_leaves_template_body_untouched() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl PlainMailer {
                    fn ping(&self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(
            !rendered.contains("__autumn_list_mail"),
            "opt-out mailers must not rewrite the template body: {rendered}"
        );
    }

    #[test]
    fn without_list_unsubscribe_emits_no_scope_or_registration() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl PlainMailer {
                    fn ping(&self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(
            !rendered.contains("list_unsubscribe"),
            "opt-out mailers must not reference list_unsubscribe at all: {rendered}"
        );
        assert!(!rendered.contains("MailerListUnsubscribeDescriptor"));
        // Existing codegen unchanged: still binds an immutable `mail`.
        assert!(rendered.contains("let mail = self . ping"));
    }

    #[test]
    fn rejects_unknown_mailer_argument() {
        let out = mailer_macro(
            quote! { bogus = "x" },
            quote! {
                impl PlainMailer {
                    fn ping(&self, to: String) -> Mail {
                        panic!("template body is irrelevant to macro rendering test")
                    }
                }
            },
        );
        assert!(out.to_string().contains("unknown #[mailer] argument"));
    }

    #[test]
    fn ignores_async_non_mail_methods() {
        let out = mailer_macro(
            TokenStream::new(),
            quote! {
                impl AccountMailer {
                    async fn helper(&self) -> String {
                        String::new()
                    }
                }
            },
        );
        let rendered = out.to_string();
        assert!(!rendered.contains("send_helper"));
        assert!(!rendered.contains("deliver_later_helper"));
    }
}
