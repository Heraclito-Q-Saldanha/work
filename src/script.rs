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

use crate::{
    rust::WASM_BINDGEN_ATTR,
    template::{Bind, Part, Region, Template, text_tokens},
};

/// Onde o código traduzido vai viver.
enum Mode<'a> {
    /// Página: o script vira a função exportada `main_name`.
    Page(&'a str),
    /// Componente: o script vira `__wk_create`, que cria uma instância com estado próprio.
    Component(&'a [Part]),
}

/// Nome do export wasm que recebe o valor novo de um `bind:` da página `main_name`.
pub fn bind_fn_name(main_name: &str, id: usize) -> String {
    let page = main_name.strip_prefix("__wk_main_").unwrap_or(main_name);
    format!("__wk_bind_{page}_{id}")
}

pub fn transform(
    code: &str,
    names: &BTreeSet<String>,
    main_name: &str,
    regions: &[Region],
    binds: &[Bind],
) -> Result<String> {
    transform_impl(code, names, regions, binds, Mode::Page(main_name))
}

/// Traduz o script de um componente (`Nome.wk`): o módulo exporta `Props` e `__wk_create`.
pub fn transform_component(
    code: &str,
    names: &BTreeSet<String>,
    template: &Template,
) -> Result<String> {
    transform_impl(
        code,
        names,
        &template.regions,
        &template.binds,
        Mode::Component(&template.parts),
    )
}

/// Funções declaradas no nível superior de um script.
pub fn fn_names(code: &str) -> Result<BTreeSet<String>> {
    let stmts = Block::parse_within
        .parse_str(code)
        .map_err(|e| anyhow!("Rust inválido: {e}"))?;
    Ok(stmts
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Item(Item::Fn(f)) => Some(f.sig.ident.to_string()),
            _ => None,
        })
        .collect())
}

fn transform_impl(
    code: &str,
    names: &BTreeSet<String>,
    regions: &[Region],
    binds: &[Bind],
    mode: Mode,
) -> Result<String> {
    let is_component = matches!(mode, Mode::Component(_));
    let bind_name = |id: usize| match mode {
        Mode::Page(main_name) => bind_fn_name(main_name, id),
        Mode::Component(_) => format!("__wk_bind_{id}"),
    };
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
                } else if is_component {
                    return Err(anyhow!(
                        "`{}` é chamada por um atributo de evento e não pode ser genérica, `async` ou usar `self`",
                        f.sig.ident
                    ));
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

    // O sentido DOM → variável é uma função como as de handler: vira closure e dá flush.
    for bind in binds {
        let name = format_ident!("{}", bind_name(bind.id));
        let target = &bind.target;
        exported.push(parse_quote! {
            pub fn #name(__v: ::wasm_bindgen::JsValue) {
                if let ::std::option::Option::Some(__x) = __WkFromJs::from_js(&__v) {
                    #target = __x;
                }
            }
        });
    }

    // `let nome: T = prop!();` declara uma prop do componente; `let mut nome: T = bind!();`
    // uma prop com ligação de mão dupla, que compartilha a variável do pai.
    let mut props: Vec<(syn::Ident, Type)> = Vec::new();
    let mut bound_props: BTreeSet<String> = BTreeSet::new();
    for stmt in body.iter_mut() {
        let Stmt::Local(Local {
            pat,
            init: Some(init),
            ..
        }) = stmt
        else {
            continue;
        };
        let macro_name = ["prop", "bind"]
            .into_iter()
            .find(|n| macro_named(&init.expr, n).is_some());
        let Some(macro_name) = macro_name else {
            continue;
        };
        let is_bind = macro_name == "bind";
        if !is_component {
            return Err(anyhow!("`{macro_name}!` só pode ser usado em componentes"));
        }
        let (Pat::Type(typed), true) = (&*pat, init.diverge.is_none()) else {
            return Err(anyhow!(
                "`{macro_name}!()` precisa de `let nome: Tipo = {macro_name}!();`"
            ));
        };
        let Pat::Ident(PatIdent {
            ident,
            by_ref: None,
            mutability,
            subpat: None,
            ..
        }) = &*typed.pat
        else {
            return Err(anyhow!("`{macro_name}!()` precisa de um nome simples"));
        };
        match (is_bind, mutability.is_some()) {
            (false, true) => {
                return Err(anyhow!(
                    "props não podem ser `mut`: `let nome: Tipo = prop!();` (use `bind!()` para mutar)"
                ));
            }
            (true, false) => {
                return Err(anyhow!(
                    "`bind!()` precisa de `let mut nome: Tipo = bind!();`"
                ));
            }
            _ => {}
        }
        if !macro_named(&init.expr, macro_name)
            .unwrap()
            .tokens
            .is_empty()
        {
            return Err(anyhow!("`{macro_name}!()` não recebe argumentos"));
        }
        let ty = (*typed.ty).clone();
        if is_bind {
            bound_props.insert(ident.to_string());
            props.push((ident.clone(), parse_quote!(__WkShared<#ty>)));
        } else {
            props.push((ident.clone(), ty));
        }
        init.expr = Box::new(parse_quote!(__props.#ident));
    }

    // `let x = derived!(expr);` e `effect!(expr);` viram bindings reativas.
    let mut derived_names: BTreeSet<String> = BTreeSet::new();
    let mut effect_exprs: BTreeMap<usize, Expr> = BTreeMap::new();
    for (i, stmt) in body.iter_mut().enumerate() {
        match stmt {
            Stmt::Local(Local {
                init: Some(init), ..
            }) if macro_named(&init.expr, "derived").is_some() => {
                let expr = macro_arg(macro_named(&init.expr, "derived").unwrap(), "derived!")?;
                init.expr = Box::new(expr);
                let name = simple_decl(stmt).ok_or_else(|| {
                    anyhow!(
                        "`derived!` precisa de `let nome = derived!(...)` ou `let nome: T = ...`"
                    )
                })?;
                derived_names.insert(name);
            }
            Stmt::Macro(m) if m.mac.path.is_ident("effect") => {
                effect_exprs.insert(i, macro_arg(&m.mac, "effect!")?);
            }
            Stmt::Expr(Expr::Macro(m), _) if m.mac.path.is_ident("effect") => {
                effect_exprs.insert(i, macro_arg(&m.mac, "effect!")?);
            }
            _ => {}
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

    // Variáveis sem `mut` (e as derivadas) não podem ser alteradas pelo script.
    let immutable: BTreeSet<String> = body
        .iter()
        .filter(|s| simple_decl(s).is_some_and(|n| candidates.contains(&n)))
        .filter_map(|s| match s {
            Stmt::Local(l) => Some(l),
            _ => None,
        })
        .filter_map(|l| {
            let pat = match &l.pat {
                Pat::Type(t) => &*t.pat,
                p => p,
            };
            match pat {
                Pat::Ident(PatIdent {
                    ident,
                    mutability: None,
                    ..
                }) => Some(ident.to_string()),
                _ => None,
            }
        })
        .chain(derived_names.iter().cloned())
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
        let mut rewriter = Rewriter::new(active, &immutable);
        rewriter.visit_block_mut(&mut f.block);
        used_by_fn.push(rewriter.used);
    }

    // Cada binding de região ou de `bind:` (variável → DOM) é um bloco que atualiza o DOM.
    // Na página os ids são fixos; num componente são relativos a `__dom`, alocado por instância.
    let dom_id = |id: usize| -> Expr {
        match mode {
            Mode::Page(_) => parse_quote!(#id),
            Mode::Component(_) => parse_quote!(__dom + #id),
        }
    };
    // `used` são as variáveis capturadas pelo bloco; `deps` só as que ele lê (as passadas a
    // `bind:` num componente filho não o fazem reexecutar).
    let bind_error: std::cell::RefCell<Option<String>> = Default::default();
    let rewritten = |mut block: Block| {
        let mut rewriter = Rewriter::new(candidates.clone(), &immutable);
        rewriter.visit_block_mut(&mut block);
        if let Some(e) = rewriter.error.take() {
            bind_error.borrow_mut().get_or_insert(e);
        }
        let deps = rewriter.used.clone();
        let used = deps.union(&rewriter.bound).cloned().collect();
        (block, used, deps)
    };
    let mut region_blocks: Vec<RegionBlock> = Vec::new();
    let mut comp_regions: Vec<(usize, Block, BTreeSet<String>, BTreeSet<String>)> = Vec::new();
    let mut comp_binds: Vec<(usize, Block, BTreeSet<String>, BTreeSet<String>)> = Vec::new();
    for region in regions {
        let (id, render) = (region.id, &region.render);
        if is_component {
            let (block, used, deps) = rewritten(render.clone());
            comp_regions.push((id, block, used, deps));
        } else {
            region_blocks.push(rewritten(parse_quote!({ __wk_set(#id, &#render); })));
        }
    }
    for bind in binds {
        let (prop, target) = (&bind.prop, &bind.target);
        let dom = dom_id(bind.id);
        let (block, used, deps) = rewritten(parse_quote!({
            __wk_set_prop(#dom, #prop, &::wasm_bindgen::JsValue::from(::std::clone::Clone::clone(&(#target))));
        }));
        if is_component {
            comp_binds.push((bind.id, block, used, deps));
        } else {
            region_blocks.push((block, used, deps));
        }
    }

    let mut derived_regs: BTreeMap<String, (Expr, BTreeSet<String>)> = BTreeMap::new();
    for stmt in &body {
        if let (
            Some(name),
            Stmt::Local(Local {
                init: Some(init), ..
            }),
        ) = (simple_decl(stmt), stmt)
            && derived_names.contains(&name)
        {
            let mut expr = (*init.expr).clone();
            let mut rewriter = Rewriter::new(candidates.clone(), &immutable);
            rewriter.visit_expr_mut(&mut expr);
            if rewriter.used.contains(&name) {
                return Err(anyhow!("`{name}` depende de si mesma em `derived!`"));
            }
            derived_regs.insert(name, (expr, rewriter.used));
        }
    }
    let mut effect_regs: BTreeMap<usize, (Expr, BTreeSet<String>)> = BTreeMap::new();
    for (i, expr) in &effect_exprs {
        let mut expr = expr.clone();
        let mut rewriter = Rewriter::new(candidates.clone(), &immutable);
        rewriter.visit_expr_mut(&mut expr);
        effect_regs.insert(*i, (expr, rewriter.used));
    }

    if let Some(e) = bind_error.into_inner() {
        return Err(anyhow!(e));
    }

    let promoted: BTreeSet<String> = used_by_fn
        .iter()
        .chain(region_blocks.iter().map(|(_, used, _)| used))
        .chain(comp_regions.iter().map(|(_, _, used, _)| used))
        .chain(comp_binds.iter().map(|(_, _, used, _)| used))
        .chain(derived_regs.values().map(|(_, used)| used))
        .chain(effect_regs.values().map(|(_, used)| used))
        .flatten()
        .chain(&derived_names)
        .chain(&bound_props)
        .cloned()
        .collect();
    let has_dom = !regions.is_empty() || !binds.is_empty();
    let has_state = !promoted.is_empty() || has_dom || !effect_regs.is_empty();
    let prop_names: BTreeSet<String> = props.iter().map(|(n, _)| n.to_string()).collect();

    let mut rewriter = Rewriter::new(promoted.clone(), &immutable);
    let mut main_stmts: Vec<Stmt> = Vec::new();
    let mut decl_end: BTreeMap<String, usize> = BTreeMap::new();
    let mut registrations: Vec<(usize, Stmt)> = Vec::new();
    for (i, mut stmt) in body.into_iter().enumerate() {
        if let Some((expr, used)) = effect_regs.get(&i) {
            let at = used.iter().filter_map(|v| decl_end.get(v)).max().copied();
            registrations.push((
                at.unwrap_or(0).max(main_stmts.len()),
                effect_binding(expr, used),
            ));
            continue;
        }
        match simple_decl(&stmt).filter(|n| promoted.contains(n)) {
            Some(name) => {
                stmt = share_declaration(stmt, &mut rewriter, bound_props.contains(&name));
                let end = main_stmts.len() + 1;
                decl_end.insert(name.clone(), end);
                if let Some((expr, used)) = derived_regs.get(&name) {
                    registrations.push((end, derived_binding(&name, expr, used)));
                }
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
    let mut statics: Vec<TokenStream> = Vec::new();
    let mut wrappers: Vec<TokenStream> = Vec::new();
    for (f, used) in exported.iter().zip(&used_by_fn) {
        if is_component {
            registrations.push((after_decls(used), handler_registration(f, used)));
            continue;
        }
        let (registration, static_def, wrapper) = closure_parts(f, used, has_state);
        registrations.push((after_decls(used), registration));
        statics.push(static_def);
        wrappers.push(wrapper);
    }
    for (block, used, deps) in &region_blocks {
        registrations.push((after_decls(used), region_binding(block, used, deps)));
    }
    for (id, render, used, deps) in &comp_regions {
        for stmt in component_region(*id, render, used, deps) {
            registrations.push((after_decls(used), stmt));
        }
    }
    for (id, update, used, deps) in &comp_binds {
        registrations.push((after_decls(used), component_bind(*id, update, used, deps)));
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

    let attr: TokenStream = WASM_BINDGEN_ATTR.parse().unwrap();
    let tokens = match mode {
        Mode::Page(main_name) => {
            let main_ident = format_ident!("{main_name}");
            let reserve = if has_dom {
                let n = regions.len().max(binds.len());
                quote!(__wk_dom_reserve(#n);)
            } else {
                quote!()
            };
            quote! {
                #[allow(unused_imports)]
                use crate::__wk_rt::*;
                #(#hoisted)*
                #(#statics)*
                #(#wrappers)*
                #attr
                pub fn #main_ident() {
                    #reserve
                    #(#final_body)*
                }
            }
        }
        Mode::Component(parts) => {
            let (field, ty): (Vec<_>, Vec<_>) = props.iter().map(|(n, t)| (n, t)).unzip();
            let n = regions.len().max(binds.len());
            let html = component_html(parts, binds);
            let updated: Vec<_> = props
                .iter()
                .filter(|(n, _)| {
                    promoted.contains(&n.to_string()) && !bound_props.contains(&n.to_string())
                })
                .map(|(n, _)| n)
                .collect();
            let shared: BTreeSet<String> = updated.iter().map(|n| n.to_string()).collect();
            let clones = clone_names(&shared);
            let region_names: Vec<_> = (0..regions.len())
                .map(|k| format_ident!("__r{k}"))
                .collect();
            let _ = prop_names;
            quote! {
                #[allow(unused_imports)]
                use crate::__wk_rt::*;
                #(#hoisted)*
                pub struct Props {
                    #(pub #field: #ty,)*
                }
                pub fn __wk_create(__props: Props) -> __WkInstance<Props> {
                    let __inst = __wk_owner();
                    let __dom = __wk_dom_alloc(#n);
                    #(#final_body)*
                    let __update: ::std::rc::Rc<dyn Fn(Props)> = {
                        #(#clones)*
                        ::std::rc::Rc::new(move |__p: Props| {
                            #(__wk_set_derived(&#updated, __p.#updated);)*
                        })
                    };
                    let __html: ::std::rc::Rc<dyn Fn() -> ::std::string::String> = {
                        #(let #region_names = #region_names.clone();)*
                        ::std::rc::Rc::new(move || {
                            let mut __out = ::std::string::String::new();
                            #html
                            __out
                        })
                    };
                    __WkInstance { update: __update, html: __html }
                }
            }
        }
    };
    let file = syn::parse2::<syn::File>(tokens).map_err(|e| anyhow!("erro interno: {e}"))?;
    Ok(prettyplease::unparse(&file))
}

/// Função chamada por atributos de evento de um componente: fica registrada na instância,
/// que o HTML alcança por `__wk.c(id).nome(...)`.
fn handler_registration(f: &ItemFn, used: &BTreeSet<String>) -> Stmt {
    let name = f.sig.ident.to_string();
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
    let indexes = 0..pats.len();
    let clones = clone_names(used);
    parse_quote!({
        #(#clones)*
        let __f = move |#(#pats: #tys),*| -> #ret #block;
        __wk_register(
            __inst,
            #name,
            ::std::rc::Rc::new(move |__args: &[::wasm_bindgen::JsValue]| {
                let _ = __f(#(__wk_arg(__args, #indexes)),*);
            }),
        );
    })
}

/// Região de um componente: `__rN` gera o HTML (usado ao criar e ao reexibir a instância) e a
/// binding o reaplica quando suas variáveis mudam.
fn component_region(
    id: usize,
    render: &Block,
    used: &BTreeSet<String>,
    deps: &BTreeSet<String>,
) -> [Stmt; 2] {
    let name = format_ident!("__r{id}");
    let clones = clone_names(used);
    let ids = deps.iter().map(|v| format_ident!("{v}"));
    [
        parse_quote!(
            let #name: ::std::rc::Rc<dyn Fn() -> ::std::string::String> = {
                #(#clones)*
                let __pool = __wk_pool();
                ::std::rc::Rc::new(move || -> ::std::string::String #render)
            };
        ),
        parse_quote!({
            let __render = #name.clone();
            __wk_bind_lazy(&[#(#ids.id()),*], move || __wk_set(__dom + #id, &__render()));
        }),
    ]
}

/// `bind:` de um componente: a variável → DOM roda quando ela muda e uma vez quando o DOM da
/// instância é inserido (`__wk_init_N`, chamada pelo JS).
fn component_bind(
    id: usize,
    update: &Block,
    used: &BTreeSet<String>,
    deps: &BTreeSet<String>,
) -> Stmt {
    let clones = clone_names(used);
    let ids: Vec<_> = deps.iter().map(|v| format_ident!("{v}")).collect();
    let init = format!("__wk_init_{id}");
    parse_quote!({
        let __deps = [#(#ids.id()),*];
        #(#clones)*
        let __update: ::std::rc::Rc<dyn Fn()> = ::std::rc::Rc::new(move || #update);
        let __init = __update.clone();
        __wk_register(
            __inst,
            #init,
            ::std::rc::Rc::new(move |_: &[::wasm_bindgen::JsValue]| __init()),
        );
        __wk_bind_lazy(&__deps, move || __update());
    })
}

/// Monta, em `__out`, o HTML de uma instância: texto estático, regiões e atributos de `bind:`.
fn component_html(parts: &[Part], binds: &[Bind]) -> TokenStream {
    let mut out = TokenStream::new();
    for part in parts {
        out.extend(match part {
            Part::Text(text) => text_tokens(text),
            Part::Region(id) => {
                let name = format_ident!("__r{id}");
                quote!(
                    __out.push_str(&::std::format!("<!--wk:{}-->", __dom + #id));
                    __out.push_str(&#name());
                    __out.push_str(&::std::format!("<!--/wk:{}-->", __dom + #id));
                )
            }
            Part::Bind(id) => {
                let bind = &binds[*id];
                let (prop, event) = (&bind.prop, &bind.event);
                quote!(
                    __out.push_str(&::std::format!(
                        "data-wk-b{}=\"{}|{}|{}|{}\"",
                        __dom + #id,
                        __inst,
                        #id,
                        #prop,
                        #event
                    ));
                )
            }
        });
    }
    out
}

/// Registro de uma região do template: renderiza agora e quando suas variáveis mudarem.
fn region_binding(block: &Block, used: &BTreeSet<String>, deps: &BTreeSet<String>) -> Stmt {
    let names: Vec<_> = used.iter().map(|v| format_ident!("{v}")).collect();
    let ids: Vec<_> = deps.iter().map(|v| format_ident!("{v}")).collect();
    parse_quote!({
        let __deps = [#(#ids.id()),*];
        #(let #names = #names.clone();)*
        let __pool = __wk_pool();
        __wk_bind(&__deps, move || #block);
    })
}

type RegionBlock = (Block, BTreeSet<String>, BTreeSet<String>);

fn macro_named<'a>(expr: &'a Expr, name: &str) -> Option<&'a Macro> {
    match expr {
        Expr::Macro(m) if m.mac.path.is_ident(name) => Some(&m.mac),
        _ => None,
    }
}

/// Argumento de `derived!(...)`/`effect!(...)`; `|| corpo` vale como `corpo`.
fn macro_arg(mac: &Macro, what: &str) -> Result<Expr> {
    let expr: Expr = mac
        .parse_body()
        .map_err(|e| anyhow!("argumento inválido em `{what}`: {e}"))?;
    Ok(match expr {
        Expr::Closure(c) if c.inputs.is_empty() => *c.body,
        e => e,
    })
}

fn clone_names(names: &BTreeSet<String>) -> Vec<TokenStream> {
    names
        .iter()
        .map(|v| {
            let v = format_ident!("{v}");
            quote!(let #v = #v.clone();)
        })
        .collect()
}

/// `let nome = derived!(expr)`: recalcula `nome` quando as variáveis de `expr` mudam.
fn derived_binding(name: &str, expr: &Expr, used: &BTreeSet<String>) -> Stmt {
    let ident = format_ident!("{name}");
    let mut owned = used.clone();
    owned.insert(name.to_string());
    let clones = clone_names(&owned);
    let ids = used.iter().map(|v| format_ident!("{v}"));
    parse_quote!({
        #(#clones)*
        __wk_bind(&[#(#ids.id()),*], move || {
            __wk_set_derived(&#ident, #expr);
        });
    })
}

/// `effect!(expr)`: executa agora e de novo quando as variáveis de `expr` mudam.
fn effect_binding(expr: &Expr, used: &BTreeSet<String>) -> Stmt {
    let clones = clone_names(used);
    let ids = used.iter().map(|v| format_ident!("{v}"));
    parse_quote!({
        #(#clones)*
        __wk_bind(&[#(#ids.id()),*], move || {
            let _ = #expr;
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
fn share_declaration(stmt: Stmt, rewriter: &mut Rewriter, bound: bool) -> Stmt {
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
    if bound {
        let (name, ty) = match &pat {
            Pat::Type(t) => (ident_of(&t.pat), &t.ty),
            _ => unreachable!("`bind!()` exige o tipo"),
        };
        return parse_quote!(let #name: __WkShared<#ty> = #value;);
    }
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
    /// Variáveis declaradas sem `mut`: nunca passam por `get_mut`.
    immutable: BTreeSet<String>,
    used: BTreeSet<String>,
    /// Variáveis passadas por `bind:name={x}` a um componente: capturadas, mas não lidas.
    bound: BTreeSet<String>,
    error: Option<String>,
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
    fn new(active: BTreeSet<String>, immutable: &BTreeSet<String>) -> Self {
        Self {
            active,
            immutable: immutable.clone(),
            used: BTreeSet::new(),
            bound: BTreeSet::new(),
            error: None,
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
        if let Expr::Macro(m) = e
            && m.mac.path.is_ident("__wk_bind")
        {
            let name = m.mac.tokens.to_string();
            if !self.active.contains(&name) {
                self.error = Some(format!(
                    "`bind:` precisa de uma variável `let mut` do script, não `{name}`"
                ));
            } else if self.immutable.contains(&name) {
                self.error = Some(format!("`bind:` exige que `{name}` seja `let mut`"));
            }
            let id = format_ident!("{name}");
            self.bound.insert(name);
            *e = parse_quote!(#id.clone());
            return;
        }
        match e {
            Expr::Path(p)
                if p.qself.is_none()
                    && p.path
                        .get_ident()
                        .is_some_and(|id| self.active.contains(&id.to_string())) =>
            {
                let id = p.path.get_ident().unwrap().clone();
                self.used.insert(id.to_string());
                *e = if self.immutable.contains(&id.to_string()) {
                    parse_quote!((*#id.get_ref()))
                } else if mutable {
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
        let out = transform(
            code,
            &names,
            "__wk_main_t",
            &template.regions,
            &template.binds,
        )
        .unwrap();
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
        assert_eq!(out.matches("(*n.get_ref())").count(), 1);
    }

    #[test]
    fn rewrites_macro_arguments() {
        let out = run("let x = 1;\npub fn f() { println!(\"{}\", x); }", &["f"]);
        assert!(out.contains("(* x.get_ref())"));
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
        assert!(flat.contains("[n.id()]"));
        assert!(flat.contains("[]"));
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

#[cfg(test)]
mod reactive_tests {
    use super::*;

    fn run(code: &str, html: &str) -> Result<String> {
        let t = crate::template::compile(html).unwrap();
        transform(code, &BTreeSet::new(), "__wk_main_t", &t.regions, &t.binds)
    }

    #[test]
    fn derived_and_effect_become_bindings() {
        let out = run(
            "let mut a = 1; let b = derived!(a * 2); effect!(println!(\"{b}\"));",
            "{b}",
        )
        .unwrap();
        assert!(out.contains("__wk_set_derived(&b"));
        assert!(out.matches("__wk_bind(").count() >= 3);
        assert!(!out.contains("derived!("));
        assert!(!out.contains("effect!("));
    }

    #[test]
    fn self_reference_is_rejected() {
        assert!(run("let a = derived!(a + 1);", "").is_err());
    }
}

#[cfg(test)]
mod immutability_tests {
    use super::*;

    fn run(code: &str) -> String {
        transform(code, &["f".to_string()].into(), "__wk_main_t", &[], &[]).unwrap()
    }

    #[test]
    fn immutable_variables_never_use_get_mut() {
        let out = run("let n = 1; let mut m = 1; pub fn f() { m += n; }");
        assert!(out.contains("(*m.get_mut())"));
        assert!(out.contains("(*n.get_ref())"));
        assert!(!out.contains("n.get_mut"));
    }

    #[test]
    fn writing_an_immutable_variable_is_left_to_rustc_to_reject() {
        let out = run("let n = 1; pub fn f() { n += 1; }");
        assert!(out.contains("(*n.get_ref()) += 1"));
    }
}

#[cfg(test)]
mod bind_tests {
    use super::*;
    use crate::template;

    fn component(code: &str) -> Result<String> {
        let t = template::compile("<p>{n}</p>").unwrap();
        transform_component(code, &BTreeSet::new(), &t)
    }

    #[test]
    fn bind_prop_shares_the_parents_state() {
        let out = component("let mut n: i32 = bind!();").unwrap();
        assert!(out.contains("pub n: __WkShared<i32>"));
        assert!(out.contains("let n: __WkShared<i32> = __props.n;"));
        assert!(!out.contains("__wk_set_derived(&n"));
    }

    #[test]
    fn bind_requires_mut_and_prop_forbids_it() {
        assert!(component("let n: i32 = bind!();").is_err());
        assert!(component("let mut n: i32 = prop!();").is_err());
    }

    #[test]
    fn binding_a_non_state_variable_is_an_error() {
        let t = template::compile("{#each xs as x}<C bind:v={x} />{/each}").unwrap();
        let err = transform(
            "let xs = Vec::<i32>::new();",
            &BTreeSet::new(),
            "m",
            &t.regions,
            &t.binds,
        );
        assert!(err.is_err());
        let t = template::compile("<C bind:v={k} />").unwrap();
        let err = transform("let k = 1;", &BTreeSet::new(), "m", &t.regions, &t.binds);
        assert!(err.unwrap_err().to_string().contains("let mut"));
    }
}
