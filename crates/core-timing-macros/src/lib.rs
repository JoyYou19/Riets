//! Proc-macro crate for `#[timed]`.
//!
//! This crate only contains the macro. The runtime (registry + report
//! formatting) lives in the sibling `core-timing` crate, which re-exports
//! this macro so consumers only ever need to depend on `core-timing`.

use proc_macro::TokenStream;
use quote::quote_spanned;
use syn::{
    Ident, ItemFn, LitStr, ReturnType, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Parses the optional `#[timed(...)]` argument list:
///   #[timed]                          -> category = None, label = None
///   #[timed(category)]                -> category = Some(category), label = None
///   #[timed(category, "custom label")] -> both
struct TimedArgs {
    category: Option<Ident>,
    label: Option<LitStr>,
}

impl Parse for TimedArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(TimedArgs {
                category: None,
                label: None,
            });
        }
        let category: Ident = input.parse()?;
        let label = if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            Some(input.parse::<LitStr>()?)
        } else {
            None
        };
        Ok(TimedArgs {
            category: Some(category),
            label,
        })
    }
}

/// Wrap a function so its wall-clock execution time is recorded into the
/// global `core_timing` registry, gated entirely behind the `timing`
/// feature of the *crate that uses this macro*.
///
/// Usage:
/// ```ignore
/// use core_timing::timed;
///
/// // No category -> grouped under "uncategorized" in the report.
/// #[timed]
/// fn parse_document(raw: &str) -> Document { ... }
///
/// // Bare identifier -> category for grouping in the report.
/// #[timed(json_parsing)]
/// fn parse_documents(body: &str) -> Result<ParseOutcome, Error> { ... }
///
/// #[timed(inserting_documents)]
/// async fn write_shard_to_disk(shard: &Shard) -> std::io::Result<()> { ... }
///
/// // Category + a custom label overriding the function name:
/// #[timed(disk_io, "shard_flush")]
/// fn flush(&mut self) { ... }
/// ```
///
/// Requires the consuming crate to declare, in its own Cargo.toml:
/// ```toml
/// [features]
/// timing = ["core-timing/timing"]
/// ```
/// and to depend on `core-timing`. When the `timing` feature is not
/// enabled, the generated branch is statically dead and the compiler
/// removes it — no `Instant::now()` call, no registry access, no cost.
#[proc_macro_attribute]
pub fn timed(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr as TimedArgs);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
        ..
    } = input;

    // Tokens this macro generates must carry a span taken from the input
    // item. `quote!` stamps them with `Span::call_site()`, whose expansion
    // context makes rustc treat the whole function as macro-generated and
    // skip the `dead_code` lint for it. `quote_spanned!` with an input span
    // keeps the lint working; interpolated `#sig`/`#block` keep their own
    // spans either way, so diagnostics stay accurate.
    let span = sig.ident.span();
    let is_async = sig.asyncness.is_some();
    let is_unsafe = sig.unsafety.is_some();
    let fn_name = sig.ident.to_string();

    let category = args
        .category.map_or_else(|| "uncategorized".to_string(), |ident| ident.to_string());
    let label = args.label.map_or(fn_name, |lit| lit.value());

    let ret_ty = match &sig.output {
        ReturnType::Default => quote_spanned! { span => () },
        ReturnType::Type(_, ty) => quote_spanned! { span => #ty },
    };

    // If the original fn was `unsafe fn`, its body may rely on being an
    // implicit unsafe context. Wrapping the body in a plain closure/async
    // block loses that, so we re-wrap explicitly in that case.
    let body_tokens = if is_unsafe {
        quote_spanned! { span => unsafe #block }
    } else {
        quote_spanned! { span => #block }
    };

    let timed_expr = if is_async {
        quote_spanned! { span =>
            {
                let __perf_start = ::std::time::Instant::now();
                let __perf_result: #ret_ty = (async move #body_tokens).await;
                ::core_timing::record(#category, #label, file!(), __perf_start.elapsed());
                __perf_result
            }
        }
    } else {
        quote_spanned! { span =>
            {
                let __perf_start = ::std::time::Instant::now();
                let __perf_result: #ret_ty = (move || -> #ret_ty #body_tokens)();
                ::core_timing::record(#category, #label, file!(), __perf_start.elapsed());
                __perf_result
            }
        }
    };

    let output = quote_spanned! { span =>
        #(#attrs)*
        #vis #sig {
            if cfg!(feature = "timing") {
                #timed_expr
            } else {
                #block
            }
        }
    };

    output.into()
}
