//! `agent-macros`: the `#[tool]` and `#[agent_test]` (re-exported by the facade as
//! `#[agent::test]`) attribute macros.
//!
//! Generated code only references items under `<agent_tools>::__private`, where
//! `<agent_tools>` is resolved with `proc-macro-crate`: `::agent_tools` when the
//! user depends on `agent-tools` (or is `agent-tools` itself, which declares
//! `extern crate self as agent_tools`), otherwise `::agent::tools` when the user
//! depends on the facade crate `agent`.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use proc_macro_crate::{crate_name, FoundCrate};
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, spanned::Spanned, FnArg, GenericArgument, ItemFn, LitStr, Pat,
    PathArguments, ReturnType, Type,
};

/// Path to the `agent-tools` crate as seen from the calling crate.
fn tools_crate() -> TokenStream2 {
    match crate_name("agent-tools") {
        Ok(FoundCrate::Itself) => quote!(::agent_tools),
        Ok(FoundCrate::Name(n)) => {
            let id = format_ident!("{}", n);
            quote!(::#id)
        }
        Err(_) => match crate_name("agent") {
            Ok(FoundCrate::Itself) => quote!(crate::tools),
            Ok(FoundCrate::Name(n)) => {
                let id = format_ident!("{}", n);
                quote!(::#id::tools)
            }
            Err(_) => quote!(::agent_tools),
        },
    }
}

const CAPS: &[&str] = &["Read", "Write", "Get", "Net", "Exec", "Secret", "Mem"];

fn last_segment(ty: &Type) -> Option<&syn::PathSegment> {
    match ty {
        Type::Path(p) if p.qself.is_none() => p.path.segments.last(),
        Type::Group(g) => last_segment(&g.elem),
        Type::Paren(p) => last_segment(&p.elem),
        _ => None,
    }
}

/// `Option<T>` -> `Some(T)`.
fn option_inner(ty: &Type) -> Option<&Type> {
    let seg = last_segment(ty)?;
    if seg.ident != "Option" {
        return None;
    }
    match &seg.arguments {
        PathArguments::AngleBracketed(a) if a.args.len() == 1 => match a.args.first() {
            Some(GenericArgument::Type(t)) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

fn is_cap_type(ty: &Type) -> bool {
    match last_segment(ty) {
        Some(seg) => {
            CAPS.iter().any(|c| seg.ident == c)
                && matches!(seg.arguments, PathArguments::AngleBracketed(_))
        }
        None => false,
    }
}

fn is_ctx_type(ty: &Type) -> Option<bool> {
    // Some(true) = by reference, Some(false) = by value.
    match ty {
        Type::Reference(r) => match last_segment(&r.elem) {
            Some(seg) if seg.ident == "ToolCtx" => Some(true),
            _ => None,
        },
        _ => match last_segment(ty) {
            Some(seg) if seg.ident == "ToolCtx" => Some(false),
            _ => None,
        },
    }
}

enum ParamKind {
    Ctx { by_ref: bool },
    Cap { optional: bool },
    Plain { optional: bool },
}

struct Param {
    ident: syn::Ident,
    ty: Type,
    kind: ParamKind,
}

fn doc_string(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for a in attrs {
        if !a.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &a.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                let v = s.value();
                lines.push(v.strip_prefix(' ').unwrap_or(&v).to_string());
            }
        }
    }
    lines.join("\n").trim().to_string()
}

/// Turns an `async fn` into a tool.
///
/// ```ignore
/// /// Replace the unique occurrence of `old` with `new`.
/// #[tool]
/// pub async fn edit(file: Write<File>, old: String, new: String) -> Result<()> {
///     file.replace_once(&old, &new).await
/// }
/// ```
///
/// generates `pub struct edit;` (unit struct, usable as a value) implementing
/// `Tool`. Parameters:
/// - capability types (`Read<..>`, `Write<..>`, `Get<..>`, `Net<..>`, `Exec<..>`,
///   `Secret<..>`, `Mem<..>`, or any type marked `#[cap]` implementing
///   `Capability`) contribute their schema fragment and access declaration, and
///   are bound to the call's grants before the body runs;
/// - `ctx: &ToolCtx` / `ctx: ToolCtx` is injected and not part of the schema;
/// - other parameters use `schemars::JsonSchema` + `serde::Deserialize`;
///   `Option<T>` parameters are optional.
///
/// Attribute arguments: `name = "..."`, `description = "..."` (defaults: the fn
/// name and its doc comment).
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut name_override: Option<LitStr> = None;
    let mut desc_override: Option<LitStr> = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            name_override = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("description") {
            desc_override = Some(meta.value()?.parse()?);
            Ok(())
        } else {
            Err(meta.error("unsupported #[tool] argument (expected `name` or `description`)"))
        }
    });
    parse_macro_input!(attr with parser);
    let func = parse_macro_input!(item as ItemFn);
    match expand_tool(func, name_override, desc_override) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_tool(
    mut func: ItemFn,
    name_override: Option<LitStr>,
    desc_override: Option<LitStr>,
) -> syn::Result<TokenStream2> {
    let krate = tools_crate();
    if func.sig.asyncness.is_none() {
        return Err(syn::Error::new(
            func.sig.fn_token.span(),
            "#[tool] requires an `async fn`",
        ));
    }
    if !func.sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            func.sig.generics.span(),
            "#[tool] functions cannot be generic",
        ));
    }
    if matches!(func.sig.output, ReturnType::Default) {
        return Err(syn::Error::new(
            func.sig.span(),
            "#[tool] functions must return a `Result<T, E>`",
        ));
    }
    let fn_name = func.sig.ident.clone();
    let tool_name = name_override.map(|l| l.value()).unwrap_or_else(|| {
        let s = fn_name.to_string();
        s.strip_prefix("r#").unwrap_or(&s).to_string()
    });
    let description = desc_override
        .map(|l| l.value())
        .unwrap_or_else(|| doc_string(&func.attrs));

    let mut params = Vec::new();
    for arg in func.sig.inputs.iter_mut() {
        let pt = match arg {
            FnArg::Typed(pt) => pt,
            FnArg::Receiver(r) => {
                return Err(syn::Error::new(
                    r.span(),
                    "#[tool] functions cannot take self",
                ))
            }
        };
        let ident = match &*pt.pat {
            Pat::Ident(pi) => pi.ident.clone(),
            other => {
                return Err(syn::Error::new(
                    other.span(),
                    "#[tool] parameters must be simple identifiers",
                ))
            }
        };
        let marked_cap = pt.attrs.iter().any(|a| a.path().is_ident("cap"));
        pt.attrs.retain(|a| !a.path().is_ident("cap"));
        let ty = (*pt.ty).clone();
        let kind = if let Some(by_ref) = is_ctx_type(&ty) {
            ParamKind::Ctx { by_ref }
        } else if let Some(inner) = option_inner(&ty) {
            if marked_cap || is_cap_type(inner) {
                ParamKind::Cap { optional: true }
            } else {
                ParamKind::Plain { optional: true }
            }
        } else if marked_cap || is_cap_type(&ty) {
            ParamKind::Cap { optional: false }
        } else {
            ParamKind::Plain { optional: false }
        };
        params.push(Param { ident, ty, kind });
    }

    let p = quote!(#krate::__private);
    let serde_crate = LitStr::new(&format!("{}::__private::serde", krate), Span::call_site());

    // __Args fields (everything except the ctx).
    let mut fields = Vec::new();
    let mut schema_stmts = Vec::new();
    let mut access_stmts = Vec::new();
    let mut bind_stmts = Vec::new();
    let mut call_args = Vec::new();
    let mut class_items = Vec::new();
    for prm in &params {
        let id = &prm.ident;
        let ty = &prm.ty;
        let key = {
            let s = id.to_string();
            s.strip_prefix("r#").unwrap_or(&s).to_string()
        };
        match prm.kind {
            ParamKind::Ctx { by_ref } => {
                if by_ref {
                    call_args.push(quote!(&__ctx));
                } else {
                    call_args.push(quote!(::core::clone::Clone::clone(&__ctx)));
                }
            }
            ParamKind::Cap { optional } => {
                fields.push(quote!(#id: #ty));
                schema_stmts.push(quote! {
                    __props.insert(::std::string::String::from(#key), <#ty as #p::Capability>::schema());
                });
                if !optional {
                    schema_stmts.push(quote!(__required.push(#key);));
                }
                access_stmts.push(quote! {
                    __acc.extend(#p::Capability::access(&__args.#id, __actx)?);
                });
                let local = format_ident!("__cap_{}", key);
                bind_stmts.push(quote! {
                    let mut #local = __args.#id;
                    #p::Capability::bind(&mut #local, &__ctx, &__obs)?;
                });
                call_args.push(quote!(#local));
                class_items.push(quote!(<#ty as #p::Capability>::CLASS));
            }
            ParamKind::Plain { optional } => {
                fields.push(quote!(#id: #ty));
                if optional {
                    let inner = option_inner(ty).expect("checked");
                    schema_stmts.push(quote! {
                        __props.insert(::std::string::String::from(#key), #p::schema_for::<#inner>(&mut __gen));
                    });
                } else {
                    schema_stmts.push(quote! {
                        __props.insert(::std::string::String::from(#key), #p::schema_for::<#ty>(&mut __gen));
                        __required.push(#key);
                    });
                }
                call_args.push(quote!(__args.#id));
            }
        }
    }

    let access_body = if access_stmts.is_empty() {
        quote! {
            let _: __Args = #p::parse_args(::core::clone::Clone::clone(__input))?;
            ::core::result::Result::Ok(::std::vec::Vec::new())
        }
    } else {
        quote! {
            let __args: __Args = #p::parse_args(::core::clone::Clone::clone(__input))?;
            let mut __acc = ::std::vec::Vec::new();
            #(#access_stmts)*
            ::core::result::Result::Ok(__acc)
        }
    };

    let vis = &func.vis;
    let doc_attrs: Vec<_> = func
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .cloned()
        .collect();
    let other_attrs: Vec<_> = func
        .attrs
        .iter()
        .filter(|a| !a.path().is_ident("doc"))
        .cloned()
        .collect();
    let inputs = &func.sig.inputs;
    let output = &func.sig.output;
    let block = &func.block;
    let body_ident = format_ident!("__tool_body");

    Ok(quote! {
        #(#doc_attrs)*
        #[allow(non_camel_case_types)]
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        #vis struct #fn_name;

        const _: () = {
            #[derive(#p::serde::Deserialize)]
            #[serde(crate = #serde_crate)]
            #[allow(non_snake_case, dead_code)]
            struct __Args { #(#fields,)* }

            impl #fn_name {
                /// Side-effect class derived from the parameter types.
                pub const CLASS: #p::agent_proto::EffectClass = #p::max_class(&[#(#class_items),*]);

                #(#other_attrs)*
                #[allow(clippy::too_many_arguments)]
                async fn #body_ident(#inputs) #output #block
            }

            impl #p::agent_runtime::ToolName for #fn_name {
                fn tool_name(&self) -> ::std::string::String {
                    <Self as #p::agent_runtime::Tool>::spec(self).name
                }
            }

            #[#p::async_trait::async_trait]
            impl #p::agent_runtime::Tool for #fn_name {
                fn spec(&self) -> #p::agent_proto::ToolSpec {
                    #[allow(unused_mut, unused_variables)]
                    let mut __gen = #p::schema_generator();
                    #[allow(unused_mut)]
                    let mut __props = #p::serde_json::Map::new();
                    #[allow(unused_mut)]
                    let mut __required: ::std::vec::Vec<&'static str> = ::std::vec::Vec::new();
                    #(#schema_stmts)*
                    #p::agent_proto::ToolSpec {
                        name: ::std::string::String::from(#tool_name),
                        description: ::std::string::String::from(#description),
                        input_schema: #p::object_schema(__props, &__required),
                        class: Self::CLASS,
                        subagent: false,
                    }
                }

                fn access(
                    &self,
                    __input: &#p::serde_json::Value,
                    __actx: &#p::agent_runtime::AccessCtx,
                ) -> ::core::result::Result<::std::vec::Vec<#p::agent_proto::Access>, #p::agent_runtime::ToolError> {
                    #access_body
                }

                fn class(&self, _input: &#p::serde_json::Value) -> #p::agent_proto::EffectClass {
                    Self::CLASS
                }

                async fn call(
                    &self,
                    __input: #p::serde_json::Value,
                    __ctx: #p::agent_runtime::ToolCtx,
                ) -> ::core::result::Result<#p::agent_runtime::ToolOutput, #p::agent_runtime::ToolError> {
                    #[allow(unused_variables)]
                    let __args: __Args = #p::parse_args(__input)?;
                    #[allow(unused_variables)]
                    let __obs = #p::Observations::default();
                    #(#bind_stmts)*
                    let __cancel = ::core::clone::Clone::clone(&__ctx.cancel);
                    let __ret = #p::with_cancel(&__cancel, Self::#body_ident(#(#call_args),*)).await?;
                    let __value = #p::ToolReturn::into_tool_result(__ret)?;
                    #[allow(unused_imports)]
                    use #p::{OutputViaSerialize as _, OutputViaSpecific as _};
                    let mut __out = (&#p::OutWrap(__value)).__into_tool_output()?;
                    __out.observed.extend(__obs.take());
                    ::core::result::Result::Ok(__out)
                }
            }
        };
    })
}

/// Wraps an async test in a current-thread tokio runtime with a fresh temporary
/// workspace (re-exported by the facade as `#[agent::test]`).
///
/// The workspace directory is created before the body runs, is reachable from
/// inside the test through `agent_tools::testing::workspace()` (a task-local),
/// and is deleted after the test. The clock is the real tokio clock (a paused
/// clock would make subprocess timeouts fire immediately).
#[proc_macro_attribute]
pub fn agent_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(Span::call_site(), "#[agent_test] takes no arguments")
            .to_compile_error()
            .into();
    }
    let func = parse_macro_input!(item as ItemFn);
    if func.sig.asyncness.is_none() {
        return syn::Error::new(
            func.sig.fn_token.span(),
            "#[agent_test] requires an `async fn`",
        )
        .to_compile_error()
        .into();
    }
    if !func.sig.inputs.is_empty() {
        return syn::Error::new(
            func.sig.inputs.span(),
            "#[agent_test] functions take no arguments",
        )
        .to_compile_error()
        .into();
    }
    let krate = tools_crate();
    let attrs = &func.attrs;
    let vis = &func.vis;
    let name = &func.sig.ident;
    let output = &func.sig.output;
    let block = &func.block;
    quote! {
        #[::core::prelude::v1::test]
        #(#attrs)*
        #vis fn #name() #output {
            async fn __agent_test_body() #output #block
            #krate::__private::testing::run_test(__agent_test_body)
        }
    }
    .into()
}
