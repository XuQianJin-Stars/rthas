// Copyright (C) 2026 Tencent. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `#[rthas::trace]` — turn a function into a probe point.
//!
//! # Why the probe lives *inside* the function body
//!
//! The natural place for a per-function `static Probe` is right next to the
//! function. That does not work here: this attribute is applied to methods
//! inside `impl` blocks, and an `impl` block may only contain associated
//! items — a bare `static` or a free `fn` is a hard error there.
//!
//! So the probe `static` is declared *inside* the function body, which is
//! legal everywhere, and registration goes through `inventory`, which is
//! built for exactly this: it emits a `#[used] static` into a linker
//! section, so the probe is collected at link time even if the function is
//! never called. That is what lets `rthas-cli list` enumerate probe points on
//! a process that has not served any traffic yet.
//!
//! # What the expansion does
//!
//! ```text
//! fn f(a: A) -> R { body }
//! ```
//!
//! becomes (schematically)
//!
//! ```text
//! fn f(a: A) -> R {
//!     static PROBE: Probe = ...;              // declared in-body
//!     inventory::submit! { ProbeSubmission(&PROBE) }
//!     let guard = SpanGuard::begin(&PROBE, || fmt_args(&[("a", &a)]));
//!     let ret  = body;                         // body is NOT wrapped in a closure
//!     if let Some(g) = guard { g.finish(fmt_ret(&ret)) }
//!     ret
//! }
//! ```
//!
//! The body is deliberately **not** wrapped in a closure: a closure would
//! silently change the meaning of `return` and `?` inside the user's code.
//! The trade-off is that an early exit bypasses `finish` — the `Drop`
//! implementation still closes the span and records `<early-return>`, so a
//! panicking or `?`-bailing call is never silently lost.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, ToTokens};
use syn::parse::{Parse, ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{
    parse_macro_input, parse_quote, visit_mut, Block, ExprLit, FnArg, Ident, ItemFn, Lifetime, Lit,
    Meta, Pat, Receiver, ReturnType, Signature, Token, TypeReference,
};
use syn::visit_mut::VisitMut;

/// Options accepted by `#[rthas::trace(...)]`.
#[derive(Default)]
struct Opts {
    /// Override the name shown in traces (defaults to `module::function`).
    name: Option<String>,
    /// Parameter names whose value must not be captured.
    skip: Vec<Ident>,
    /// Also capture `self`. Off by default: it usually prints a whole struct.
    capture_self: bool,
    /// Add `+ Send` to the generated `impl Future` (async fns only).
    send: bool,
}

impl Parse for Opts {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut opts = Opts::default();
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        for meta in metas {
            match meta {
                Meta::NameValue(nv) if nv.path.is_ident("name") => {
                    if let syn::Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }) = nv.value
                    {
                        opts.name = Some(s.value());
                    } else {
                        return Err(syn::Error::new(nv.value.span(), "expected a string literal"));
                    }
                }
                Meta::List(list) if list.path.is_ident("skip") => {
                    let skipped =
                        Punctuated::<Ident, Token![,]>::parse_terminated.parse2(list.tokens)?;
                    opts.skip = skipped.into_iter().collect();
                }
                Meta::Path(p) if p.is_ident("self") => opts.capture_self = true,
                Meta::Path(p) if p.is_ident("send") => opts.send = true,
                other => {
                    return Err(syn::Error::new(
                        other.span(),
                        "unknown rthas option; expected `name = \"..\"`, `skip(..)`, `self` or `send`",
                    ))
                }
            }
        }
        Ok(opts)
    }
}

#[proc_macro_attribute]
pub fn trace(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = match syn::parse::<Opts>(attr) {
        Ok(o) => o,
        Err(e) => return e.to_compile_error().into(),
    };
    let mut func = parse_macro_input!(item as ItemFn);
    let debug = std::env::var("RTHAS_MACRO_DEBUG").is_ok();

    match expand(&mut func, &opts) {
        Ok(ts) => {
            if debug {
                eprintln!(
                    "--- rthas expansion of {} ---\n{}\n---",
                    func.sig.ident, ts
                );
            }
            ts.into()
        }
        Err(e) => {
            // Keep the original function so a bad option degrades to
            // "uninstrumented" rather than "does not compile at all".
            let original = func.into_token_stream();
            let err = e.to_compile_error();
            quote! { #err #original }.into()
        }
    }
}

fn expand(func: &mut ItemFn, opts: &Opts) -> syn::Result<TokenStream2> {
    let sig = &func.sig;
    let vis = &func.vis;
    let attrs = &func.attrs;
    let body: &Block = &func.block;

    let fn_ident = &sig.ident;
    let fn_name_str = opts
        .name
        .clone()
        .unwrap_or_else(|| fn_ident.to_string());

    // `span-locations` gives a usable line number, which uniquifies the
    // generated static name when the same function name appears twice.
    let line = fn_ident.span().start().line;
    let probe_ident = format_ident!(
        "__RTHAS_PROBE_{}_{}",
        fn_name_str.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_"),
        line,
        span = Span::call_site()
    );

    let arg_pairs = build_arg_pairs(sig, opts);
    let is_async = sig.asyncness.is_some();
    let kind = if is_async {
        quote! { ::rthas::ProbeKind::Async }
    } else {
        quote! { ::rthas::ProbeKind::Sync }
    };

    let probe_static = quote! {
        static #probe_ident: ::rthas::Probe = ::rthas::Probe::new(
            concat!(module_path!(), "::", #fn_name_str),
            file!(),
            line!(),
            #kind,
        );
        ::rthas::inventory::submit! { ::rthas::ProbeSubmission(&#probe_ident) }
        // Lets `rthas attach` recognise an instrumented binary without running
        // it. `#[used]` because nothing reads it: without that the linker
        // drops the reference and takes the string bytes with it.
        #[used]
        static __RTHAS_MAGIC: &str = ::rthas::MAGIC;
    };

    let guard = quote! {
        let __rthas_guard = ::rthas::SpanGuard::begin(
            &#probe_ident,
            || ::rthas::fmt_args(&[#(#arg_pairs),*]),
        );
    };
    let finish = quote! {
        if let Some(__rthas_g) = __rthas_guard {
            __rthas_g.finish(::rthas::fmt_ret(&__rthas_ret));
        }
    };

    if !is_async {
        return Ok(quote! {
            #(#attrs)*
            #vis #sig {
                #probe_static
                #guard
                let __rthas_ret = #body;
                #finish
                __rthas_ret
            }
        });
    }

    // ---- async fn -------------------------------------------------------
    //
    // `async fn` desugars to `fn -> impl Future`, so we rewrite the
    // signature and move the body into an `async move` block. The probe
    // wrapper is an *outer* async block that awaits the user's body, which
    // keeps `?` and `return` inside the user's code meaning what they mean.
    let output_ty = match &sig.output {
        ReturnType::Default => quote! { () },
        ReturnType::Type(_, ty) => quote! { #ty },
    };

    let send_bound = if opts.send {
        quote! { + Send }
    } else {
        quote! {}
    };

    let mut new_sig: Signature = sig.clone();
    new_sig.asyncness = None;

    // Unify elided borrows into one lifetime parameter so the generated
    // `impl Future` can be bounded by it. See `LifetimeUnifier`.
    let mut unifier = LifetimeUnifier::new();
    unifier.visit_signature_mut(&mut new_sig);
    if let Some(lt) = &unifier.generated {
        new_sig.generics.params.push(parse_quote!(#lt));
    }
    let lifetimes: Vec<Lifetime> = new_sig
        .generics
        .lifetimes()
        .map(|def| def.lifetime.clone())
        .collect();
    let lt_bound = quote! { #( + #lifetimes )* };

    new_sig.output = ReturnType::Type(
        parse_quote! { -> },
        Box::new(parse_quote! {
            impl ::core::future::Future<Output = #output_ty> #send_bound #lt_bound
        }),
    );

    Ok(quote! {
        #(#attrs)*
        #vis #new_sig {
            #probe_static
            async move {
                #guard
                let __rthas_ret = (async move #body).await;
                #finish
                __rthas_ret
            }
        }
    })
}

/// Build `("name", &name as &dyn Debug)` pairs for every capturable argument.
///
/// Parameters that are destructuring patterns (`(a, b): (u8, u8)`) or `_`
/// have no single binding to name, so they are skipped: inventing a name for
/// them would be more confusing than omitting them.
fn build_arg_pairs(sig: &Signature, opts: &Opts) -> Vec<TokenStream2> {
    let mut pairs = Vec::new();

    for input in &sig.inputs {
        match input {
            FnArg::Receiver(_) => {
                if opts.capture_self {
                    pairs.push(quote! { ("self", &self as &dyn ::std::fmt::Debug) });
                }
            }
            FnArg::Typed(pat_ty) => {
                let ident = match &*pat_ty.pat {
                    Pat::Ident(pi) => Some(&pi.ident),
                    _ => None,
                };
                if let Some(ident) = ident {
                    if !opts.skip.iter().any(|s| s == ident) {
                        let name = ident.to_string();
                        pairs.push(quote! { (#name, &#ident as &dyn ::std::fmt::Debug) });
                    }
                }
            }
        }
    }
    pairs
}

/// Rewrites every elided lifetime in a signature into a fresh named one.
///
/// Needed because `async fn` desugars to `impl Future`, and a future that
/// captures two borrows outlives *both* of them. `&self` and `&str` in the
/// same signature are two distinct lifetimes, so `+ '_` (which binds to one)
/// cannot express that; the bound has to list each.
/// Collapses every elided lifetime in a signature into a single fresh one.
///
/// `async fn` desugars to `fn(..) -> impl Future`, and a future that captures
/// two borrows must outlive *both* of them. `&self` and `&str` in
/// `async fn get(&self, key: &str)` are two *different* lifetimes, so a
/// single `'_` cannot express the bound — and naming each one separately
/// produces an unused parameter the compiler then demands a relationship
/// for, which it cannot infer.
///
/// Unifying them sidesteps both problems: the compiler simply infers the
/// shorter of the borrows, which is exactly the lifetime the future needs to
/// be bounded by. Lifetimes the user wrote explicitly are left untouched.
/// This is the same approach `tracing`'s `#[instrument]` takes.
struct LifetimeUnifier {
    generated: Option<Lifetime>,
}

impl LifetimeUnifier {
    fn new() -> Self {
        Self { generated: None }
    }

    fn lifetime(&mut self) -> Lifetime {
        if let Some(existing) = &self.generated {
            return existing.clone();
        }
        let lifetime = Lifetime::new("'rthas_lt", Span::call_site());
        self.generated = Some(lifetime.clone());
        lifetime
    }
}

impl VisitMut for LifetimeUnifier {
    fn visit_type_reference_mut(&mut self, node: &mut TypeReference) {
        // Recurse first so inner references are rewritten too.
        visit_mut::visit_type_reference_mut(self, node);
        if node.lifetime.is_none() {
            node.lifetime = Some(self.lifetime());
        }
    }

    fn visit_receiver_mut(&mut self, node: &mut Receiver) {
        visit_mut::visit_receiver_mut(self, node);
        // `Receiver` keeps its lifetime inside the `&(And, Option<Lifetime>)`
        // pair rather than in a plain field.
        if let Some((_, lifetime)) = &mut node.reference {
            if lifetime.is_none() {
                *lifetime = Some(self.lifetime());
            }
        }
    }
}
