//! Tradução do código de um `<script lang="rs">`.
//!
//! O script roda quando a página carrega, então o código vira o corpo de uma função
//! exportada (`__wk_main_*`). As funções chamadas por atributos de evento viram closures
//! guardadas em `thread_local!`, com wrappers exportados que as chamam. Os `let` de nível
//! superior usados por essas funções passam a ser estado compartilhado (`__WkShared`).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::{
    Block, Expr, FnArg, Item, ItemFn, Local, Macro, Pat, PatIdent, ReturnType, Stmt, Token, Type,
    parse::Parser,
    parse_quote,
    punctuated::Punctuated,
    visit::{self, Visit},
    visit_mut::{self, VisitMut},
};

use crate::rust::WASM_BINDGEN_ATTR;

const SHARED_HELPER: &str = r#"
// Estado compartilhado entre as funções do script. O wasm roda em uma única thread, então
// o acesso sem checagem de empréstimo é aceitável aqui.
struct __WkShared<T>(::std::rc::Rc<::std::cell::UnsafeCell<T>>);

impl<T> __WkShared<T> {
    fn new(value: T) -> Self {
        Self(::std::rc::Rc::new(::std::cell::UnsafeCell::new(value)))
    }

    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

impl<T> Clone for __WkShared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ::std::fmt::Display> ::std::fmt::Display for __WkShared<T> {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        ::std::fmt::Display::fmt(self.get(), f)
    }
}

impl<T: ::std::fmt::Debug> ::std::fmt::Debug for __WkShared<T> {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        ::std::fmt::Debug::fmt(self.get(), f)
    }
}
"#;

pub fn transform(code: &str, names: &BTreeSet<String>, main_name: &str) -> Result<String> {
    let stmts = Block::parse_within
        .parse_str(code)
        .map_err(|e| anyhow!("Rust inválido: {e}"))?;

    let mut hoisted: Vec<Item> = Vec::new();
    let mut exported: Vec<ItemFn> = Vec::new();
    let mut body: Vec<Stmt> = Vec::new();
    for stmt in stmts {
        match stmt {
            Stmt::Item(Item::Fn(f)) if names.contains(&f.sig.ident.to_string()) => {
                if is_closurable(&f) {
                    exported.push(f);
                } else {
                    let mut f = f;
                    f.attrs
                        .push(parse_quote!(#[::wasm_bindgen::prelude::wasm_bindgen]));
                    hoisted.push(Item::Fn(f));
                }
            }
            Stmt::Item(mut item) => {
                convert_extern_js(&mut item);
                hoisted.push(item);
            }
            other => body.push(other),
        }
    }

    // Só variáveis declaradas uma única vez, com padrão simples, podem virar estado compartilhado.
    let mut declared: BTreeMap<String, usize> = BTreeMap::new();
    for stmt in &body {
        if let Stmt::Local(local) = stmt {
            for name in bound_idents(&local.pat) {
                *declared.entry(name).or_default() += 1;
            }
        }
    }
    let candidates: BTreeSet<String> = body
        .iter()
        .filter_map(simple_decl)
        .filter(|n| declared[n] == 1)
        .collect();

    let mut used_by_fn: Vec<BTreeSet<String>> = Vec::new();
    for f in &mut exported {
        let mut active = candidates.clone();
        for arg in &f.sig.inputs {
            if let FnArg::Typed(t) = arg {
                for name in bound_idents(&t.pat) {
                    active.remove(&name);
                }
            }
        }
        let mut rewriter = Rewriter {
            active,
            used: BTreeSet::new(),
        };
        rewriter.visit_block_mut(&mut f.block);
        used_by_fn.push(rewriter.used);
    }
    let promoted: BTreeSet<String> = used_by_fn.iter().flatten().cloned().collect();

    let mut rewriter = Rewriter {
        active: promoted.clone(),
        used: BTreeSet::new(),
    };
    let mut main_stmts: Vec<Stmt> = Vec::new();
    let mut decl_end: BTreeMap<String, usize> = BTreeMap::new();
    for mut stmt in body {
        match simple_decl(&stmt).filter(|n| promoted.contains(n)) {
            Some(name) => {
                stmt = share_declaration(stmt, &mut rewriter);
                decl_end.insert(name, main_stmts.len() + 1);
            }
            None => rewriter.visit_stmt_mut(&mut stmt),
        }
        main_stmts.push(stmt);
    }

    // Cada função é registrada logo após a última variável compartilhada que ela usa.
    let mut registrations: Vec<(usize, Stmt)> = Vec::new();
    let mut statics: Vec<TokenStream> = Vec::new();
    let mut wrappers: Vec<TokenStream> = Vec::new();
    for (f, used) in exported.iter().zip(&used_by_fn) {
        let (registration, static_def, wrapper) = closure_parts(f, used);
        let at = used
            .iter()
            .filter_map(|v| decl_end.get(v))
            .max()
            .copied()
            .unwrap_or(0);
        registrations.push((at, registration));
        statics.push(static_def);
        wrappers.push(wrapper);
    }
    let mut final_body: Vec<Stmt> = Vec::new();
    let mut pending = registrations.into_iter().peekable();
    for (i, stmt) in main_stmts.into_iter().enumerate() {
        while let Some((_, reg)) = pending.next_if(|(at, _)| *at <= i) {
            final_body.push(reg);
        }
        final_body.push(stmt);
    }
    final_body.extend(pending.map(|(_, reg)| reg));

    let main_ident = format_ident!("{main_name}");
    let attr: TokenStream = WASM_BINDGEN_ATTR.parse().unwrap();
    let helper: TokenStream = if promoted.is_empty() {
        quote!()
    } else {
        SHARED_HELPER.parse().unwrap()
    };
    let tokens = quote! {
        #helper
        #(#hoisted)*
        #(#statics)*
        #(#wrappers)*
        #attr
        pub fn #main_ident() {
            #(#final_body)*
        }
    };
    let file = syn::parse2::<syn::File>(tokens).map_err(|e| anyhow!("erro interno: {e}"))?;
    Ok(prettyplease::unparse(&file))
}

/// Funções que podem virar closures: sem genéricos, `async` ou `self`.
fn is_closurable(f: &ItemFn) -> bool {
    f.sig.generics.params.is_empty()
        && f.sig.asyncness.is_none()
        && f.sig.variadic.is_none()
        && f.sig.inputs.iter().all(|a| matches!(a, FnArg::Typed(_)))
}

fn convert_extern_js(item: &mut Item) {
    if let Item::ForeignMod(m) = item
        && m.abi.name.as_ref().is_some_and(|n| n.value() == "js")
    {
        m.abi.name = Some(parse_quote!("C"));
        m.attrs
            .push(parse_quote!(#[::wasm_bindgen::prelude::wasm_bindgen]));
    }
}

/// Nome da variável em `let nome = ...;` / `let mut nome: T = ...;`.
fn simple_decl(stmt: &Stmt) -> Option<String> {
    let Stmt::Local(local) = stmt else {
        return None;
    };
    let init = local.init.as_ref()?;
    if init.diverge.is_some() {
        return None;
    }
    let pat = match &local.pat {
        Pat::Type(t) => &*t.pat,
        p => p,
    };
    match pat {
        Pat::Ident(PatIdent {
            ident,
            by_ref: None,
            subpat: None,
            ..
        }) => Some(ident.to_string()),
        _ => None,
    }
}

/// `let mut x: T = init;` vira `let x: __WkShared<T> = __WkShared::new(init);`.
fn share_declaration(stmt: Stmt, rewriter: &mut Rewriter) -> Stmt {
    let Stmt::Local(Local {
        pat,
        init: Some(mut init),
        ..
    }) = stmt
    else {
        unreachable!("simple_decl garante um let com inicializador");
    };
    rewriter.visit_expr_mut(&mut init.expr);
    let value = &init.expr;
    match pat {
        Pat::Type(t) => {
            let (name, ty) = (ident_of(&t.pat), &t.ty);
            parse_quote!(let #name: __WkShared<#ty> = __WkShared::new(#value);)
        }
        pat => {
            let name = ident_of(&pat);
            parse_quote!(let #name = __WkShared::new(#value);)
        }
    }
}

fn ident_of(pat: &Pat) -> syn::Ident {
    match pat {
        Pat::Ident(p) => p.ident.clone(),
        _ => unreachable!("simple_decl garante um identificador"),
    }
}

/// Devolve (registro da closure, `thread_local!` que a guarda, wrapper exportado).
fn closure_parts(f: &ItemFn, used: &BTreeSet<String>) -> (Stmt, TokenStream, TokenStream) {
    let name = &f.sig.ident;
    let storage = format_ident!("__WK_FN_{}", name.to_string().to_uppercase());
    let attrs = &f.attrs;
    let block = &f.block;
    let ret: Type = match &f.sig.output {
        ReturnType::Default => parse_quote!(()),
        ReturnType::Type(_, ty) => (**ty).clone(),
    };
    let (pats, tys): (Vec<_>, Vec<_>) = f
        .sig
        .inputs
        .iter()
        .filter_map(|a| match a {
            FnArg::Typed(t) => Some((&t.pat, &t.ty)),
            FnArg::Receiver(_) => None,
        })
        .unzip();
    let args: Vec<_> = (0..pats.len()).map(|i| format_ident!("__a{i}")).collect();
    let clones = used.iter().map(|v| {
        let v = format_ident!("{v}");
        quote!(let #v = #v.clone();)
    });
    let missing = format!("`{name}` chamada antes do script da página terminar de carregar");
    let attr: TokenStream = WASM_BINDGEN_ATTR.parse().unwrap();

    let registration = parse_quote!({
        #(#clones)*
        let __wk_f: ::std::boxed::Box<dyn Fn(#(#tys),*) -> #ret> =
            ::std::boxed::Box::new(move |#(#pats: #tys),*| -> #ret #block);
        #storage.with(|s| *s.borrow_mut() = Some(__wk_f));
    });
    let static_def = quote! {
        thread_local! {
            static #storage: ::std::cell::RefCell<Option<::std::boxed::Box<dyn Fn(#(#tys),*) -> #ret>>> =
                ::std::cell::RefCell::new(None);
        }
    };
    let wrapper = quote! {
        #(#attrs)*
        #attr
        pub fn #name(#(#args: #tys),*) -> #ret {
            #storage.with(|f| (f.borrow().as_ref().expect(#missing))(#(#args),*))
        }
    };
    (registration, static_def, wrapper)
}

fn bound_idents(pat: &Pat) -> Vec<String> {
    struct Collector(Vec<String>);
    impl<'ast> Visit<'ast> for Collector {
        fn visit_pat_ident(&mut self, p: &'ast PatIdent) {
            self.0.push(p.ident.to_string());
            visit::visit_pat_ident(self, p);
        }
    }
    let mut c = Collector(Vec::new());
    c.visit_pat(pat);
    c.0
}

/// Troca usos das variáveis `active` por `(*nome.get())`, respeitando sombreamento.
struct Rewriter {
    active: BTreeSet<String>,
    used: BTreeSet<String>,
}

impl Rewriter {
    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        let saved = self.active.clone();
        f(self);
        self.active = saved;
    }

    fn shadow(&mut self, pat: &Pat) {
        for name in bound_idents(pat) {
            self.active.remove(&name);
        }
    }
}

impl VisitMut for Rewriter {
    fn visit_expr_mut(&mut self, e: &mut Expr) {
        if let Expr::Path(p) = e
            && p.qself.is_none()
            && let Some(id) = p.path.get_ident()
            && self.active.contains(&id.to_string())
        {
            let id = id.clone();
            self.used.insert(id.to_string());
            *e = parse_quote!((*#id.get()));
            return;
        }
        visit_mut::visit_expr_mut(self, e);
    }

    fn visit_block_mut(&mut self, block: &mut Block) {
        self.scoped(|this| {
            for stmt in &mut block.stmts {
                this.visit_stmt_mut(stmt);
                if let Stmt::Local(local) = stmt {
                    this.shadow(&local.pat);
                }
            }
        });
    }

    fn visit_expr_closure_mut(&mut self, c: &mut syn::ExprClosure) {
        self.scoped(|this| {
            for input in &c.inputs {
                this.shadow(input);
            }
            this.visit_expr_mut(&mut c.body);
        });
    }

    fn visit_arm_mut(&mut self, arm: &mut syn::Arm) {
        self.scoped(|this| {
            this.shadow(&arm.pat);
            this.visit_pat_mut(&mut arm.pat);
            this.visit_expr_mut(&mut arm.body);
        });
    }

    fn visit_expr_for_loop_mut(&mut self, l: &mut syn::ExprForLoop) {
        self.visit_expr_mut(&mut l.expr);
        self.scoped(|this| {
            this.shadow(&l.pat);
            this.visit_block_mut(&mut l.body);
        });
    }

    fn visit_expr_if_mut(&mut self, i: &mut syn::ExprIf) {
        self.scoped(|this| {
            this.visit_expr_mut(&mut i.cond);
            this.visit_block_mut(&mut i.then_branch);
        });
        if let Some((_, otherwise)) = &mut i.else_branch {
            self.visit_expr_mut(otherwise);
        }
    }

    fn visit_expr_while_mut(&mut self, w: &mut syn::ExprWhile) {
        self.scoped(|this| {
            this.visit_expr_mut(&mut w.cond);
            this.visit_block_mut(&mut w.body);
        });
    }

    fn visit_expr_let_mut(&mut self, l: &mut syn::ExprLet) {
        self.visit_expr_mut(&mut l.expr);
        self.shadow(&l.pat);
    }

    fn visit_pat_guard_mut(&mut self, g: &mut syn::PatGuard) {
        self.visit_expr_mut(&mut g.guard);
    }

    // Itens aninhados não capturam variáveis locais.
    fn visit_item_mut(&mut self, _: &mut Item) {}

    fn visit_field_value_mut(&mut self, f: &mut syn::FieldValue) {
        visit_mut::visit_field_value_mut(self, f);
        if f.colon_token.is_none() && !matches!(f.expr, Expr::Path(_)) {
            f.colon_token = Some(Default::default());
        }
    }

    // Macros como `format!("{}", x)` têm argumentos opacos; reescreve os que parseiam como expressões.
    fn visit_macro_mut(&mut self, m: &mut Macro) {
        let parser = Punctuated::<Expr, Token![,]>::parse_terminated;
        if let Ok(mut args) = parser.parse2(m.tokens.clone()) {
            for arg in &mut args {
                self.visit_expr_mut(arg);
            }
            m.tokens = args.into_token_stream();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: &str, names: &[&str]) -> String {
        let names = names.iter().map(|s| s.to_string()).collect();
        let out = transform(code, &names, "__wk_main_t").unwrap();
        syn::parse_file(&out).unwrap();
        out
    }

    #[test]
    fn shares_variables_with_exported_functions() {
        let out = run(
            "let mut count = 0;\nlet unused = 1;\npub fn inc() { count += 1; }\ncount += 10;",
            &["inc"],
        );
        assert!(out.contains("let count = __WkShared::new(0);"));
        assert!(out.contains("(*count.get()) += 1;"));
        assert!(out.contains("(*count.get()) += 10;"));
        assert!(out.contains("let unused = 1;"));
        assert!(out.contains("pub fn inc()"));
        assert!(out.contains("pub fn __wk_main_t()"));
    }

    #[test]
    fn respects_shadowing_and_params() {
        let out = run(
            "let n = 1;\npub fn a(n: i32) -> i32 { n }\npub fn b() -> i32 { let n = 2; n }\npub fn c() -> i32 { n }",
            &["a", "b", "c"],
        );
        assert_eq!(out.matches("(*n.get())").count(), 1);
    }

    #[test]
    fn rewrites_macro_arguments() {
        let out = run("let x = 1;\npub fn f() { println!(\"{}\", x); }", &["f"]);
        assert!(out.contains("(* x.get())"));
    }

    #[test]
    fn converts_extern_js_and_hoists_items() {
        let out = run("extern \"js\" { fn alert(s: &str); }\nalert(\"a\");", &[]);
        assert!(out.contains("extern \"C\""));
        assert!(out.find("extern \"C\"").unwrap() < out.find("pub fn __wk_main_t").unwrap());
    }
}
