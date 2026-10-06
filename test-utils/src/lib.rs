//! Test-only attribute macros for devsandbox. A path-only dev-dependency of
//! the root crate: `cargo publish` strips it, so this crate is never published.
//!
//! The expansions call into the root crate's `crate::test_support`, which owns
//! the actual probing and the skip-locally / fail-on-CI policy.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{Ident, ItemFn, ReturnType, parse_macro_input, punctuated::Punctuated, Token};

/// A test that needs a working `docker`. `#[docker_test(helper)]` also needs
/// the embedded devsbd helper for the host arch.
///
/// The function takes no arguments and returns `Result<(), E: Display>`.
/// `Err(why)` means "environment can't run this" (no network, missing host
/// tool, …) and is treated like a missing requirement: printed and skipped
/// locally, a failure on CI. Panics (asserts) are ordinary test failures.
#[proc_macro_attribute]
pub fn docker_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let flags = parse_macro_input!(attr with Punctuated::<Ident, Token![,]>::parse_terminated);
    let mut needs = vec![quote!(Docker)];
    for flag in &flags {
        match flag.to_string().as_str() {
            "helper" => needs.push(quote!(Helper)),
            other => {
                let msg = format!("unknown docker_test flag `{other}` (expected `helper`)");
                return syn::Error::new(flag.span(), msg).to_compile_error().into();
            }
        }
    }
    gated(needs, parse_macro_input!(item as ItemFn)).into()
}

/// A test that needs the embedded devsbd helper for the host arch but not
/// docker. Same contract as [`docker_test`].
#[proc_macro_attribute]
pub fn helper_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        let msg = "helper_test takes no arguments";
        return syn::Error::new(Span::call_site(), msg).to_compile_error().into();
    }
    gated(vec![quote!(Helper)], parse_macro_input!(item as ItemFn)).into()
}

/// A test whose requirement only its body can probe (a host tool, a service
/// manager): no declared needs, so `Err(why)` is the whole gate. Same
/// contract as [`docker_test`].
#[proc_macro_attribute]
pub fn host_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        let msg = "host_test takes no arguments";
        return syn::Error::new(Span::call_site(), msg).to_compile_error().into();
    }
    gated(vec![], parse_macro_input!(item as ItemFn)).into()
}

fn gated(needs: Vec<TokenStream2>, func: ItemFn) -> TokenStream2 {
    let ItemFn { attrs, vis, sig, block } = func;
    if !sig.inputs.is_empty() || sig.asyncness.is_some() || !sig.generics.params.is_empty() {
        let msg = "gated tests must be plain `fn name() -> Result<(), E>`";
        return syn::Error::new_spanned(&sig, msg).to_compile_error();
    }
    let ReturnType::Type(_, ret) = &sig.output else {
        let msg = "gated tests must return `Result<(), E>`; `Err(why)` skips locally, fails on CI";
        return syn::Error::new_spanned(&sig, msg).to_compile_error();
    };
    let name = &sig.ident;
    let label = name.to_string();
    quote! {
        #(#attrs)*
        #[test]
        #vis fn #name() {
            crate::test_support::gated(
                #label,
                &[#(crate::test_support::Need::#needs),*],
                || -> #ret #block,
            );
        }
    }
}
