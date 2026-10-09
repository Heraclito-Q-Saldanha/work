// Runtime reativo gerado pelo cargo-wk. Este arquivo é copiado para `src/__wk_rt.rs` do projeto
// e compartilhado pelas páginas e componentes.
#![allow(dead_code, clippy::all)]

use ::std::{
    any::Any,
    cell::{Cell, RefCell, UnsafeCell},
    collections::{HashMap, HashSet},
    rc::Rc,
};
use ::wasm_bindgen::{JsValue, prelude::wasm_bindgen};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = __wk, js_name = set)]
    pub fn __wk_set(id: usize, html: &str);
    #[wasm_bindgen(js_namespace = __wk, js_name = set_prop)]
    pub fn __wk_set_prop(id: usize, prop: &str, value: &JsValue);
}

struct Binding {
    owner: usize,
    deps: Vec<usize>,
    render: Rc<dyn Fn()>,
}

thread_local! {
    static NEXT_ID: Cell<usize> = Cell::new(0);
    static NEXT_DOM: Cell<usize> = Cell::new(0);
    static NEXT_INSTANCE: Cell<usize> = Cell::new(1);
    // Dono das bindings registradas agora: 0 é a página, o resto são instâncias de componentes.
    static OWNER: Cell<usize> = Cell::new(0);
    static DIRTY: RefCell<Vec<usize>> = RefCell::new(Vec::new());
    static DERIVED: RefCell<Vec<usize>> = RefCell::new(Vec::new());
    static BINDINGS: RefCell<Vec<Binding>> = RefCell::new(Vec::new());
    // Instância viva -> dono dela (a instância ou a página que a criou).
    static INSTANCES: RefCell<HashMap<usize, usize>> = RefCell::new(HashMap::new());
    static HANDLERS: RefCell<HashMap<(usize, String), Rc<dyn Fn(&[JsValue])>>> =
        RefCell::new(HashMap::new());
}

// Estado compartilhado entre as funções do script. O wasm roda em uma única thread, então
// o acesso sem checagem de empréstimo é aceitável aqui.
pub struct __WkShared<T> {
    id: usize,
    cell: Rc<UnsafeCell<T>>,
}

impl<T> __WkShared<T> {
    pub fn new(value: T) -> Self {
        let id = NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id + 1);
            id
        });
        Self { id, cell: Rc::new(UnsafeCell::new(value)) }
    }

    pub fn id(&self) -> usize {
        self.id
    }

    // Leitura.
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.cell.get() }
    }

    // Leitura de variável imutável: o rustc rejeita escritas através do `&T`.
    pub fn get_ref(&self) -> &T {
        unsafe { &*self.cell.get() }
    }

    // Possível escrita: marca a variável como suja.
    #[allow(clippy::mut_from_ref)]
    pub fn get_mut(&self) -> &mut T {
        DIRTY.with(|d| d.borrow_mut().push(self.id));
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

fn register(deps: &[usize], render: Rc<dyn Fn()>) {
    let owner = OWNER.with(|o| o.get());
    BINDINGS.with(|b| b.borrow_mut().push(Binding { owner, deps: deps.to_vec(), render }));
}

// Renderiza uma vez e reexecuta quando alguma variável em `deps` for escrita.
pub fn __wk_bind(deps: &[usize], render: impl Fn() + 'static) {
    let pending = DIRTY.with(|d| d.borrow().len());
    render();
    DIRTY.with(|d| d.borrow_mut().truncate(pending));
    register(deps, Rc::new(render));
}

// Só registra: o conteúdo inicial já foi gerado por quem criou a binding.
pub fn __wk_bind_lazy(deps: &[usize], render: impl Fn() + 'static) {
    register(deps, Rc::new(render));
}

// Atualiza uma variável derivada; só propaga se o valor mudou.
pub fn __wk_set_derived<T: PartialEq>(slot: &__WkShared<T>, value: T) {
    if *slot.get() != value {
        *slot.get() = value;
        DERIVED.with(|d| d.borrow_mut().push(slot.id));
    }
}

fn is_alive(owner: usize) -> bool {
    owner == 0 || INSTANCES.with(|i| i.borrow().contains_key(&owner))
}

pub fn __wk_flush() {
    // Cada passada pode alterar variáveis derivadas, que sujam as bindings seguintes.
    for _ in 0..100 {
        let dirty = DIRTY.with(|d| ::std::mem::take(&mut *d.borrow_mut()));
        if dirty.is_empty() {
            return;
        }
        let stale: Vec<(usize, Rc<dyn Fn()>)> = BINDINGS.with(|b| {
            b.borrow()
                .iter()
                .filter(|b| b.deps.iter().any(|d| dirty.contains(d)))
                .map(|b| (b.owner, b.render.clone()))
                .collect()
        });
        for (owner, render) in stale {
            // Uma binding pode ter sido removida por uma passada anterior da mesma rodada.
            if !is_alive(owner) {
                continue;
            }
            let previous = OWNER.with(|o| o.replace(owner));
            render();
            OWNER.with(|o| o.set(previous));
        }
        // Renderizar só lê, mas métodos usados como receptor marcam a variável como suja.
        let changed = DERIVED.with(|d| ::std::mem::take(&mut *d.borrow_mut()));
        DIRTY.with(|d| *d.borrow_mut() = changed);
    }
    ::std::panic!("ciclo entre variáveis derived!/effect!");
}

pub fn __wk_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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

// Ids de DOM (âncoras de região e `bind:`) das páginas são fixos no HTML; as instâncias
// de componentes alocam os seus a partir do que sobra.
pub fn __wk_dom_reserve(count: usize) {
    NEXT_DOM.with(|n| n.set(n.get().max(count)));
}

pub fn __wk_dom_alloc(count: usize) -> usize {
    NEXT_DOM.with(|n| {
        let base = n.get();
        n.set(base + count);
        base
    })
}

// Id da instância de componente que está sendo criada ou renderizada.
pub fn __wk_owner() -> usize {
    OWNER.with(|o| o.get())
}

// ---------------------------------------------------------------------------
// Conversão dos argumentos vindos do JS

pub trait __WkFromJs: Sized {
    fn from_js(v: &JsValue) -> Option<Self>;
}

impl __WkFromJs for JsValue {
    fn from_js(v: &JsValue) -> Option<Self> {
        Some(v.clone())
    }
}

impl __WkFromJs for String {
    fn from_js(v: &JsValue) -> Option<Self> {
        v.as_string()
    }
}

impl __WkFromJs for bool {
    fn from_js(v: &JsValue) -> Option<Self> {
        v.as_bool()
    }
}

impl<T: __WkFromJs> __WkFromJs for Option<T> {
    fn from_js(v: &JsValue) -> Option<Self> {
        if v.is_null() || v.is_undefined() {
            Some(None)
        } else {
            T::from_js(v).map(Some)
        }
    }
}

macro_rules! __wk_number {
    ($($t:ty),*) => {$(
        impl __WkFromJs for $t {
            fn from_js(v: &JsValue) -> Option<Self> {
                let n = v
                    .as_f64()
                    .or_else(|| v.as_string().and_then(|s| s.trim().parse::<f64>().ok()))?;
                n.is_finite().then(|| n as $t)
            }
        }
    )*};
}
__wk_number!(f32, f64, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

pub fn __wk_arg<T: __WkFromJs>(args: &[JsValue], index: usize) -> T {
    args.get(index)
        .and_then(T::from_js)
        .unwrap_or_else(|| ::std::panic!("argumento {} inválido ou ausente", index + 1))
}

// ---------------------------------------------------------------------------
// Componentes

pub struct __WkInstance<P> {
    // Aplica as props novas, vindas do pai.
    pub update: Rc<dyn Fn(P)>,
    // HTML atual da instância, com as âncoras das suas regiões.
    pub html: Rc<dyn Fn() -> String>,
}

struct Entry {
    id: usize,
    instance: Rc<dyn Any>,
}

#[derive(Default)]
struct PoolData {
    entries: HashMap<(usize, String), Entry>,
    used: HashSet<(usize, String)>,
}

// Instâncias criadas por uma região do template, reaproveitadas entre renderizações.
#[derive(Clone, Default)]
pub struct __WkPool(Rc<RefCell<PoolData>>);

pub fn __wk_pool() -> __WkPool {
    __WkPool::default()
}

pub fn __wk_begin(pool: &__WkPool) {
    pool.0.borrow_mut().used.clear();
}

// Destrói as instâncias que a renderização que acabou não usou.
pub fn __wk_end(pool: &__WkPool) {
    let gone: Vec<Entry> = {
        let mut data = pool.0.borrow_mut();
        let used = ::std::mem::take(&mut data.used);
        let keys: Vec<_> = data.entries.keys().filter(|k| !used.contains(*k)).cloned().collect();
        keys.iter().filter_map(|k| data.entries.remove(k)).collect()
    };
    for entry in gone {
        destroy(entry.id);
    }
}

fn destroy(id: usize) {
    BINDINGS.with(|b| b.borrow_mut().retain(|b| b.owner != id));
    HANDLERS.with(|h| h.borrow_mut().retain(|(owner, _), _| *owner != id));
    let children: Vec<usize> = INSTANCES.with(|i| {
        let mut i = i.borrow_mut();
        i.remove(&id);
        i.iter().filter(|(_, parent)| **parent == id).map(|(c, _)| *c).collect()
    });
    for child in children {
        destroy(child);
    }
}

// Devolve o HTML da instância identificada por (`site`, `key`), criando-a se for nova.
pub fn __wk_use<P: 'static>(
    pool: &__WkPool,
    site: usize,
    key: &str,
    props: P,
    create: fn(P) -> __WkInstance<P>,
) -> String {
    let slot = (site, key.to_string());
    let existing = pool.0.borrow().entries.get(&slot).and_then(|e| {
        e.instance.clone().downcast::<__WkInstance<P>>().ok().map(|i| (e.id, i))
    });
    let (id, instance) = match existing {
        Some((id, instance)) => {
            (instance.update)(props);
            (id, instance)
        }
        None => {
            let id = NEXT_INSTANCE.with(|n| {
                let id = n.get();
                n.set(id + 1);
                id
            });
            let parent = OWNER.with(|o| o.get());
            INSTANCES.with(|i| i.borrow_mut().insert(id, parent));
            let previous = OWNER.with(|o| o.replace(id));
            let instance = Rc::new(create(props));
            OWNER.with(|o| o.set(previous));
            pool.0.borrow_mut().entries.insert(slot.clone(), Entry { id, instance: instance.clone() });
            (id, instance)
        }
    };
    pool.0.borrow_mut().used.insert(slot);
    let previous = OWNER.with(|o| o.replace(id));
    let html = (instance.html)();
    OWNER.with(|o| o.set(previous));
    html
}

pub fn __wk_register(owner: usize, name: &str, handler: Rc<dyn Fn(&[JsValue])>) {
    HANDLERS.with(|h| h.borrow_mut().insert((owner, name.to_string()), handler));
}

// Chamada pelos atributos `onclick="..."` etc. dos componentes (via `__wk.c(id).nome(...)`).
#[wasm_bindgen]
pub fn __wk_call(instance: usize, name: &str, args: Vec<JsValue>) {
    let handler = HANDLERS.with(|h| h.borrow().get(&(instance, name.to_string())).cloned());
    if let Some(handler) = handler {
        handler(&args);
        __wk_flush();
    }
}

// Executa a atualização inicial de um `bind:` de componente, sem flush: só aplica o valor ao DOM.
#[wasm_bindgen]
pub fn __wk_init(instance: usize, name: &str) {
    let handler = HANDLERS.with(|h| h.borrow().get(&(instance, name.to_string())).cloned());
    if let Some(handler) = handler {
        handler(&[]);
    }
}
