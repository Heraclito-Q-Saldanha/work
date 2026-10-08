//! Tradução do código de um `<script lang="rs">`.
//!
//! O script roda quando a página carrega, então o código vira o corpo de uma função
//! exportada (`__wk_main_*`). As funções chamadas por atributos de evento viram closures
//! guardadas em `thread_local!`, com wrappers exportados que as chamam. Os `let` de nível
//! superior usados por essas funções ou pelos templates do HTML passam a ser estado
//! compartilhado (`__WkShared`).
//!
//! Cada região do template (ver [`crate::template`]) vira uma *binding*: uma closure que
//! gera o HTML da região, assinada nos ids das variáveis que usa. Escritas nas variáveis
//! (`get_mut`) as marcam como sujas, e `__wk_flush` reexecuta só as bindings afetadas.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use proc_macro2::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::{
    Block, Expr, FnArg, Item, ItemFn, Lit, Local, Macro, Pat, PatIdent, ReturnType, Stmt, Token,
    Type, UnOp,
    parse::Parser,
    parse_quote,
    punctuated::Punctuated,
    visit::{self, Visit},
    visit_mut::{self, VisitMut},
};

use crate::{rust::WASM_BINDGEN_ATTR, template::Region};

const RUNTIME_STATE: &str = r#"
thread_local! {
    static __WK_NEXT_ID: ::std::cell::Cell<usize> = ::std::cell::Cell::new(0);
    static __WK_DIRTY: ::std::cell::RefCell<::std::vec::Vec<usize>> =
        ::std::cell::RefCell::new(::std::vec::Vec::new());
    static __WK_BINDINGS: ::std::cell::RefCell<::std::vec::Vec<(::std::vec::Vec<usize>, ::std::rc::Rc<dyn Fn()>)>> =
        ::std::cell::RefCell::new(::std::vec::Vec::new());
}

// Estado compartilhado entre as funções do script. O wasm roda em uma única thread, então
// o acesso sem checagem de empréstimo é aceitável aqui.
#[allow(dead_code)]
struct __WkShared<T> {
    id: usize,
    cell: ::std::rc::Rc<::std::cell::UnsafeCell<T>>,
}

#[allow(dead_code)]
impl<T> __WkShared<T> {
    fn new(value: T) -> Self {
        let id = __WK_NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id + 1);
            id
        });
        Self { id, cell: ::std::rc::Rc::new(::std::cell::UnsafeCell::new(value)) }
    }

    fn id(&self) -> usize {
        self.id
    }

    // Leitura.
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut T {
        unsafe { &mut *self.cell.get() }
    }

    // Possível escrita: marca a variável como suja.
    #[allow(clippy::mut_from_ref)]
    fn get_mut(&self) -> &mut T {
        __WK_DIRTY.with(|d| d.borrow_mut().push(self.id));
        self.get()
    }
}

impl<T> Clone for __WkShared<T> {
    fn clone(&self) -> Self {
        Self { id: self.id, cell: self.cell.clone() }
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

// Renderiza uma vez e reexecuta quando alguma variável em `deps` for escrita.
#[allow(dead_code)]
fn __wk_bind(deps: &[usize], render: impl Fn() + 'static) {
    let pending = __WK_DIRTY.with(|d| d.borrow().len());
    render();
    __WK_DIRTY.with(|d| d.borrow_mut().truncate(pending));
    __WK_BINDINGS.with(|b| b.borrow_mut().push((deps.to_vec(), ::std::rc::Rc::new(render))));
}

#[allow(dead_code)]
fn __wk_flush() {
    let dirty = __WK_DIRTY.with(|d| ::std::mem::take(&mut *d.borrow_mut()));
    if dirty.is_empty() {
        return;
    }
    let stale: ::std::vec::Vec<::std::rc::Rc<dyn Fn()>> = __WK_BINDINGS.with(|b| {
        b.borrow()
            .iter()
            .filter(|(deps, _)| deps.iter().any(|d| dirty.contains(d)))
            .map(|(_, f)| f.clone())
            .collect()
    });
    for render in stale {
        render();
    }
    // Renderizar só lê, mas métodos usados como receptor marcam a variável como suja.
    __WK_DIRTY.with(|d| d.borrow_mut().clear());
}
"#;

const RUNTIME_REGIONS: &str = r#"
#[::wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = __wk, js_name = set)]
    fn __wk_set(id: usize, html: &str);
}

fn __wk_escape(s: &str) -> ::std::string::String {
    let mut out = ::std::string::String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
"#;

pub fn transform(
    code: &str,
    names: &BTreeSet<String>,
    main_name: &str,
    regions: &[Region],
) -> Result<String> {
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
        let mut rewriter = Rewriter::new(active);
        rewriter.visit_block_mut(&mut f.block);
        used_by_fn.push(rewriter.used);
    }

    let mut region_blocks: Vec<(usize, Block, BTreeSet<String>)> = Vec::new();
    for region in regions {
        let mut block = region.render.clone();
        let mut rewriter = Rewriter::new(candidates.clone());
        rewriter.visit_block_mut(&mut block);
        region_blocks.push((region.id, block, rewriter.used));
    }

    let promoted: BTreeSet<String> = used_by_fn
        .iter()
        .chain(region_blocks.iter().map(|(_, _, used)| used))
        .flatten()
        .cloned()
        .collect();
    let has_state = !promoted.is_empty() || !regions.is_empty();

    let mut rewriter = Rewriter::new(promoted.clone());
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

    // Cada função e região é registrada logo após a última variável compartilhada que usa.
    let after_decls = |used: &BTreeSet<String>| {
        used.iter()
            .filter_map(|v| decl_end.get(v))
            .max()
            .copied()
            .unwrap_or(0)
    };
    let mut registrations: Vec<(usize, Stmt)> = Vec::new();
    let mut statics: Vec<TokenStream> = Vec::new();
    let mut wrappers: Vec<TokenStream> = Vec::new();
    for (f, used) in exported.iter().zip(&used_by_fn) {
        let (registration, static_def, wrapper) = closure_parts(f, used, has_state);
        registrations.push((after_decls(used), registration));
        statics.push(static_def);
        wrappers.push(wrapper);
    }
    for (id, block, used) in &region_blocks {
        registrations.push((after_decls(used), region_binding(*id, block, used)));
    }
    registrations.sort_by_key(|(at, _)| *at);

    let mut final_body: Vec<Stmt> = Vec::new();
    let mut pending = registrations.into_iter().peekable();
    for (i, stmt) in main_stmts.into_iter().enumerate() {
        while let Some((_, reg)) = pending.next_if(|(at, _)| *at <= i) {
            final_body.push(reg);
        }
        final_body.push(stmt);
    }
    final_body.extend(pending.map(|(_, reg)| reg));
    if has_state {
        final_body.push(parse_quote!(__wk_flush();));
    }

    let main_ident = format_ident!("{main_name}");
    let attr: TokenStream = WASM_BINDGEN_ATTR.parse().unwrap();
    let state: TokenStream = if has_state {
        RUNTIME_STATE.parse().unwrap()
    } else {
        quote!()
    };
    let region_rt: TokenStream = if regions.is_empty() {
        quote!()
    } else {
        RUNTIME_REGIONS.parse().unwrap()
    };
    let tokens = quote! {
        #state
        #region_rt
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

/// Registro de uma região do template: renderiza agora e quando suas variáveis mudarem.
fn region_binding(id: usize, block: &Block, used: &BTreeSet<String>) -> Stmt {
    let names: Vec<_> = used.iter().map(|v| format_ident!("{v}")).collect();
    parse_quote!({
        #(let #names = #names.clone();)*
        __wk_bind(&[#(#names.id()),*], move || {
            __wk_set(#id, &#block);
        });
    })
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
fn closure_parts(
    f: &ItemFn,
    used: &BTreeSet<String>,
    flush: bool,
) -> (Stmt, TokenStream, TokenStream) {
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
    let flush = if flush {
        quote!(__wk_flush();)
    } else {
        quote!()
    };

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
            let __wk_result = #storage.with(|f| (f.borrow().as_ref().expect(#missing))(#(#args),*));
            #flush
            __wk_result
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

/// Troca usos das variáveis `active` por `(*nome.get())` (leitura) ou `(*nome.get_mut())`
/// (possível escrita), respeitando sombreamento.
struct Rewriter {
    active: BTreeSet<String>,
    used: BTreeSet<String>,
    /// A expressão visitada é um lugar que pode ser escrito (lado esquerdo de `=`, `&mut`, receptor de método).
    mutable: bool,
}

fn is_compound_assign(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_)
            | SubAssign(_)
            | MulAssign(_)
            | DivAssign(_)
            | RemAssign(_)
            | BitXorAssign(_)
            | BitAndAssign(_)
            | BitOrAssign(_)
            | ShlAssign(_)
            | ShrAssign(_)
    )
}

/// Identificadores capturados em strings de formatação (`"{nome}"`, `"{nome:?}"`).
fn inline_format_args(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '{' {
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'{') {
            i += 2;
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        if end > start
            && !chars[start].is_ascii_digit()
            && matches!(chars.get(end), Some('}' | ':'))
        {
            found.push(chars[start..end].iter().collect());
        }
        i = end.max(i + 1);
    }
    found
}

impl Rewriter {
    fn new(active: BTreeSet<String>) -> Self {
        Self {
            active,
            used: BTreeSet::new(),
            mutable: false,
        }
    }

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

    fn visit_as(&mut self, mutable: bool, e: &mut Expr) {
        self.mutable = mutable;
        self.visit_expr_mut(e);
    }
}

impl VisitMut for Rewriter {
    fn visit_expr_mut(&mut self, e: &mut Expr) {
        let mutable = std::mem::replace(&mut self.mutable, false);
        match e {
            Expr::Path(p)
                if p.qself.is_none()
                    && p.path
                        .get_ident()
                        .is_some_and(|id| self.active.contains(&id.to_string())) =>
            {
                let id = p.path.get_ident().unwrap().clone();
                self.used.insert(id.to_string());
                *e = if mutable {
                    parse_quote!((*#id.get_mut()))
                } else {
                    parse_quote!((*#id.get()))
                };
            }
            Expr::Assign(a) => {
                self.visit_as(true, &mut a.left);
                self.visit_as(false, &mut a.right);
            }
            Expr::Binary(b) if is_compound_assign(&b.op) => {
                self.visit_as(true, &mut b.left);
                self.visit_as(false, &mut b.right);
            }
            Expr::Reference(r) => {
                let is_mut = r.mutability.is_some();
                self.visit_as(is_mut, &mut r.expr);
            }
            Expr::MethodCall(m) => {
                self.visit_as(true, &mut m.receiver);
                for arg in &mut m.args {
                    self.visit_as(false, arg);
                }
            }
            Expr::Field(f) => self.visit_as(mutable, &mut f.base),
            Expr::Index(i) => {
                self.visit_as(mutable, &mut i.expr);
                self.visit_as(false, &mut i.index);
            }
            Expr::Paren(p) => self.visit_as(mutable, &mut p.expr),
            Expr::Unary(u) if matches!(u.op, UnOp::Deref(_)) => self.visit_as(mutable, &mut u.expr),
            _ => visit_mut::visit_expr_mut(self, e),
        }
        self.mutable = false;
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
                if let Expr::Lit(lit) = arg
                    && let Lit::Str(s) = &lit.lit
                {
                    // O estado capturado em `"{nome}"` precisa ser clonado para dentro das closures.
                    for name in inline_format_args(&s.value()) {
                        if self.active.contains(&name) {
                            self.used.insert(name);
                        }
                    }
                }
                self.visit_as(false, arg);
            }
            m.tokens = args.into_token_stream();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code: &str, names: &[&str]) -> String {
        run_with(code, names, "")
    }

    fn run_with(code: &str, names: &[&str], html: &str) -> String {
        let names = names.iter().map(|s| s.to_string()).collect();
        let template = crate::template::compile(html).unwrap();
        let out = transform(code, &names, "__wk_main_t", &template.regions).unwrap();
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
        assert!(out.contains("(*count.get_mut()) += 1;"));
        assert!(out.contains("(*count.get_mut()) += 10;"));
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

    #[test]
    fn writes_use_get_mut_and_reads_use_get() {
        let out = run(
            "let mut v = vec![1];\nlet mut n = 0;\npub fn f() { v.push(n); n = n + 1; let _ = v.len(); }",
            &["f"],
        );
        assert!(out.contains("(*v.get_mut()).push((*n.get()));"));
        assert!(out.contains("(*n.get_mut()) = (*n.get()) + 1;"));
    }

    #[test]
    fn inline_format_args_count_as_uses() {
        let out = run(
            "let a = 1;\nlet b = 2;\npub fn f() { println!(\"{a} {b:?}\"); }",
            &["f"],
        );
        assert!(out.contains("let a = a.clone();"));
        assert!(out.contains("let b = b.clone();"));
    }

    #[test]
    fn regions_become_bindings_subscribed_to_their_variables() {
        let out = run_with(
            "let mut n = 0;\nlet other = 1;\npub fn inc() { n += 1; }",
            &["inc"],
            "<p>{n}</p>{#each [1, 2].iter() as x}{x}{/each}",
        );
        let flat: String = out.split_whitespace().collect();
        assert!(flat.contains("&[n.id()]"));
        assert!(flat.contains("&[],"));
        assert!(flat.contains("__wk_set(0usize"));
        assert!(flat.contains("__wk_set(1usize"));
        assert!(out.contains("let other = 1;"));
        assert!(out.contains("__wk_flush();"));
    }

    #[test]
    fn template_loop_variables_shadow_state() {
        let out = run_with(
            "let item = 1;",
            &[],
            "{#each [1].iter() as item}{item}{/each}",
        );
        assert!(!out.contains("__WkShared::new"));
    }
}
