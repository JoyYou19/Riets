//! Proc-macro crate for `#[timed]`.
//!
//! This crate only contains the macro. The runtime (registry + report
//! formatting) lives in the sibling `core-timing` crate, which re-exports
//! this macro so consumers only ever need to depend on `core-timing`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, LitStr, ReturnType, parse_macro_input};

/// Wrap a function so its wall-clock execution time is recorded into the
/// global `core_timing` registry, gated entirely behind the `timing`
/// feature of the *crate that uses this macro*.
///
/// Usage:
/// ```ignore
/// use core_timing::timed;
///
/// #[timed]
/// fn parse_document(raw: &str) -> Document { ... }
///
/// #[timed]
/// async fn write_shard_to_disk(shard: &Shard) -> std::io::Result<()> { ... }
///
/// // Optional custom label instead of the function name:
/// #[timed("shard_flush")]
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
    let ItemFn {
        attrs,
        vis,
        sig,
        block,
        ..
    } = input;

    let is_async = sig.asyncness.is_some();
    let is_unsafe = sig.unsafety.is_some();
    let fn_name = sig.ident.to_string();

    // #[timed] uses the function name; #[timed("label")] overrides it.
    let label = if attr.is_empty() {
        fn_name
    } else {
        let lit = parse_macro_input!(attr as LitStr);
        lit.value()
    };

    let ret_ty = match &sig.output {
        ReturnType::Default => quote! { () },
        ReturnType::Type(_, ty) => quote! { #ty },
    };

    // If the original fn was `unsafe fn`, its body may rely on being an
    // implicit unsafe context. Wrapping the body in a plain closure/async
    // block loses that, so we re-wrap explicitly in that case.
    let body_tokens = if is_unsafe {
        quote! { unsafe #block }
    } else {
        quote! { #block }
    };

    let timed_expr = if is_async {
        quote! {
            {
                let __perf_start = ::std::time::Instant::now();
                let __perf_result: #ret_ty = (async move #body_tokens).await;
                ::core_timing::record(#label, __perf_start.elapsed());
                __perf_result
            }
        }
    } else {
        quote! {
            {
                let __perf_start = ::std::time::Instant::now();
                let __perf_result: #ret_ty = (move || -> #ret_ty #body_tokens)();
                ::core_timing::record(#label, __perf_start.elapsed());
                __perf_result
            }
        }
    };

    let output = quote! {
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
