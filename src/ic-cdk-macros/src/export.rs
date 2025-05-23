use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};
use serde::Deserialize;
use serde_tokenstream::from_tokenstream;
use std::fmt::Formatter;
use syn::Error;
use syn::{spanned::Spanned, FnArg, ItemFn, Pat, PatIdent, PatType, ReturnType, Signature, Type};

#[derive(Default, Deserialize)]
struct ExportAttributes {
    pub name: Option<String>,
    pub guard: Option<String>,
    #[serde(default)]
    pub manual_reply: bool,
    #[serde(default)]
    pub composite: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub decoding_quota: Option<usize>,
    #[serde(default = "default_skipping_quota")]
    pub skipping_quota: Option<usize>,
    #[serde(default)]
    pub debug: bool,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MethodType {
    Init,
    PreUpgrade,
    PostUpgrade,
    Update,
    Query,
    Heartbeat,
    InspectMessage,
    OnLowWasmMemory,
}

impl MethodType {
    pub fn is_lifecycle(&self) -> bool {
        match self {
            MethodType::Init
            | MethodType::PreUpgrade
            | MethodType::PostUpgrade
            | MethodType::Heartbeat
            | MethodType::InspectMessage
            | MethodType::OnLowWasmMemory => true,
            MethodType::Update | MethodType::Query => false,
        }
    }
}

impl std::fmt::Display for MethodType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            MethodType::Init => f.write_str("init"),
            MethodType::PreUpgrade => f.write_str("pre_upgrade"),
            MethodType::PostUpgrade => f.write_str("post_upgrade"),
            MethodType::Query => f.write_str("query"),
            MethodType::Update => f.write_str("update"),
            MethodType::Heartbeat => f.write_str("heartbeat"),
            MethodType::InspectMessage => f.write_str("inspect_message"),
            MethodType::OnLowWasmMemory => f.write_str("on_low_wasm_memory"),
        }
    }
}

fn default_skipping_quota() -> Option<usize> {
    Some(10_000)
}

fn get_args(method: MethodType, signature: &Signature) -> Result<Vec<(Ident, Box<Type>)>, Error> {
    // We only need the tuple of arguments, not their types. Magic of type inference.
    let mut args = vec![];
    for (i, arg) in signature.inputs.iter().enumerate() {
        let (ident, ty) = match arg {
            FnArg::Receiver(r) => {
                return Err(Error::new(
                    r.span(),
                    format!(
                        "#[{}] cannot be above functions with `self` as a parameter.",
                        method
                    ),
                ));
            }
            FnArg::Typed(PatType { pat, ty, .. }) => {
                if let Pat::Ident(PatIdent { ident, .. }) = pat.as_ref() {
                    (ident.clone(), ty.clone())
                } else {
                    (
                        format_ident!("__unnamed_arg_{i}", span = pat.span()),
                        ty.clone(),
                    )
                }
            }
        };

        args.push((ident, ty));
    }

    Ok(args)
}

fn dfn_macro(
    method: MethodType,
    attr: TokenStream,
    item: TokenStream,
) -> Result<TokenStream, Error> {
    let attrs = from_tokenstream::<ExportAttributes>(&attr)
        .map_err(|e| Error::new(attr.span(), format!("Failed to deserialize {attr}. \n{e}")))?;

    let fun: ItemFn = syn::parse2::<syn::ItemFn>(item.clone()).map_err(|e| {
        Error::new(
            item.span(),
            format!("#[{0}] must be above a function. \n{1}", method, e),
        )
    })?;
    let signature = &fun.sig;
    let generics = &signature.generics;

    if !generics.params.is_empty() {
        return Err(Error::new(
            generics.span(),
            format!(
                "#[{}] must be above a function with no generic parameters.",
                method
            ),
        ));
    }

    let is_async = signature.asyncness.is_some();

    let return_length = match &signature.output {
        ReturnType::Default => 0,
        ReturnType::Type(_, ty) => match ty.as_ref() {
            Type::Tuple(tuple) => tuple.elems.len(),
            _ => 1,
        },
    };

    if method.is_lifecycle() && return_length > 0 {
        return Err(Error::new(
            Span::call_site(),
            format!("#[{}] function cannot have a return value.", method),
        ));
    }

    let (arg_tuple, _): (Vec<Ident>, Vec<Box<Type>>) =
        get_args(method, signature)?.iter().cloned().unzip();
    let name = &signature.ident;

    let outer_function_ident = format_ident!("__canister_method_{name}");

    let function_name = attrs.name.unwrap_or_else(|| name.to_string());
    let export_name = if method.is_lifecycle() {
        format!("canister_{}", method)
    } else if method == MethodType::Query && attrs.composite {
        format!("canister_composite_query {function_name}",)
    } else {
        if function_name.starts_with("<ic-cdk internal>") {
            return Err(Error::new(
                Span::call_site(),
                "Functions starting with `<ic-cdk internal>` are reserved for CDK internal use.",
            ));
        }
        format!("canister_{method} {function_name}")
    };
    let host_compatible_name = export_name.replace(' ', ".").replace(['-', '<', '>'], "_");

    let function_call = if is_async {
        quote! { #name ( #(#arg_tuple),* ) .await }
    } else {
        quote! { #name ( #(#arg_tuple),* ) }
    };

    let arg_count = arg_tuple.len();

    let return_encode = if method.is_lifecycle() || attrs.manual_reply {
        quote! {}
    } else {
        match return_length {
            0 => quote! { ic_cdk::api::call::reply(()) },
            1 => quote! { ic_cdk::api::call::reply((result,)) },
            _ => quote! { ic_cdk::api::call::reply(result) },
        }
    };

    // On initialization we can actually not receive any input and it's okay, only if
    // we don't have any arguments either.
    // If the data we receive is not empty, then try to unwrap it as if it's DID.
    let arg_decode = if method.is_lifecycle() && arg_count == 0 {
        quote! {}
    } else {
        let decoding_quota = if let Some(n) = attrs.decoding_quota {
            quote! { Some(#n) }
        } else {
            quote! { None }
        };
        let skipping_quota = if let Some(n) = attrs.skipping_quota {
            quote! { Some(#n) }
        } else {
            quote! { None }
        };
        let debug = if attrs.debug {
            quote! { true }
        } else {
            quote! { false }
        };
        let config = quote! {
            ic_cdk::api::call::ArgDecoderConfig {
                decoding_quota: #decoding_quota,
                skipping_quota: #skipping_quota,
                debug: #debug,
            }
        };
        quote! { let ( #( #arg_tuple, )* ) = ic_cdk::api::call::arg_data(#config); }
    };

    let guard = if let Some(guard_name) = attrs.guard {
        // ic_cdk::api::call::reject calls ic0::msg_reject which is only allowed in update/query
        if method.is_lifecycle() {
            return Err(Error::new(
                attr.span(),
                format!("#[{}] cannot have a guard function.", method),
            ));
        }
        let guard_ident = syn::Ident::new(&guard_name, Span::call_site());

        quote! {
            let r: Result<(), String> = #guard_ident ();
            if let Err(e) = r {
                ic_cdk::api::call::reject(&e);
                return;
            }
        }
    } else {
        quote! {}
    };

    let candid_method_attr = if attrs.hidden {
        quote! {}
    } else {
        match method {
            MethodType::Query if attrs.composite => {
                quote! { #[::candid::candid_method(composite_query, rename = #function_name)] }
            }
            MethodType::Query => {
                quote! { #[::candid::candid_method(query, rename = #function_name)] }
            }
            MethodType::Update => {
                quote! { #[::candid::candid_method(update, rename = #function_name)] }
            }
            MethodType::Init => quote! { #[::candid::candid_method(init)] },
            _ => quote! {},
        }
    };
    let item = quote! {
        #candid_method_attr
        #item
    };

    Ok(quote! {
        #[cfg_attr(target_family = "wasm", export_name = #export_name)]
        #[cfg_attr(not(target_family = "wasm"), export_name = #host_compatible_name)]
        fn #outer_function_ident() {
            ic_cdk::setup();

            #guard

            ic_cdk::spawn(async {
                #arg_decode
                let result = #function_call;
                #return_encode
            });
        }

        #item
    })
}

pub(crate) fn ic_query(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::Query, attr, item)
}

pub(crate) fn ic_update(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::Update, attr, item)
}

#[derive(Default, Deserialize)]
struct InitAttributes {}

pub(crate) fn ic_init(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::Init, attr, item)
}

pub(crate) fn ic_pre_upgrade(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::PreUpgrade, attr, item)
}

pub(crate) fn ic_post_upgrade(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::PostUpgrade, attr, item)
}

pub(crate) fn ic_heartbeat(attr: TokenStream, item: TokenStream) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::Heartbeat, attr, item)
}

pub(crate) fn ic_inspect_message(
    attr: TokenStream,
    item: TokenStream,
) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::InspectMessage, attr, item)
}

pub(crate) fn ic_on_low_wasm_memory(
    attr: TokenStream,
    item: TokenStream,
) -> Result<TokenStream, Error> {
    dfn_macro(MethodType::OnLowWasmMemory, attr, item)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn ic_query_empty() {
        let generated = ic_query(
            quote!(),
            quote! {
                fn query() {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let () = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query();
                    ic_cdk::api::call::reply(())
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }
    #[test]
    fn ic_query_return_one_value() {
        let generated = ic_query(
            quote!(),
            quote! {
                fn query() -> u32 {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let () = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query();
                    ic_cdk::api::call::reply((result,))
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }

    #[test]
    fn ic_query_return_tuple() {
        let generated = ic_query(
            quote!(),
            quote! {
                fn query() -> (u32, u32) {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let () = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query();
                    ic_cdk::api::call::reply(result)
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }

    #[test]
    fn ic_query_one_arg() {
        let generated = ic_query(
            quote!(),
            quote! {
                fn query(a: u32) {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let (a, ) = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query(a);
                    ic_cdk::api::call::reply(())
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }

    #[test]
    fn ic_query_two_args() {
        let generated = ic_query(
            quote!(),
            quote! {
                fn query(a: u32, b: u32) {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let (a, b, ) = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query(a, b);
                    ic_cdk::api::call::reply(())
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }

    #[test]
    fn ic_query_two_args_return_value() {
        let generated = ic_query(
            quote!(),
            quote! {
                fn query(a: u32, b: u32) -> u64 {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let (a, b, ) = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query(a, b);
                    ic_cdk::api::call::reply((result,))
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }

    #[test]
    fn ic_query_export_name() {
        let generated = ic_query(
            quote!(name = "custom_query"),
            quote! {
                fn query() {}
            },
        )
        .unwrap();
        let parsed = syn::parse2::<syn::File>(generated).unwrap();
        let fn_name = match parsed.items[0] {
            syn::Item::Fn(ref f) => &f.sig.ident,
            _ => panic!("Incorrect parsed AST."),
        };

        let expected = quote! {
            #[cfg_attr(target_family = "wasm", export_name = "canister_query custom_query")]
            #[cfg_attr(not(target_family = "wasm"), export_name = "canister_query.custom_query")]
            fn #fn_name() {
                ic_cdk::setup();
                ic_cdk::spawn(async {
                    let () = ic_cdk::api::call::arg_data(
                        ic_cdk::api::call::ArgDecoderConfig {
                            decoding_quota: None,
                            skipping_quota: Some(10000usize),
                            debug: false,
                        }
                    );
                    let result = query();
                    ic_cdk::api::call::reply(())
                });
            }
        };
        let expected = syn::parse2::<syn::ItemFn>(expected).unwrap();

        assert!(parsed.items.len() == 2);
        match &parsed.items[0] {
            syn::Item::Fn(f) => {
                assert_eq!(*f, expected);
            }
            _ => panic!("not a function"),
        };
    }
}
