//! Proc-macro crate for `#[timed]`.
//!
//! This crate only contains the macro. The runtime lives in the sibling
//! `core-timing` crate, which re-exports this macro so consumers only ever
//! need to depend on `core-timing`.

use proc_macro::TokenStream;
use quote::quote_spanned;
use syn::{
    Ident, ItemFn, LitStr, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Parses the optional `#[timed(...)]` argument list:
///   #[timed]                           -> category = None, label = None
///   #[timed(category)]                 -> category = Some(category), label = None
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
/// global `core_timing` registry.
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
/// The consuming crate only needs to depend on `core-timing`; it does not
/// have to declare any feature of its own. Whether anything is recorded is
/// decided solely by the `timing` feature of `core-timing`. When that
/// feature is off, the guard this macro creates is a zero-sized value with
/// no `Drop` impl: no `Instant::now()`, no registry, no lock, no cost.
///
/// To switch timing on for a whole workspace with one flag, forward it:
/// ```toml
/// [features]
/// timing = ["core-timing/timing"]
/// ```
#[proc_macro_attribute]
pub fn timed(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let args = parse_macro_input!(attr as TimedArgs);

    // A drop guard is not allowed in a `const fn`; fail here with a clear
    // message instead of a confusing error inside the expansion.
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

    // Generated tokens carry a span taken from the input item. `quote!`
    // would stamp them with `Span::call_site()`, which makes rustc treat the
    // whole function as macro-generated and skip the `dead_code` lint.
    let span = sig.ident.span();
    let fn_name = sig.ident.to_string();
    // the body's statements are spliced in directly; `{ #block }` would trip `unused_braces`
    let stmts = &block.stmts;

    let category = args
        .category
        .map_or_else(|| "uncategorized".to_string(), |ident| ident.to_string());
    let label = args.label.map_or(fn_name, |lit| lit.value());

    // The body is emitted once and unchanged, behind a guard that records
    // the time when the function returns (normally, by `return` or by `?`).
    // No closure or async block wraps it, so the return type is never
    // written out: `impl Trait` returns, borrowed returns and `unsafe fn`
    // bodies keep working, and `static`s in the body stay single. In an
    // `async fn` the guard lives inside the future, so it measures until the
    // future completes or is dropped.
    //
    // There is deliberately no `cfg!` here. `core_timing::Timer` is the real
    // guard when core-timing's `timing` feature is on and a zero-sized no-op
    // when it is off, so one feature controls everything and the macro and
    // the runtime can never disagree.
    let output = quote_spanned! { span =>
        #(#attrs)*
        #vis #sig {
            let __core_timing_guard = ::core_timing::Timer::start(
                #category,
                #label,
                ::core::file!(),
            );
            #(#stmts)*
        }
    };

    output.into()
}

