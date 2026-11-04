//! Proc-macro crate for `#[timed]`.
//!
//! This crate only contains the macro. The runtime (registry + report
//! formatting) lives in the sibling `core-timing` crate, which re-exports
//! this macro so consumers only ever need to depend on `core-timing`.

use proc_macro::TokenStream;
use quote::quote_spanned;
use syn::{
    Ident, ItemFn, LitStr, Token,
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

    // `Instant::now()` and a `Drop` guard are not allowed in a `const fn`; fail here with a
    // clear message instead of a confusing error inside the expansion.
    if let Some(constness) = &input.sig.constness {
        return syn::Error::new_spanned(constness, "#[timed] cannot be used on a `const fn`")
            .to_compile_error()
            .into();
    }

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
    let fn_name = sig.ident.to_string();
    // the body's statements are spliced in directly; `{ #block }` would trip `unused_braces`
    let stmts = &block.stmts;

    let category = args
        .category
        .map_or_else(|| "uncategorized".to_string(), |ident| ident.to_string());
    let label = args.label.map_or(fn_name, |lit| lit.value());

    // The body is emitted once and unchanged, behind a drop guard that records the time when
    // the function returns (normally, by `return` or by `?`). No closure or async block
    // wraps it, so the return type is never written out: `impl Trait` returns, borrowed
    // returns and `unsafe fn` bodies keep working, and `static`s in the body stay single.
    // In an `async fn` the guard lives inside the future, so it measures until the future
    // completes or is dropped. `cfg!` is evaluated in the crate that uses the macro; when
    // its `timing` feature is off the guard is a constant `None` and compiles away.
    let output = quote_spanned! { span =>
        #(#attrs)*
        #vis #sig {
            let __core_timing_guard = if ::core::cfg!(feature = "timing") {
                ::core::option::Option::Some(::core_timing::Timer::start(
                    #category,
                    #label,
                    ::core::file!(),
                ))
            } else {
                ::core::option::Option::None
            };
            #(#stmts)*
        }
    };

    output.into()
}