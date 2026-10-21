//! Typed mailbox clients for the `actors` runtime.

use std::collections::HashSet;

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Error, Fields, GenericArgument, Ident, ItemEnum, PathArguments, Result, Token, Type,
};

/// Generate a typed client for a nongeneric mailbox enum.
///
/// Use `#[mailbox(client = MyClient)]`, optionally with `unbounded`. Annotate a
/// single `oneshot::Sender<T>` field per request variant with `#[reply]`.
/// Variant doc comments are copied to the corresponding client methods.
#[proc_macro_attribute]
pub fn mailbox(options: TokenStream, item: TokenStream) -> TokenStream {
    let result = syn::parse(options)
        .and_then(|options| syn::parse(item).and_then(|item| expand_mailbox(options, item)));
    result.unwrap_or_else(Error::into_compile_error).into()
}

struct Options {
    client: Ident,
    unbounded: bool,
}

impl Parse for Options {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut client = None;
        let mut unbounded = false;

        while !input.is_empty() {
            let option: Ident = input.parse()?;
            match option.to_string().as_str() {
                "client" => {
                    if client.is_some() {
                        return Err(Error::new(option.span(), "duplicate `client` option"));
                    }
                    input.parse::<Token![=]>()?;
                    client = Some(input.parse::<Ident>()?);
                }
                "unbounded" => {
                    if unbounded {
                        return Err(Error::new(option.span(), "duplicate `unbounded` flag"));
                    }
                    if input.peek(Token![=]) {
                        return Err(Error::new(
                            option.span(),
                            "`unbounded` is a flag, not a value",
                        ));
                    }
                    unbounded = true;
                }
                _ => {
                    return Err(Error::new(
                        option.span(),
                        "unknown mailbox option; expected `client = ClientName` or `unbounded`",
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        let client = client.ok_or_else(|| input.error("missing `client = ClientName` option"))?;
        Ok(Self { client, unbounded })
    }
}

fn expand_mailbox(options: Options, mut item: ItemEnum) -> Result<TokenStream2> {
    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
        return Err(Error::new_spanned(
            &item.generics,
            "mailbox enums cannot have generics or a where clause",
        ));
    }
    if options.client == item.ident {
        return Err(Error::new_spanned(
            &options.client,
            "mailbox client name must differ from the enum name",
        ));
    }

    let enum_attributes = conditional_attributes(&item.attrs)?;
    let enum_name = &item.ident;
    let visibility = &item.vis;
    let client = &options.client;
    let handle = if options.unbounded {
        quote!(::actors::UnboundedMailboxHandle<#enum_name>)
    } else {
        quote!(::actors::MailboxHandle<#enum_name>)
    };
    let mut methods = Vec::new();
    let mut method_names = HashSet::new();

    for variant in &mut item.variants {
        let name = &variant.ident;
        let method_name = name.to_string().trim_start_matches("r#").to_snake_case();
        if method_name == "new" {
            return Err(Error::new_spanned(
                name,
                "mailbox variant generates reserved method `new`",
            ));
        }
        if !method_names.insert(method_name.clone()) {
            return Err(Error::new_spanned(
                name,
                format!("mailbox variants generate duplicate method `{method_name}`"),
            ));
        }
        let method: Ident = syn::parse_str(&method_name)
            .or_else(|_| syn::parse_str(&format!("r#{method_name}")))
            .map_err(|_| {
                Error::new_spanned(name, "variant cannot be used as a client method name")
            })?;
        let mut parameters = Vec::new();
        let mut field_names = Vec::new();
        let mut reply = None;

        match &mut variant.fields {
            Fields::Named(fields) => {
                for field in &mut fields.named {
                    if field.attrs.iter().any(is_conditional_attribute) {
                        return Err(Error::new_spanned(
                            field,
                            "conditional mailbox fields (`cfg`/`cfg_attr`) are unsupported; gate the entire variant instead",
                        ));
                    }
                    let field_type = &field.ty;
                    reject_self_type(quote!(#field_type))?;
                    let field_name = field.ident.as_ref().expect("named field");
                    field_names.push(field_name.clone());
                    let mut is_reply = false;
                    for attribute in &field.attrs {
                        if !attribute.path().is_ident("reply") {
                            continue;
                        }
                        if is_reply || reply.is_some() {
                            return Err(Error::new_spanned(
                                attribute,
                                "at most one `#[reply]` field is allowed per variant",
                            ));
                        }
                        if !matches!(attribute.meta, syn::Meta::Path(_)) {
                            return Err(Error::new_spanned(
                                attribute,
                                "expected bare `#[reply]` annotation",
                            ));
                        }
                        is_reply = true;
                    }
                    if is_reply {
                        reply = Some((field_name.clone(), parse_reply_type(&field.ty)?));
                    } else {
                        let ty = &field.ty;
                        parameters.push(quote!(#field_name: #ty));
                    }
                    field
                        .attrs
                        .retain(|attribute| !attribute.path().is_ident("reply"));
                }
            }
            Fields::Unit => {}
            Fields::Unnamed(_) => {
                return Err(Error::new_spanned(
                    &variant.fields,
                    "tuple mailbox variants are unsupported; use named fields or a unit variant",
                ));
            }
        }

        let mut attributes = conditional_attributes(&variant.attrs)?;
        attributes.extend(
            variant
                .attrs
                .iter()
                .filter(|attribute| attribute.path().is_ident("doc"))
                .cloned(),
        );
        let message = match variant.fields {
            Fields::Unit => quote!(#enum_name::#name),
            _ => quote!(#enum_name::#name { #(#field_names),* }),
        };
        let method = if let Some((reply_name, reply_type)) = reply {
            quote! {
                #(#attributes)*
                #visibility async fn #method(&self, #(#parameters),*) -> ::core::result::Result<#reply_type, ::actors::MailboxError> {
                    self.handle.request(|#reply_name| #message).await
                }
            }
        } else if options.unbounded {
            quote! {
                #(#attributes)*
                #visibility fn #method(&self, #(#parameters),*) -> ::core::result::Result<(), ::actors::MailboxError> {
                    self.handle.send(#message)
                }
            }
        } else {
            quote! {
                #(#attributes)*
                #visibility async fn #method(&self, #(#parameters),*) -> ::core::result::Result<(), ::actors::MailboxError> {
                    self.handle.send(#message).await
                }
            }
        };
        methods.push(method);
    }

    Ok(quote! {
        #item

        #(#enum_attributes)*
        #visibility struct #client {
            handle: #handle,
        }

        #(#enum_attributes)*
        impl ::core::clone::Clone for #client {
            fn clone(&self) -> Self {
                Self { handle: ::core::clone::Clone::clone(&self.handle) }
            }
        }

        #(#enum_attributes)*
        impl #client {
            #visibility fn new(handle: #handle) -> Self {
                Self { handle }
            }

            #(#methods)*
        }
    })
}

fn is_conditional_attribute(attribute: &Attribute) -> bool {
    attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr")
}

fn conditional_attributes(attributes: &[Attribute]) -> Result<Vec<Attribute>> {
    let mut conditional = Vec::new();
    for attribute in attributes {
        if let Some(meta) = conditional_meta(&attribute.meta)? {
            conditional.push(syn::parse_quote!(#[#meta]));
        }
    }
    Ok(conditional)
}

fn conditional_meta(meta: &syn::Meta) -> Result<Option<syn::Meta>> {
    if meta.path().is_ident("cfg") {
        return Ok(Some(meta.clone()));
    }
    if !meta.path().is_ident("cfg_attr") {
        return Ok(None);
    }

    let syn::Meta::List(list) = meta else {
        return Err(Error::new_spanned(
            meta,
            "expected `cfg_attr(predicate, attributes...)`",
        ));
    };
    let arguments = list.parse_args_with(Punctuated::<syn::Meta, Token![,]>::parse_terminated)?;
    let mut arguments = arguments.iter();
    let predicate = arguments
        .next()
        .ok_or_else(|| Error::new_spanned(meta, "missing cfg_attr predicate"))?;
    let mut nested = Vec::new();
    for argument in arguments {
        if let Some(meta) = conditional_meta(argument)? {
            nested.push(meta);
        }
    }
    if nested.is_empty() {
        return Ok(None);
    }
    Ok(Some(syn::parse_quote!(cfg_attr(#predicate, #(#nested),*))))
}

fn reject_self_type(tokens: TokenStream2) -> Result<()> {
    for token in tokens {
        match token {
            TokenTree::Ident(ident) if ident == "Self" => {
                return Err(Error::new(
                    ident.span(),
                    "`Self` in mailbox field types is unsupported; use the enum name explicitly",
                ));
            }
            TokenTree::Group(group) => reject_self_type(group.stream())?,
            _ => {}
        }
    }
    Ok(())
}

fn parse_reply_type(ty: &Type) -> Result<Type> {
    let invalid = || {
        Error::new_spanned(
            ty,
            "`#[reply]` requires `oneshot::Sender<T>` with exactly one type argument",
        )
    };
    let Type::Path(path) = ty else {
        return Err(invalid());
    };
    if path.qself.is_some() || path.path.segments.len() < 2 {
        return Err(invalid());
    }
    let mut segments = path.path.segments.iter().rev();
    let sender = segments.next().expect("two segments");
    let oneshot = segments.next().expect("two segments");
    if sender.ident != "Sender" || oneshot.ident != "oneshot" {
        return Err(invalid());
    }
    if path
        .path
        .segments
        .iter()
        .rev()
        .skip(1)
        .any(|segment| !matches!(segment.arguments, PathArguments::None))
    {
        return Err(invalid());
    }
    let PathArguments::AngleBracketed(arguments) = &sender.arguments else {
        return Err(invalid());
    };
    if arguments.args.len() != 1 {
        return Err(invalid());
    }
    match arguments.args.first() {
        Some(GenericArgument::Type(reply_type)) => Ok(reply_type.clone()),
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests;
