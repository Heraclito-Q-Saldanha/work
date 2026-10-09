# Arquitetura do `cargo-wk`

Guia para reimplementar o projeto do zero. Cada seção diz **o que é necessário** e **onde está no código**
(`arquivo:linha` aponta para o símbolo; as linhas mudam, o nome do símbolo é a referência estável).

Legenda de prioridade: **[núcleo]** sem isso nada funciona · **[reatividade]** · **[componentes]** · **[extra]** pode ficar para o fim.

Ordem sugerida de implementação: §3 → §4 → §6 (só página sem reatividade) → §7 → §8 → §9 → §10 → §11 → §12.

---

## 1. Visão geral

`cargo wk` é um subcomando cargo. Ele lê um projeto cargo cujo `src/` contém arquivos `.wk` (HTML + `<script lang="rs">`),
**transpila** tudo para um crate Rust comum numa pasta temporária, compila para `wasm32-unknown-unknown`, roda o
`wasm-bindgen` e escreve `dist/` (um `.wasm`, o `.js` de cola e um `index.html` por rota).

```
projeto/                      pasta temporária                    dist/
  Cargo.toml                    Cargo.toml (+ wasm-bindgen)         <crate>_bg.wasm
  src/                  ──▶     src/  (só .rs, lib.rs gerado) ──▶   <crate>.js
    routes/+page.wk               routes/+page.rs                   index.html
    routes/about/+page.wk         routes/about/+page.rs             about/index.html
    components/Counter.wk         components/Counter.rs
    x.wk.rs / y.rs                x.rs / y.rs
                                  __wk_rt.rs  (runtime copiado)
```

Pipeline por página: `parser` (extrai script e handlers) → `template` (HTML dinâmico → regiões/binds) →
`script` (reescreve o Rust do usuário) → `loader` (monta o HTML final) → `builder` (cargo + wasm-bindgen).

## 2. Bibliotecas usadas

Definidas em [Cargo.toml](../Cargo.toml).

| Crate | Onde | Para quê |
|---|---|---|
| `clap` (derive) | [main.rs:20-66](../src/main.rs) | Subcomandos `new`, `init`, `run`, `build`, `expand`. O enum `Cargo { Wk(..) }` com `bin_name = "cargo"` faz o binário `cargo-wk` funcionar como `cargo wk ...`. |
| `lol_html` | [parser.rs:14](../src/parser.rs) | Parser HTML *streaming*: seleciona `script[lang='rs']`, captura o texto, remove o elemento e lê atributos `on*`. Preserva o resto do HTML byte a byte (não normaliza). |
| `syn` (`full, parsing, visit, visit-mut`) | [script.rs](../src/script.rs), [template.rs](../src/template.rs), [rust.rs](../src/rust.rs) | Parsear o Rust do script (`Block::parse_within`), reescrever expressões (`VisitMut` → `Rewriter`, [script.rs:927](../src/script.rs)) e validar expressões do template. |
| `quote` + `proc-macro2` (`span-locations`) | [script.rs](../src/script.rs), [template.rs](../src/template.rs), [rust.rs](../src/rust.rs) | Gerar código (`quote!`/`parse_quote!`). `span-locations` dá linha/coluna de itens, usada em [rust.rs:31](../src/rust.rs) para edição *textual* de `.wk.rs`. |
| `prettyplease` | fim de `transform_impl` ([script.rs](../src/script.rs)) | Formata o `syn::File` gerado. Efeito colateral: comentários do script se perdem. |
| `walkdir` | [transpile.rs:275](../src/transpile.rs), [serve.rs](../src/serve.rs), [manifest.rs](../src/manifest.rs) | Copiar projeto, listar arquivos, snapshot do watcher, achar `async`. |
| `tempfile` | [main.rs:114](../src/main.rs) | Diretório temporário do crate transpilado. |
| `toml` | [manifest.rs:62](../src/manifest.rs) | Ler o nome do pacote e checar `[dependencies]`. |
| `serde_json` | [builder.rs:18](../src/builder.rs), [loader.rs](../src/loader.rs) | Ler as mensagens `--message-format=json` do cargo (achar o `.wasm`); serializar a lista de binds para o JS. |
| `wasm-bindgen-cli-support` (`=0.2.129`) | [builder.rs:56](../src/builder.rs) | Rodar o wasm-bindgen **como biblioteca** (`Bindgen::new().web(true)`), sem exigir o CLI instalado. A versão **tem que ser igual** à do `wasm-bindgen` do projeto gerado ([builder.rs:13](../src/builder.rs), [manifest.rs:21](../src/manifest.rs)). |
| `anyhow` | tudo | Erros com contexto. |

No crate **gerado** (não no `cargo-wk`): `wasm-bindgen` (adicionado por `cargo add`, [manifest.rs:21](../src/manifest.rs)) e,
se houver `async`, `wasm-bindgen-futures` ([manifest.rs:21-60](../src/manifest.rs)). O servidor de `run` usa só `std`.

Ferramentas externas necessárias: `cargo`, `rustup target add wasm32-unknown-unknown`.

## 3. Estrutura do projeto e tipos de arquivo **[núcleo]**

Classificação em `FileKind::of` ([transpile.rs:177](../src/transpile.rs)):

| Arquivo | Tipo | Resultado |
|---|---|---|
| `src/routes/**/+page.wk` | Página | `<dir>/index.html` + `+page.rs` (se tem script ou parte dinâmica) |
| `src/**/Nome.wk` fora de `routes/` (inicial maiúscula) | Componente | `Nome.rs` (módulo com `Props` e `__wk_create`); sem HTML |
| `src/**/foo.wk.rs` | RustOnly | `foo.rs`, com `#[wasm_bindgen]` nas funções chamadas por qualquer handler de qualquer página |
| qualquer `.rs` | Other | copiado intacto |
| `src/lib.rs` | — | **proibido** (gerado); erro em [transpile.rs:34](../src/transpile.rs) |

Regras extras: `.wk` que não é `+page.wk` dentro de `routes/` é erro; nome de componente precisa ser identificador com
maiúscula ([transpile.rs:149](../src/transpile.rs)).

## 4. Transpilação do projeto **[núcleo]**

`transpile_project(root, dest, glue_name)` — [transpile.rs:34](../src/transpile.rs). Passos:

1. `copy_project` ([transpile.rs:275](../src/transpile.rs)): copia tudo menos `target`, `dist`, `.git` (`IGNORED_DIRS`).
2. Lista os arquivos de `dest/src` e despacha por `FileKind`.
3. **Página**: `parser::parse` → acumula `handlers` (união global) → `template::compile` → se há script **ou** região/bind,
   `script::transform(...)` e grava `+page.rs` ([write_rs, transpile.rs:261](../src/transpile.rs)) → apaga o `.wk` →
   `loader::render_page` gera o HTML. O caminho da saída é `<dir relativo a routes/>/index.html`.
   O nome do export `main` da página vem de `main_fn_name` ([transpile.rs:155](../src/transpile.rs)): `__wk_main_<caminho sanitizado>`
   (`/` vira `__`), único por página porque todas as páginas vivem no mesmo wasm.
4. **Componente**: `component_code` ([transpile.rs:125](../src/transpile.rs)) → `script::transform_component`. Só as funções
   do próprio script que aparecem em handlers do template são "redirecionadas" para a instância.
5. **RustOnly**: `rust::transform` ([rust.rs:9](../src/rust.rs)): edição textual por offsets (insere `#[wasm_bindgen]` antes das `fn`
   cujo nome está em `handlers`; troca `extern "js"` por `#[wasm_bindgen] extern "C"`). Roda **depois** das páginas porque precisa do
   conjunto completo de `handlers`.
6. Escreve `src/__wk_rt.rs` com `include_str!("runtime/wk_rt.rs")` ([transpile.rs:12,119](../src/transpile.rs)).
7. `write_modules` ([transpile.rs:197](../src/transpile.rs)): gera `src/lib.rs` com `pub mod x;` para cada `.rs`/pasta com Rust
   (`module_decls`, [transpile.rs:210](../src/transpile.rs)); gera `mod.rs` em subpastas que não tenham; `routes/` **não** vira módulo
   — cada página entra como `#[path = "routes/.../+page.rs"] mod __route_<nome>;` (o nome `+page` não é um módulo válido).
   Nomes com maiúscula ganham `#[allow(non_snake_case)]`.

Dica: escrever o `expand` ([main.rs:76](../src/main.rs)) cedo — é só `transpile_project` num diretório fixo; é a ferramenta de debug mais útil.

## 5. Build e empacotamento **[núcleo]**

[main.rs:114 `build_project`](../src/main.rs):

1. `tempdir` → `transpile_project`.
2. `manifest::ensure_wasm_bindgen` ([manifest.rs:21](../src/manifest.rs)): `cargo add wasm-bindgen@=<versão>` (e `wasm-bindgen-futures` se algum `.rs` gerado contém `async`, ignorando `__wk_rt.rs`).
3. `builder::compile_wasm` ([builder.rs:18](../src/builder.rs)): `cargo rustc --release --lib --target wasm32-unknown-unknown --crate-type cdylib --message-format=json-render-diagnostics`,
   com `CARGO_TARGET_DIR=<projeto>/target/wk` (cache compartilhado entre builds, senão cada build recompila do zero porque a pasta é temporária).
   Lê o stdout linha a linha procurando `reason == "compiler-artifact"` com arquivo `.wasm`. `json-render-diagnostics` mantém os erros do rustc legíveis no stderr.
4. `builder::package` ([builder.rs:56](../src/builder.rs)): recria `dist/`, roda `Bindgen::...web(true)` (gera `<crate>.js` e `<crate>_bg.wasm`), escreve os `Page`s.

`manifest::lib_name` ([manifest.rs:8](../src/manifest.rs)) = `package.name` com `-` → `_` (é o nome dos arquivos do wasm-bindgen).

Outros comandos: `new`/`init` em [scaffold.rs](../src/scaffold.rs) (escrevem `Cargo.toml`, `routes/+page.wk`, `.gitignore`);
`run` em [main.rs:95](../src/main.rs) + [serve.rs](../src/serve.rs) (servidor HTTP com `TcpListener` em thread, e *polling* de (mtime, tamanho) a cada 300 ms
com debounce de 150 ms; erro de build não derruba o loop). **[extra]**

## 6. Parser do `.wk` **[núcleo]**

[parser.rs:14 `parse`](../src/parser.rs) — uma passada do `lol_html::rewrite_str`:

- `element!("script[lang='rs']")`: abre um buffer novo e **remove** o elemento. `text!(...)`: acumula o conteúdo (o texto pode vir em vários pedaços).
- `element!("*")`: para cada atributo que começa com `on`, `called_functions` ([parser.rs:55](../src/parser.rs)) extrai `nome(` ignorando `obj.nome(`.
  Esses nomes (`handlers`) são as funções que o JS precisa alcançar.
- Saída: `ParsedWk { html (sem os scripts), rust (múltiplos scripts concatenados com \n, ou None), handlers }`. Sem script não é erro.

## 7. Compilador de template **[núcleo da reatividade]**

Entrada: o HTML restante. Saída: `Template { html, parts, regions, binds }` ([template.rs:62](../src/template.rs)).
Arquivo: [template.rs](../src/template.rs). Fases em `compile_with` ([template.rs:128](../src/template.rs)):

1. **Lexer** `tokenize` ([template.rs:333](../src/template.rs)) — escrito à mão porque o HTML tem `{...}` em lugares onde parsers HTML não deixam. Mantém `in_tag`, `quote_char`
   e `raw_text_element` (`script`/`style`: não interpreta `{}` lá). Reconhece:
   - `{expr}` → `Tok::Interp`; `{#if c}`/`{:else if c}`/`{:else}`/`{/if}`; `{#each it as pat[, i] [by chave]}`/`{/each}` (`directive`, [template.rs:633](../src/template.rs); `parse_each`, [template.rs:738](../src/template.rs)).
   - `{{` e `}}` = chaves literais. `read_braced` ([template.rs:676](../src/template.rs)) conta chaves aninhadas e ignora strings.
   - `bind:prop[|evento]={alvo}` dentro de uma tag (`Lexer::bind`, [template.rs:473](../src/template.rs)): registra um `Bind { id, prop, event, target }`, valida que o alvo é atribuível
     (`is_assignable`, [template.rs:719](../src/template.rs)) e deixa no HTML um marcador `\u{1}b<id>\u{2}` no lugar do atributo. Evento padrão por propriedade em `default_event` ([template.rs:71](../src/template.rs)): `value`→`input`, `checked`→`change`, etc.
     Limitação: só no nível superior (`depth == 0`).
   - `<Maiúscula .../>` → `Tok::Component` (`Lexer::component`, [template.rs:547](../src/template.rs)): props `a={expr}` (vira `Clone::clone(&(expr))`), `a="txt"` (`Into::into("txt")`), `{atalho}`, `bind:a={var}` / `bind:a` (vira o marcador `__wk_bind!(var)`, ver §10).
2. **Árvore** `parse_nodes`/`parse_if` ([template.rs:806,854](../src/template.rs)) transformam tokens em `Node` (`Raw`, `Interp(Expr)`, `If`, `Each`, `Component`). As expressões passam por `syn::parse_str`, então erros de sintaxe aparecem na transpilação.
3. **Regiões**: cada nó **dinâmico de nível superior** (`Interp`, `If`, `Each`, `Component`) vira uma `Region { id, render: Block }` e deixa no HTML um par de comentários
   `<!--wk:ID--><!--/wk:ID-->`; texto estático vira `Part::Text`. O `render` é um bloco Rust que monta uma `String __out` — gerado por `gen_nodes` ([template.rs:904](../src/template.rs)):
   - `Raw` → `__out.push_str("...")` (`text_tokens`, [template.rs:221](../src/template.rs));
   - `Interp(e)` → `__out.push_str(&__wk_escape(&e.to_string()))` (escapa HTML);
   - `If` → `if/else if/else` do Rust; `Each` → `for pat in &iter` (ou `for (i, pat) in (&iter).into_iter().enumerate()` com índice; chamadas/ranges/macros iteram por valor, `by_value`); dentro de componentes acrescenta `__key = "{pai}/{chave}"`;
   - `Component` → `__wk_use(&__pool, SITE, &__key, Nome::Props{..}, Nome::__wk_create)` (§11).
4. `parts` guarda a sequência `Text | Region(id) | Bind(id)`; `html` é só `parts` concatenado (`data-wk-b<id>` para binds). Em componentes o `html` é montado em runtime a partir de `parts` (`component_html`, [script.rs:635](../src/script.rs)).

Escolha de projeto: a granularidade é a **região de nível superior** — mudou uma variável usada nela, o HTML inteiro da região é regerado e substituído (não há diff de DOM).

## 8. Reescrita do script (o coração) **[núcleo]**

[script.rs:86 `transform_impl`](../src/script.rs). Entrada: código Rust do usuário + `names` (funções de handler) + regiões/binds do template. Saída: um arquivo Rust.

### 8.1 Separar o script em categorias ([script.rs:86-135](../src/script.rs))
Parseia com `Block::parse_within` (o script é uma lista de statements, não um arquivo). Cada statement vai para:

- **`exported`** — `fn` cujo nome está em `names` e é "closurable" (`is_closurable`, [script.rs:737](../src/script.rs): sem genéricos nem `self`; `async` **é** permitido). Viram closures (§8.5).
  Se não é closurable: em página vira função solta com `#[wasm_bindgen]` (`hoisted`, não enxerga variáveis); em componente é erro.
- **`hoisted`** — qualquer outro item (`struct`, `use`, `fn` não chamada por handler, `extern "js"`→`extern "C"` via `convert_extern_js`, [script.rs:743](../src/script.rs)). Ficam no nível do módulo.
- **`body`** — o resto (`let`, expressões, macros). Vai para dentro da função `main`/`__wk_create`.
- Para cada `bind:` do template cria-se uma `fn` sintética `__wk_bind_...(JsValue)` que atribui `alvo = valor` (§10). Entra em `exported`.

### 8.2 Macros especiais ([script.rs:143-236](../src/script.rs))
Detectadas em `Stmt::Local`/`Stmt::Macro` por `macro_named` ([script.rs:681](../src/script.rs)):
`let x = derived!(expr)` (vira `let x = expr` + binding, §8.6), `effect!(expr);` (binding sem destino), `prop!()`/`bind!()` (componentes, §10/§11).
Parseadas como `syn::Macro`, o argumento reparseado por `macro_arg` ([script.rs:689](../src/script.rs)). **Não** usei `$derive` porque `$` não parseia em `syn`; macros com `!` parseiam.

### 8.3 Quais variáveis viram estado ([script.rs:238-278](../src/script.rs))
- `candidates`: `let` com padrão simples (`simple_decl`, [script.rs:754](../src/script.rs)) **declarados uma única vez** (shadowing desqualifica).
- `immutable`: candidatos sem `mut` + os `derived!`. Nunca passam por `get_mut`.
- Só é **promovida** a `__WkShared<T>` a variável realmente usada por uma `fn` exportada, região, bind, derived ou effect (`promoted`, [script.rs:370](../src/script.rs)). Variável não usada pelo template/funções continua um `let` normal (zero custo).

### 8.4 O `Rewriter` ([script.rs:927-1120](../src/script.rs)) — `VisitMut` que troca usos de variável por acessos ao estado

| Posição da variável | Vira | Efeito |
|---|---|---|
| leitura | `(*x.get())` | só lê |
| lado esquerdo de `=`/`+=`, receptor de método (`x.push(..)`), `&mut x`, campo/índice/deref de qualquer um desses | `(*x.get_mut())` | marca `x` como **suja** |
| variável imutável | `(*x.get_ref())` | devolve `&T`; escrever vira erro do rustc ("mutar imutável") |

A flag `self.mutable` é lida e zerada a cada nó (`std::mem::replace`) e é propagada por `visit_as(true, ..)` só pelos nós que mantêm "lugar" (Field, Index, Paren, Deref).
Também: respeita **sombreamento** (`scoped`/`shadow` em blocos, closures, `match`, `for`, `if let`...), olha dentro de macros de formato (`visit_macro_mut`, `inline_format_args` pega `"{nome}"`), e acumula `used` = variáveis tocadas (isso vira as dependências e os `clone`s).
Receptor de método é sempre `get_mut` (não dá para saber se `push` muta); por isso o flush tolera falsos positivos (renderizar de novo é barato) — ver `DERIVED` em §9.

A declaração em si: `share_declaration` ([script.rs:778](../src/script.rs)): `let x = v;` → `let x = __WkShared::new(v);`.

### 8.5 Funções de handler → closures ([script.rs:825 `closure_parts`](../src/script.rs))
Ideia: a `fn` do usuário vira uma **closure guardada num `thread_local`**, e o export wasm é um wrapper fino que a chama. Assim o corpo captura as variáveis (clones de `__WkShared`).
Para `pub fn happy(a: T) -> R { body }` na página, gera:

```rust
thread_local! { static __WK_FN_HAPPY: RefCell<Option<Box<dyn Fn(T)->R>>> = RefCell::new(None); }
#[wasm_bindgen] pub fn happy(__a0: T) -> R {
    let r = __WK_FN_HAPPY.with(|f| (f.borrow().as_ref().expect("..."))(__a0));
    __wk_flush();          // só se a página tem estado
    r
}
// dentro de __wk_main_X():
{ let count = count.clone(); /* um clone por variável usada */
  let f: Box<dyn Fn(T)->R> = Box::new(move |a: T| -> R { body });
  __WK_FN_HAPPY.with(|s| *s.borrow_mut() = Some(f)); }
```
`async fn`: a closure devolve `Pin<Box<dyn Future>>` (`async move {body}`) e o wrapper é `pub async fn` que espera `__wk_flushing(future)` (runtime, §9) — dá flush a cada `poll`, então estados intermediários (`status = "loading"` antes de um `.await`) aparecem.
O `expect` cobre o caso do usuário clicar antes de `main` terminar.

### 8.6 Ordem de registro ([script.rs:343-420](../src/script.rs))
Os statements originais ficam na ordem. Cada registro (closure, região, derived, effect) é inserido **logo depois da última declaração** de uma variável que ele usa (`decl_end` / `after_decls`, [script.rs:413](../src/script.rs)).
Isso é necessário porque o `clone()` só existe depois do `let`. Por fim `__wk_flush();` é o último statement se `has_state`.

### 8.7 Derived/effect ([script.rs:709,725](../src/script.rs))
```rust
// let d = derived!(a * 2);   →   let d = __WkShared::new(a*2);  e logo depois:
{ let a = a.clone(); let d = d.clone();
  __wk_bind(&[a.id()], move || { __wk_set_derived(&d, <expr reescrita>); }); }
// effect!(e);  →  __wk_bind(&[deps], move || { let _ = e; });
```
`__wk_bind` executa uma vez na hora (registrando o valor inicial) e guarda o closure como dependente dessas ids. `__wk_set_derived` exige `PartialEq` e só propaga se mudou.

### 8.8 Regiões e binds da página ([script.rs:293-330, 668](../src/script.rs), `region_binding`)
- Região: `__wk_bind(&[deps], move || { __wk_set(ID, &{render}); })`. `__wk_set` é uma função **importada do JS** (§9/§12) que troca o conteúdo entre os comentários.
- Bind variável→DOM: `__wk_bind(&[deps], || __wk_set_prop(ID, "value", &JsValue::from(alvo.clone())))`.
- No início de `main`: `__wk_dom_reserve(max(regiões, binds))` (ids fixos da página; instâncias de componentes alocam depois, §11).

### 8.9 Saída final
Página: `use crate::__wk_rt::*; <hoisted> <statics> <wrappers> #[wasm_bindgen] pub fn __wk_main_X() { reserve; <body com registros>; flush }`.
Componente: ver §11. Tudo passa por `prettyplease::unparse`.

## 9. Runtime em tempo de execução **[reatividade]**

[src/runtime/wk_rt.rs](../src/runtime/wk_rt.rs) é um arquivo Rust **normal** (compilado junto com o código do usuário via `include_str!`, copiado como `src/__wk_rt.rs`). Estado global em `thread_local!` (wasm é single-thread), linhas 27-39.

| Peça | Função | Linhas |
|---|---|---|
| `__WkShared<T>` | `{ id, Rc<UnsafeCell<T>> }`. `id` é único (`NEXT_ID`) e é a "identidade reativa". `get`/`get_ref` leem, `get_mut` empurra `id` em `DIRTY` e devolve `&mut`. `Clone` copia o `Rc` e **mantém o id**. `UnsafeCell` evita `RefCell` (que daria *panic* com leituras aninhadas durante render). | 44-98 |
| `BINDINGS` | `Vec<Binding{owner, deps, render}>` | 21, 35, 100 |
| `__wk_bind(deps, f)` | roda `f` já, **descarta o que ficou sujo durante essa execução** (`truncate`) e registra. | 106 |
| `__wk_bind_lazy(deps, f)` | só registra (componentes: o HTML inicial já vem de `html()`). | 114 |
| `__wk_set_derived` | escreve se `!=`, e põe o id em `DERIVED` (sujo "para a próxima passada") | 119 |
| `__wk_flush` | laço de até 100 passadas: pega `DIRTY` (take) → todas as bindings cujo `deps` intersecta → roda cada uma (com `OWNER` ajustado e pulando donos mortos) → `DIRTY := DERIVED`. Zerado = fim. Estouro = `panic!("ciclo...")`. | 130 |
| `__wk_escape` | escape HTML de `& < > " '` | 160 |
| `__wk_dom_reserve/alloc` | ids de âncoras DOM (páginas: fixos; instâncias: dinâmicos) | 177-188 |
| `__WkFromJs` / `__wk_arg` | converte `JsValue` → `String`/número/`bool`/`Option` para argumentos de handlers de componente e valores de `bind:` | 197-250 |
| `__wk_flushing` | adaptador de `Future` que chama `__wk_flush` depois de cada `poll` | final do arquivo |

Por que a passada de `DERIVED`: renderizar só lê, mas `x.método()` conta como escrita (`get_mut`) e sujaria tudo → por isso `__wk_bind` e o flush **só** propagam como sujo o que um `derived!` mudou de fato, e os `DIRTY` gerados por renderização são descartados.

### Ciclo de vida de uma mudança
```
clique → JS chama wrapper wasm (ex. happy())
  → closure roda: `clicks += 1`  →  (*clicks.get_mut()) += 1  →  DIRTY=[id(clicks)]
  → wrapper: __wk_flush()
       passada 1: bindings com dep em clicks: [derived doubled, região do if, effect...]
         doubled recalcula → __wk_set_derived → DERIVED=[id(doubled)]
         região roda → __wk_set(ID, html)  →  JS substitui o DOM
       passada 2: DIRTY=[id(doubled)] → regiões/effects que dependem dele
       passada 3: vazio → fim
```

## 10. `bind:` (propriedade ↔ variável) **[reatividade]**

Duas direções, geradas em pontos diferentes:

- **Variável → DOM**: bloco de região especial (§8.8) que chama `__wk_set_prop(id, prop, valor)`; o JS faz `document.querySelector('[data-wk-b<id>]')[prop] = valor` só se mudou ([loader.rs:60-100](../src/loader.rs), `set_prop`).
- **DOM → variável**: o `Lexer::bind` ([template.rs:473](../src/template.rs)) registra `(id, prop, event, alvo)`. `transform_impl` ([script.rs:130](../src/script.rs)) cria `fn __wk_bind_<pagina>_<id>(v: JsValue)` que faz `if let Some(x) = __WkFromJs::from_js(&v) { alvo = x }` — passa pelo `Rewriter`, então vira
  `get_mut` e entra no fluxo normal (+ flush, pois é uma `exported`). O nome vem de `bind_fn_name` ([script.rs:42](../src/script.rs)).
  O JS é montado em [loader.rs:8-40 `render_page`](../src/loader.rs): para cada bind, `el.addEventListener(event, () => wasm[fn](el[prop]))`, depois que o wasm iniciou.
- `bind:` serve para **qualquer propriedade** do elemento (`value`, `checked`, `valueAsNumber`, `textContent`, ...); o evento vem de `default_event` ou `bind:prop|evento=`.
- Como não existe laço infinito: `set_prop` só escreve se o valor mudou e o evento `input` não dispara ao escrever por script.
- **Em componentes** o JS não conhece ids fixos; o HTML da instância carrega `data-wk-b<N>="instância|N|prop|evento"` e `hook(root)` ([loader.rs:97-115](../src/loader.rs)) liga os listeners ao inserir o DOM, chamando `__wk_call(inst, "__wk_bind_N", [valor])`. O valor inicial é aplicado por `__wk_init(inst, "__wk_init_N")` depois da inserção ([script.rs:611 `component_bind`](../src/script.rs)).
- **`bind:` entre pai e filho**: `<Filho bind:x={v} />` → o template emite `__wk_bind!(v)` como valor da prop; o `Rewriter` troca por `v.clone()` (o próprio `__WkShared`, mesmo `id`), e marca `v` como capturada mas **não** como dependência (`bound` vs `used`, para o pai não re-renderizar o filho e perder foco). No filho, `let mut x: T = bind!();` vira campo `Props { x: __WkShared<T> }` e `let x: __WkShared<T> = __props.x;` — pai e filho compartilham a mesma célula e o mesmo id ([script.rs:143-200, share_declaration:778](../src/script.rs)).

## 11. Componentes **[componentes]**

Um `Nome.wk` vira o módulo `Nome` com `pub struct Props { .. }` e `pub fn __wk_create(props: Props) -> __WkInstance<Props>` ([script.rs:455-500](../src/script.rs)).
**Estado por instância**: tudo que na página seria variável do `main` aqui é local de `__wk_create`, então cada chamada cria o seu próprio estado.

Dentro de `__wk_create`:
- `let __inst = __wk_owner();` (id desta instância) e `let __dom = __wk_dom_alloc(n);` (ids de âncoras exclusivos).
- `prop!()`: `let x: T = prop!();` → campo `x: T` em `Props` e `__WkShared::new(__props.x)`; imutável. O pai atualiza com `__wk_set_derived` no `update` ([script.rs:480-490](../src/script.rs)) (exige `PartialEq`).
- Handlers: `handler_registration` ([script.rs:535](../src/script.rs)) → `__wk_register(__inst, "nome", Rc<closure>)` (em vez de `thread_local`). No HTML, `onclick="inc()"` é reescrito por `redirect_handlers` ([template.rs:236](../src/template.rs)) para `__wk.c(<id da instância>).inc()`; o JS (`c(instance)`, um `Proxy`) chama o export `__wk_call(inst, "inc", args)` ([wk_rt.rs:353](../src/runtime/wk_rt.rs)), que acha o handler em `HANDLERS` e dá flush.
- Regiões: `component_region` ([script.rs:585](../src/script.rs)) — `let __rN: Rc<dyn Fn()->String>` + `__wk_bind_lazy(deps, || __wk_set(__dom+N, &__rN()))`.
- Retorna `__WkInstance { update, html }`; `html()` monta o HTML da instância (`component_html`, [script.rs:635](../src/script.rs)) com âncoras `<!--wk:__dom+N-->` e atributos `data-wk-b`.

### Quem é dono do quê ([wk_rt.rs:252-345](../src/runtime/wk_rt.rs))
- `OWNER`: id da instância "em execução" (0 = página). `register` grava o dono em cada `Binding`; `__wk_register` em cada handler.
- Uso no template: `__wk_use(pool, site, key, props, create)` ([wk_rt.rs:309](../src/runtime/wk_rt.rs)). `pool` é um `__WkPool` **por região** (criado em `region_binding`/`component_region`). Identidade da instância = `(site, key)`:
  `site` = índice textual da tag no template; `key` = posição ou a expressão de `{#each .. by chave}` (campo `Node::Each.key`, [template.rs:104](../src/template.rs)). Existente → `update(props)`; nova → `create` com `OWNER` = id novo e `INSTANCES[id] = pai`.
- `__wk_begin(pool)` limpa `used`; `__wk_end(pool)` destrói quem não foi usado: `destroy(id)` remove bindings e handlers com esse dono e **recursivamente** os filhos (`INSTANCES`). `is_alive` no flush evita rodar binding de dono removido na mesma rodada.
- Consequência assumida: quando o pai re-renderiza, o DOM do filho é recriado (o estado não).

## 12. Carregamento da página (HTML + JS) **[núcleo]**

[loader.rs:8 `render_page`](../src/loader.rs) produz um documento completo: o `body` do template + um único `<script type="module">`:

1. `REGION_RUNTIME` ([loader.rs:60](../src/loader.rs)): define `window.__wk` **antes** de importar o wasm (o wasm importa `__wk.set/set_prop` via `#[wasm_bindgen(js_namespace = __wk)]`, [wk_rt.rs:14](../src/runtime/wk_rt.rs)).
   - `find(id)`: varre comentários com `TreeWalker`, cacheia nós `<!--wk:ID-->`/`<!--/wk:ID-->`; reescaneia se o cache estiver desconectado do DOM.
   - `set(id, html)`: se `html` é igual ao último (`WeakMap last`), não faz nada; senão `Range` entre os dois comentários → `deleteContents()` → `createContextualFragment(html)` → `hook(fragment)` (liga `bind:` de componentes) → `insertNode` → `__wk_init` dos binds novos.
2. `import init, * as wasm from "<prefix><glue>.js"` e `await init({module_or_path: new URL("<prefix><glue>_bg.wasm", import.meta.url)})`. O `prefix` é `../` repetido conforme a profundidade da página ([loader.rs:16](../src/loader.rs)) — todas as páginas compartilham o mesmo `.wasm` na raiz de `dist/`.
3. Cada export que não começa com `__wk_` vai para `window[k]` → permite `onclick="happy()"` inline (é por isso que `handlers` é coletado no parser).
4. `window.__wk.wasm = wasm` (usado por `c(instance)` e `__wk_init`).
5. `wasm.__wk_main_<página>()` — executa o script da página: cria estado, **renderiza todas as regiões** (cada `__wk_bind` roda uma vez → `__wk_set(ID, html)` preenche as âncoras vazias) e registra tudo.
6. Binds de página: `addEventListener` por bind (§10).

Limitação conhecida: antes do passo 5 as regiões estão vazias (flash de conteúdo).

## 13. Mapa de dependências entre módulos

```
main ─▶ scaffold, serve
main ─▶ manifest, transpile ─▶ builder (compile_wasm, package)
transpile ─▶ parser ─▶ (lol_html)
transpile ─▶ template ─▶ (syn, quote)
transpile ─▶ script   ─▶ template (Region, Bind, Part, text_tokens), rust (WASM_BINDGEN_ATTR)
transpile ─▶ loader   ─▶ script::bind_fn_name, template::Bind
transpile ─▶ rust     (arquivos .wk.rs)
transpile ─▶ runtime/wk_rt.rs (include_str!, copiado para o crate gerado)
```

## 14. Resumo do que cada decisão compra (para não esquecer ao reescrever)

| Decisão | Motivo |
|---|---|
| `__WkShared` com `id` estável nos clones | dependência por id, não por referência; closures podem ter cópias |
| `UnsafeCell` em vez de `RefCell` | leitura aninhada durante render/flush não pode dar panic |
| Estado só para variáveis usadas | custo zero para o resto do script |
| Registro após a última declaração usada | o `clone()` precisa que a variável já exista |
| Região = HTML inteiro entre comentários | evita diff de DOM; cache `last` no JS evita trabalho inútil |
| `get_mut` em receptor de método | simplicidade; falsos positivos são baratos |
| Passada `DERIVED` + limite 100 | derived em cadeia converge; ciclo vira erro claro |
| `thread_local` + wrapper por função (página) | o export wasm é função solta e precisa alcançar o estado do `main` |
| `HANDLERS[(inst, nome)]` + `__wk_call` (componente) | o export é um só para todas as instâncias |
| `include_str!` do runtime | o runtime é código Rust de verdade (editável/testável), sem `string` gigante no gerador |
| `cargo rustc --crate-type cdylib` + target dir fixo | não exige `[lib] crate-type` no `Cargo.toml` do usuário; cache entre builds |
| wasm-bindgen como lib | sem CLI externo, versão travada |

## 15. Limitações conhecidas (candidatas a melhorar na reescrita)

- `{#if a<b}` sem espaços confunde o lexer; interpolação em atributos só dentro de blocos; `bind:` só no nível superior.
- Handlers inline colidem com nomes de `document` (`window[k] = v`).
- Comentários do script se perdem (prettyplease).
- Componentes: sem slots, eventos, contexto, defaults de props; props faltando = erro de struct do rustc.
- Cada mudança regenera o HTML inteiro da região (foco/seleção de elementos não ligados por `bind:` se perdem).
- `Cargo.toml` do usuário precisa de dependências à mão (não há `cargo wk add`); o projeto gerado não tem alvo `lib`, então `cargo add` no projeto original falha.
