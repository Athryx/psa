use super::*;
use quote::quote;
use syn::{ImplItem, Item};

fn expand(options: TokenStream2, item: TokenStream2) -> Result<syn::File> {
    let output = expand_mailbox(syn::parse2(options)?, syn::parse2(item)?)?;
    syn::parse2(output)
}

fn rejection(options: TokenStream2, item: TokenStream2, expected: &str) {
    let error = match expand(options, item) {
        Ok(_) => panic!("expected rejection containing {expected:?}"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains(expected),
        "unexpected error: {error}"
    );
}

fn methods(file: &syn::File) -> Vec<&syn::ImplItemFn> {
    file.items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(item) if item.trait_.is_none() => Some(item.items.iter()),
            _ => None,
        })
        .flatten()
        .filter_map(|item| match item {
            ImplItem::Fn(method) => Some(method),
            _ => None,
        })
        .collect()
}

#[test]
fn bounded_client_expands_named_unit_and_nested_reply_results() {
    let file = expand(quote!(client = Client), quote! {
        pub(crate) enum Message {
            Notify { handle: usize, reply: oneshot::Sender<()> },
            Stop,
            HTTPRequest { value: String, #[reply] answer: ::tokio::sync::oneshot::Sender<Result<u32, String>> },
        }
    }).unwrap();
    let output = quote!(#file).to_string();
    let methods = methods(&file);
    assert_eq!(
        methods
            .iter()
            .map(|method| method.sig.ident.to_string())
            .collect::<Vec<_>>(),
        ["new", "notify", "stop", "http_request"]
    );
    assert!(
        methods[1..]
            .iter()
            .all(|method| method.sig.asyncness.is_some())
    );
    assert_eq!(
        methods[1].sig.inputs.len(),
        3,
        "unannotated sender must remain a normal parameter"
    );
    assert_eq!(methods[3].sig.inputs.len(), 2);
    assert!(output.contains(
        "self . handle . request (| answer | Message :: HTTPRequest { value , answer }) . await"
    ));
    assert!(output.contains("Result < Result < u32 , String > , :: actors :: MailboxError >"));
    assert!(output.contains("pub (crate) struct Client"));
    assert!(output.contains("pub (crate) async fn notify"));
    assert!(output.contains("impl :: core :: clone :: Clone for Client"));
    assert!(!output.contains("# [reply]"));
    assert!(!output.contains("derive (Clone)"));
    assert!(!output.contains("Message : Clone"));
}

#[test]
fn unbounded_notifications_are_sync_but_requests_are_async() {
    let file = expand(
        quote!(unbounded, client = Client,),
        quote! {
            enum Message {
                Stop,
                Notify { handle: usize },
                Ask { #[reply] handle: oneshot::Sender<bool> },
            }
        },
    )
    .unwrap();
    let output = quote!(#file).to_string();
    let methods = methods(&file);
    assert!(methods[1].sig.asyncness.is_none());
    assert!(methods[2].sig.asyncness.is_none());
    assert!(methods[3].sig.asyncness.is_some());
    assert!(output.contains(":: actors :: UnboundedMailboxHandle < Message >"));
    assert!(output.contains("self . handle . send (Message :: Notify { handle })"));
    assert!(
        output.contains("self . handle . request (| handle | Message :: Ask { handle }) . await")
    );
}

#[test]
fn preserves_variant_attributes_and_strips_only_reply_annotation() {
    let file = expand(
        quote!(client = Client),
        quote! {
            enum Message {
                #[cfg(feature = "optional")]
                Ping { #[allow(dead_code)] #[reply] reply: oneshot::Sender<()> },
                #[cfg_attr(feature = "optional", cfg(unix))]
                Stop,
            }
        },
    )
    .unwrap();
    let methods = methods(&file);
    assert!(methods[1].attrs[0].path().is_ident("cfg"));
    assert!(methods[2].attrs[0].path().is_ident("cfg_attr"));
    assert!(quote!(#file).to_string().contains("allow (dead_code)"));
}

#[test]
fn copies_variant_docs_to_bounded_and_unbounded_client_methods() {
    for options in [quote!(client = Client), quote!(client = Client, unbounded)] {
        let file = expand(
            options,
            quote! {
                enum Message {
                    /// Stop the actor.
                    ///
                    /// Queued messages are drained first.
                    Stop,
                    /** Update the current value. */
                    Notify { value: u64 },
                    #[cfg(unix)]
                    #[allow(dead_code)]
                    #[doc = "Read the current value."]
                    Ask { #[reply] reply: oneshot::Sender<u64> },
                    Undocumented,
                }
            },
        )
        .unwrap();
        let Item::Enum(item) = &file.items[0] else {
            panic!("expected original enum");
        };
        let methods = methods(&file);
        assert!(
            methods[0].attrs.is_empty(),
            "constructor has no variant docs"
        );
        for (variant, method) in item.variants.iter().zip(&methods[1..]) {
            let docs: Vec<_> = variant
                .attrs
                .iter()
                .filter(|attribute| attribute.path().is_ident("doc"))
                .collect();
            let copied: Vec<_> = method
                .attrs
                .iter()
                .filter(|attribute| attribute.path().is_ident("doc"))
                .collect();
            assert_eq!(
                quote!(#(#docs)*).to_string(),
                quote!(#(#copied)*).to_string(),
            );
            assert!(
                !method
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("allow"))
            );
        }
        assert_eq!(methods[1].attrs.len(), 3, "preserve multiline doc order");
        assert_eq!(methods[2].attrs.len(), 1, "preserve block doc comments");
        assert_eq!(methods[3].attrs.len(), 2, "preserve cfg and explicit docs");
        assert!(methods[4].attrs.is_empty());
    }
}

#[test]
fn accepts_keyword_methods_using_raw_identifiers() {
    let file = expand(
        quote!(client = Client),
        quote!(
            enum Message {
                Type,
            }
        ),
    )
    .unwrap();
    assert_eq!(methods(&file)[1].sig.ident.to_string(), "r#type");
}

#[test]
fn rejects_duplicate_reply_fields_and_annotations() {
    rejection(
        quote!(client = Client),
        quote! {
            enum Message { Ask { #[reply] one: oneshot::Sender<()>, #[reply] two: oneshot::Sender<()> } }
        },
        "at most one",
    );
    rejection(
        quote!(client = Client),
        quote! {
            enum Message { Ask { #[reply] #[reply] one: oneshot::Sender<()> } }
        },
        "at most one",
    );
    rejection(
        quote!(client = Client),
        quote! {
            enum Message { Ask { #[reply(value)] one: oneshot::Sender<()> } }
        },
        "bare `#[reply]`",
    );
}

#[test]
fn rejects_malformed_reply_types() {
    for ty in [
        quote!(String),
        quote!(Sender<()>),
        quote!(other::Sender<()>),
        quote!(oneshot::Sender),
        quote!(oneshot::Sender<(), bool>),
        quote!(oneshot::Sender<'static>),
        quote!(oneshot::Sender<3>),
        quote!(&oneshot::Sender<()>),
        quote!(oneshot::<()>::Sender<()>),
        quote!(<Thing as oneshot>::Sender<()>),
    ] {
        rejection(
            quote!(client = Client),
            quote! {
                enum Message { Ask { #[reply] reply: #ty } }
            },
            "oneshot::Sender<T>",
        );
    }
}

#[test]
fn rejects_tuple_variants_generics_and_conditional_fields() {
    rejection(
        quote!(client = Client),
        quote!(
            enum Message {
                Tuple(u8),
            }
        ),
        "tuple mailbox variants",
    );
    rejection(
        quote!(client = Client),
        quote!(
            enum Message<T> {
                Named { value: T },
            }
        ),
        "cannot have generics",
    );
    rejection(
        quote!(client = Client),
        quote!(
            enum Message<'a> {
                Named { value: &'a str },
            }
        ),
        "cannot have generics",
    );
    rejection(
        quote!(client = Client),
        quote!(
            enum Message<const N: usize> {
                Unit,
            }
        ),
        "cannot have generics",
    );
    rejection(
        quote!(client = Client),
        quote!(
            enum Message {
                Named {
                    #[cfg(unix)]
                    value: u8,
                },
            }
        ),
        "conditional mailbox fields",
    );
}

#[test]
fn rejects_invalid_options() {
    for (options, expected) in [
        (quote!(), "missing `client"),
        (quote!(unbounded), "missing `client"),
        (quote!(client = One, client = Two), "duplicate `client`"),
        (
            quote!(client = Client, unbounded, unbounded),
            "duplicate `unbounded`",
        ),
        (quote!(client = Client, unbounded = true), "is a flag"),
        (
            quote!(client = Client, capacity = 4),
            "unknown mailbox option",
        ),
        (quote!(client =), "expected identifier"),
    ] {
        rejection(
            options,
            quote!(
                enum Message {
                    Stop,
                }
            ),
            expected,
        );
    }
}

#[test]
fn rejects_generated_method_and_type_collisions() {
    rejection(
        quote!(client = Client),
        quote!(
            enum Message {
                New,
            }
        ),
        "reserved method `new`",
    );
    rejection(
        quote!(client = Client),
        quote!(
            enum Message {
                HttpRequest,
                HTTPRequest,
            }
        ),
        "duplicate method `http_request`",
    );
    rejection(
        quote!(client = Message),
        quote!(
            enum Message {
                Stop,
            }
        ),
        "must differ",
    );
    rejection(
        quote!(client = Client),
        quote!(
            enum Message {
                SELF,
            }
        ),
        "cannot be used as a client method name",
    );
}

#[test]
fn propagates_enum_gates_to_client_and_both_impls() {
    let file = expand(
        quote!(client = Client),
        quote! {
            #[cfg(unix)]
            #[cfg_attr(feature = "optional", cfg(any()), derive(Debug))]
            #[cfg_attr(test, cfg_attr(unix, cfg(any()), derive(Debug)))]
            #[cfg_attr(test, derive(Debug))]
            enum Message { Stop }
        },
    )
    .unwrap();
    assert_eq!(file.items.len(), 4);
    for item in &file.items[1..] {
        let attributes = match item {
            Item::Struct(item) => &item.attrs,
            Item::Impl(item) => &item.attrs,
            _ => panic!("unexpected generated item"),
        };
        assert_eq!(attributes.len(), 3);
        assert!(attributes[0].path().is_ident("cfg"));
        let output = quote!(#(#attributes)*).to_string();
        assert!(output.contains("cfg_attr"));
        assert!(output.contains("cfg (any ())"));
        assert!(!output.contains("derive"));
    }
}

#[test]
fn rejects_self_in_field_and_reply_types() {
    for ty in [
        quote!(Self),
        quote!(Box<Self>),
        quote!(<Self as Trait>::Item),
        quote!(fn() -> Self),
    ] {
        rejection(
            quote!(client = Client),
            quote! {
                enum Message { Notify { value: #ty } }
            },
            "`Self` in mailbox field types is unsupported",
        );
    }
    rejection(
        quote!(client = Client),
        quote! {
            enum Message { Ask { #[reply] reply: oneshot::Sender<Option<Box<Self>>> } }
        },
        "`Self` in mailbox field types is unsupported",
    );
}
