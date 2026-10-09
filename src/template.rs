//! Sintaxe de template do HTML de um `.wk`:
//!
//! - `{expr}`: valor de uma expressão Rust (escapado);
//! - `{#if cond}…{:else if cond}…{:else}…{/if}`;
//! - `{#each iter as item}…{/each}` e `{#each iter as item, i}…{/each}`;
//! - `{{` e `}}` produzem `{` e `}` literais;
//! - `bind:prop={var}` (ou `bind:prop|evento={var}`) em uma tag de nível superior: liga a
//!   propriedade `prop` do elemento à variável nos dois sentidos;
//! - `{#each iter as item by chave}`: identifica cada item pela chave (padrão: a posição).
//!
//! - `<Nome prop={expr} texto="..." />`: instância do componente `Nome` (o `.wk` de mesmo nome,
//!   trazido por um `use`). O estado de cada instância é dele; instâncias são reaproveitadas
//!   entre renderizações pela posição ou, em `{#each ... by chave}`, pela chave.
//!
//! Cada trecho dinâmico de nível superior vira uma *região*: um par de comentários
//! `<!--wk:N-->…<!--/wk:N-->` no HTML e um bloco Rust que devolve o HTML da região.
//! Dentro de uma região, tudo (inclusive atributos) pode usar a sintaxe.

use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Block, Expr, Pat, parse::Parser};

/// Marca, no HTML estático de um componente, o id da instância (só conhecido em tempo de execução).
pub const INSTANCE_MARK: &str = "\u{1}i\u{2}";
const BIND_OPEN: char = '\u{1}';
const BIND_CLOSE: char = '\u{2}';

/// Como o template será usado.
#[derive(Default)]
pub struct Options<'a> {
    /// Para componentes: as funções do script que atributos de evento podem chamar. Elas são
    /// redirecionadas para a instância dona do HTML.
    pub component_handlers: Option<&'a BTreeSet<String>>,
}

/// Pedaço do HTML de nível superior, na ordem em que aparece.
pub enum Part {
    Text(String),
    Region(usize),
    /// Atributo `data-wk-bN` de um `bind:`.
    Bind(usize),
}

/// Região dinâmica da página.
pub struct Region {
    pub id: usize,
    /// Bloco que usa `__out: String` e devolve o HTML da região.
    pub render: Block,
}

/// Ligação bidirecional entre uma propriedade de um elemento e uma expressão atribuível.
pub struct Bind {
    pub id: usize,
    pub prop: String,
    pub event: String,
    pub target: Expr,
}

pub struct Template {
    /// HTML estático, com as âncoras das regiões.
    pub html: String,
    pub parts: Vec<Part>,
    pub regions: Vec<Region>,
    pub binds: Vec<Bind>,
}

/// Evento que sinaliza mudança de cada propriedade conhecida.
fn default_event(prop: &str) -> Option<&'static str> {
    match prop {
        "value" | "valueAsNumber" | "valueAsDate" | "textContent" | "innerText" | "innerHTML" => {
            Some("input")
        }
        "checked" | "indeterminate" | "files" | "selectedIndex" => Some("change"),
        "open" => Some("toggle"),
        _ => None,
    }
}

#[derive(Debug)]
enum Tok {
    Raw(String),
    Interp(String),
    If(String),
    ElseIf(String),
    Else,
    EndIf,
    Each {
        iter: String,
        pat: String,
        index: Option<String>,
        key: Option<String>,
    },
    EndEach,
    Component {
        path: String,
        /// Nome do campo e o código Rust do seu valor.
        props: Vec<(String, String)>,
    },
}

enum Node {
    Raw(String),
    Interp(Expr),
    If {
        branches: Vec<(Expr, Vec<Node>)>,
        otherwise: Option<Vec<Node>>,
    },
    Each {
        iter: Expr,
        pat: Pat,
        index: Option<Pat>,
        key: Option<Expr>,
        body: Vec<Node>,
    },
    Component {
        path: syn::Path,
        props: Vec<(syn::Ident, Expr)>,
    },
}

pub fn compile(html: &str) -> Result<Template> {
    compile_with(html, &Options::default())
}

pub fn compile_with(html: &str, options: &Options) -> Result<Template> {
    let (mut tokens, binds) = tokenize(html)?;
    if let Some(names) = options.component_handlers {
        redirect_handlers(&mut tokens, names);
    }
    let mut iter = tokens.into_iter().peekable();
    let nodes = parse_nodes(&mut iter)?;
    if let Some(tok) = iter.next() {
        bail!("`{}` sem bloco correspondente", describe(&tok));
    }

    let mut parts = Vec::new();
    let mut regions = Vec::new();
    for node in nodes {
        match node {
            Node::Raw(s) => split_binds(&s, &mut parts),
            dynamic => {
                let id = regions.len();
                parts.push(Part::Region(id));
                let track = contains_component(&dynamic);
                let body = gen_nodes(std::slice::from_ref(&dynamic), track, &mut Sites::default());
                let render = if track {
                    quote!({
                        __wk_begin(&__pool);
                        let __key = ::std::string::String::new();
                        let mut __out = ::std::string::String::new();
                        #body
                        __wk_end(&__pool);
                        __out
                    })
                } else {
                    quote!({
                        let mut __out = ::std::string::String::new();
                        #body
                        __out
                    })
                };
                let render = syn::parse2(render)
                    .map_err(|e| anyhow!("erro interno ao gerar região: {e}"))?;
                regions.push(Region { id, render });
            }
        }
    }

    let mut html = String::new();
    for part in &parts {
        match part {
            Part::Text(s) => html.push_str(s),
            Part::Region(id) => html.push_str(&format!("<!--wk:{id}--><!--/wk:{id}-->")),
            Part::Bind(id) => html.push_str(&format!("data-wk-b{id}")),
        }
    }
    Ok(Template {
        html,
        parts,
        regions,
        binds,
    })
}

/// Separa o texto estático nos atributos `data-wk-bN` marcados pelo tokenizer.
fn split_binds(text: &str, parts: &mut Vec<Part>) {
    let mut rest = text;
    while let Some(start) = rest.find(BIND_OPEN) {
        let after = &rest[start + 1..];
        let Some(end) = after.find(BIND_CLOSE) else {
            break;
        };
        let Some(id) = after[..end].strip_prefix('b').and_then(|n| n.parse().ok()) else {
            // É a marca de instância, que fica no texto.
            let (head, tail) = rest.split_at(start + 1 + end + 1);
            push_text(parts, head);
            rest = tail;
            continue;
        };
        push_text(parts, &rest[..start]);
        parts.push(Part::Bind(id));
        rest = &after[end + 1..];
    }
    push_text(parts, rest);
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if text.is_empty() {
        return;
    }
    match parts.last_mut() {
        Some(Part::Text(last)) => last.push_str(text),
        _ => parts.push(Part::Text(text.to_string())),
    }
}

/// Código que acrescenta `text` a `__out`, trocando a marca de instância pelo id (`__inst`).
pub fn text_tokens(text: &str) -> TokenStream {
    let pieces = text.split(INSTANCE_MARK).collect::<Vec<_>>();
    let mut out = TokenStream::new();
    for (i, piece) in pieces.iter().enumerate() {
        if i > 0 {
            out.extend(quote!(__out.push_str(&::std::string::ToString::to_string(&__inst));));
        }
        if !piece.is_empty() {
            out.extend(quote!(__out.push_str(#piece);));
        }
    }
    out
}

/// Nos componentes, `onclick="inc()"` chama a instância: `onclick="__wk.c(<id>).inc()"`.
fn redirect_handlers(toks: &mut [Tok], names: &BTreeSet<String>) {
    let mut in_tag = false;
    // Aspas do valor de atributo em andamento e se é um atributo de evento.
    let mut attr: Option<(char, bool)> = None;
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    for tok in toks {
        let Tok::Raw(raw) = tok else {
            continue;
        };
        let chars: Vec<char> = raw.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match attr {
                Some((q, _)) if c == q => attr = None,
                Some((_, true))
                    if is_ident(c)
                        && !chars[..i].last().is_some_and(|p| is_ident(*p) || *p == '.') =>
                {
                    let end = i + chars[i..].iter().take_while(|c| is_ident(**c)).count();
                    let ident: String = chars[i..end].iter().collect();
                    let call = chars[end..].iter().find(|c| !c.is_whitespace()) == Some(&'(');
                    if call && names.contains(&ident) {
                        out.push_str(&format!("__wk.c({INSTANCE_MARK}).{ident}"));
                    } else {
                        out.push_str(&ident);
                    }
                    i = end;
                    continue;
                }
                Some(_) => {}
                None if in_tag && c == '>' => in_tag = false,
                None if in_tag && (c == '"' || c == '\'') => {
                    attr = Some((c, is_event_attr(&chars[..i])));
                }
                None if !in_tag
                    && c == '<'
                    && chars.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic()) =>
                {
                    in_tag = true;
                }
                None => {}
            }
            out.push(c);
            i += 1;
        }
        *raw = out;
    }
}

/// `before` termina em `on<evento>=`?
fn is_event_attr(before: &[char]) -> bool {
    let mut end = before.len();
    while end > 0 && before[end - 1].is_whitespace() {
        end -= 1;
    }
    if end == 0 || before[end - 1] != '=' {
        return false;
    }
    end -= 1;
    while end > 0 && before[end - 1].is_whitespace() {
        end -= 1;
    }
    let name: String = before[..end]
        .iter()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || **c == '-' || **c == '_')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    name.len() > 2 && name.to_ascii_lowercase().starts_with("on")
}

fn describe(tok: &Tok) -> &'static str {
    match tok {
        Tok::ElseIf(_) => "{:else if}",
        Tok::Else => "{:else}",
        Tok::EndIf => "{/if}",
        Tok::EndEach => "{/each}",
        _ => "bloco",
    }
}

// ---------------------------------------------------------------------------
// Tokenizer

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    raw: String,
    toks: Vec<Tok>,
    binds: Vec<Bind>,
    depth: usize,
}

fn tokenize(src: &str) -> Result<(Vec<Tok>, Vec<Bind>)> {
    let mut lx = Lexer {
        chars: src.chars().collect(),
        pos: 0,
        raw: String::new(),
        toks: vec![],
        binds: vec![],
        depth: 0,
    };
    let mut in_tag = false;
    let mut quote_char: Option<char> = None;
    let mut raw_text_element: Option<String> = None;

    while lx.pos < lx.chars.len() {
        let c = lx.chars[lx.pos];

        if !in_tag {
            if lx.starts_with("<!--") {
                lx.copy_until("-->");
                continue;
            }
            if c == '<' {
                let next = lx.chars.get(lx.pos + 1).copied().unwrap_or(' ');
                if next.is_ascii_uppercase() && raw_text_element.is_none() {
                    lx.component()?;
                    continue;
                }
                if next.is_ascii_alphabetic() {
                    let name: String = lx.chars[lx.pos + 1..]
                        .iter()
                        .take_while(|c| c.is_ascii_alphanumeric() || **c == '-')
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if name == "script" || name == "style" {
                        raw_text_element = Some(name);
                    }
                    in_tag = true;
                } else if next == '/'
                    && lx
                        .chars
                        .get(lx.pos + 2)
                        .is_some_and(|c| c.is_ascii_alphabetic())
                {
                    in_tag = true;
                }
            }
        } else if let Some(q) = quote_char {
            if c == q {
                quote_char = None;
            }
        } else if c == '"' || c == '\'' {
            quote_char = Some(c);
        } else if c == '>' {
            in_tag = false;
            lx.raw.push(c);
            lx.pos += 1;
            if let Some(name) = raw_text_element.take() {
                lx.copy_raw_text(&name);
            }
            continue;
        }

        if in_tag
            && quote_char.is_none()
            && lx.starts_with("bind:")
            && lx.raw.ends_with(|c: char| c.is_whitespace())
        {
            lx.bind(raw_text_element.is_some())?;
            continue;
        }

        if c == '{' {
            if lx.chars.get(lx.pos + 1) == Some(&'{') {
                lx.raw.push('{');
                lx.pos += 2;
                continue;
            }
            lx.directive(in_tag)?;
            continue;
        }
        if c == '}' && lx.chars.get(lx.pos + 1) == Some(&'}') {
            lx.raw.push('}');
            lx.pos += 2;
            continue;
        }
        lx.raw.push(c);
        lx.pos += 1;
    }
    if lx.depth > 0 {
        bail!("bloco `{{#if}}`/`{{#each}}` sem fechamento");
    }
    lx.flush();
    Ok((lx.toks, lx.binds))
}

impl Lexer {
    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(i, c)| self.chars.get(self.pos + i) == Some(&c))
    }

    fn flush(&mut self) {
        if !self.raw.is_empty() {
            self.toks.push(Tok::Raw(std::mem::take(&mut self.raw)));
        }
    }

    /// Copia o texto cru até `end` (inclusive).
    fn copy_until(&mut self, end: &str) {
        while self.pos < self.chars.len() {
            let at_end = self.starts_with(end);
            self.raw.push(self.chars[self.pos]);
            self.pos += 1;
            if at_end {
                self.raw.extend(end.chars().skip(1));
                self.pos += end.chars().count() - 1;
                return;
            }
        }
    }

    /// Copia o conteúdo de `<script>`/`<style>` até antes de `</name`.
    fn copy_raw_text(&mut self, name: &str) {
        let close: Vec<char> = format!("</{name}").chars().collect();
        while self.pos < self.chars.len() {
            let matches = close.iter().enumerate().all(|(i, c)| {
                self.chars
                    .get(self.pos + i)
                    .is_some_and(|x| x.to_ascii_lowercase() == *c)
            });
            if matches {
                return;
            }
            self.raw.push(self.chars[self.pos]);
            self.pos += 1;
        }
    }

    /// Lê `bind:prop[|evento]={expr}` (ou `="{expr}"`) e o troca por `data-wk-b<N>`.
    fn bind(&mut self, raw_text: bool) -> Result<()> {
        if self.depth > 0 {
            bail!("`bind:` ainda não é suportado dentro de `{{#if}}`/`{{#each}}`");
        }
        if raw_text {
            bail!("`bind:` não é suportado em <script>/<style>");
        }
        self.pos += "bind:".len();
        let word = |lx: &mut Lexer, stop: &[char]| {
            let start = lx.pos;
            while lx
                .chars
                .get(lx.pos)
                .is_some_and(|c| !stop.contains(c) && !c.is_whitespace() && *c != '>')
            {
                lx.pos += 1;
            }
            lx.chars[start..lx.pos].iter().collect::<String>()
        };
        let prop = word(self, &['=', '|']);
        let explicit = if self.chars.get(self.pos) == Some(&'|') {
            self.pos += 1;
            Some(word(self, &['=']))
        } else {
            None
        };
        if prop.is_empty() || !prop.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("nome de propriedade inválido em `bind:{prop}`");
        }
        let event = match explicit {
            Some(e) if !e.is_empty() => e,
            Some(_) => bail!("evento vazio em `bind:{prop}|`"),
            None => default_event(&prop)
                .ok_or_else(|| {
                    anyhow!("`bind:{prop}` precisa de um evento: bind:{prop}|evento={{var}}")
                })?
                .to_string(),
        };
        if self.chars.get(self.pos) != Some(&'=') {
            bail!("`bind:{prop}` precisa de um valor: bind:{prop}={{var}}");
        }
        self.pos += 1;
        let quoted = matches!(self.chars.get(self.pos), Some('"' | '\''));
        if quoted {
            self.pos += 1;
        }
        if self.chars.get(self.pos) != Some(&'{') {
            bail!("o valor de `bind:{prop}` deve ser `{{expressão}}`");
        }
        let inner = self.read_braced()?;
        if quoted {
            self.pos += 1;
        }
        let target = parse_expr(inner.trim())?;
        if !is_assignable(&target) {
            bail!(
                "`bind:{prop}={{{}}}` precisa de uma variável, campo ou índice",
                inner.trim()
            );
        }
        let id = self.binds.len();
        self.binds.push(Bind {
            id,
            prop,
            event,
            target,
        });
        self.raw.push(BIND_OPEN);
        self.raw.push_str(&format!("b{id}"));
        self.raw.push(BIND_CLOSE);
        Ok(())
    }

    /// Lê `<Nome prop={expr} texto="..." {atalho} />` na posição atual.
    fn component(&mut self) -> Result<()> {
        self.pos += 1;
        let start = self.pos;
        while self
            .chars
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        {
            self.pos += 1;
        }
        let path: String = self.chars[start..self.pos].iter().collect();
        let mut props = Vec::new();
        loop {
            while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
                self.pos += 1;
            }
            match self.chars.get(self.pos) {
                None => bail!("tag `<{path}` sem fechamento"),
                Some('/') if self.chars.get(self.pos + 1) == Some(&'>') => {
                    self.pos += 2;
                    break;
                }
                Some('>') => {
                    self.pos += 1;
                    let close = format!("</{path}>");
                    while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
                        self.pos += 1;
                    }
                    if !self.starts_with(&close) {
                        bail!("`<{path}>` não aceita conteúdo (slots ainda não são suportados)");
                    }
                    self.pos += close.chars().count();
                    break;
                }
                Some('{') => {
                    let inner = self.read_braced()?;
                    let name = inner.trim().to_string();
                    props.push((name.clone(), clone_of(&name)));
                }
                Some(_) => {
                    let name_start = self.pos;
                    while self
                        .chars
                        .get(self.pos)
                        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                    {
                        self.pos += 1;
                    }
                    let name: String = self.chars[name_start..self.pos].iter().collect();
                    if name.is_empty() || self.chars.get(self.pos) != Some(&'=') {
                        bail!("em `<{path}`: esperado `nome={{expr}}` ou `nome=\"texto\"`");
                    }
                    self.pos += 1;
                    let value = match self.chars.get(self.pos) {
                        Some('{') => clone_of(self.read_braced()?.trim()),
                        Some(q @ ('"' | '\'')) => {
                            let q = *q;
                            let from = self.pos + 1;
                            let len = self.chars[from..]
                                .iter()
                                .position(|c| *c == q)
                                .ok_or_else(|| anyhow!("texto sem fechamento em `{name}=`"))?;
                            self.pos = from + len + 1;
                            let text: String = self.chars[from..from + len].iter().collect();
                            format!("::std::convert::Into::into({text:?})")
                        }
                        _ => bail!("o valor de `{name}` deve ser `{{expr}}` ou `\"texto\"`"),
                    };
                    props.push((name, value));
                }
            }
        }
        self.flush();
        self.toks.push(Tok::Component { path, props });
        Ok(())
    }

    fn directive(&mut self, in_tag: bool) -> Result<()> {
        let inner = self.read_braced()?;
        let inner = inner.trim();
        let tok = if let Some(rest) = inner.strip_prefix("#if") {
            Tok::If(non_empty(rest, "{#if}")?)
        } else if let Some(rest) = inner.strip_prefix("#each") {
            parse_each(rest)?
        } else if let Some(rest) = inner.strip_prefix(":else if") {
            Tok::ElseIf(non_empty(rest, "{:else if}")?)
        } else if inner == ":else" {
            Tok::Else
        } else if inner == "/if" {
            Tok::EndIf
        } else if inner == "/each" {
            Tok::EndEach
        } else if inner.starts_with(['#', ':', '/']) {
            bail!("diretiva desconhecida: {{{inner}}}");
        } else {
            Tok::Interp(non_empty(inner, "{}")?)
        };

        if in_tag && self.depth == 0 {
            bail!(
                "`{{{inner}}}` dentro de uma tag só é suportado dentro de um bloco `{{#if}}`/`{{#each}}`; \
                 use `{{{{` e `}}}}` para chaves literais"
            );
        }
        match tok {
            Tok::If(_) | Tok::Each { .. } => self.depth += 1,
            Tok::EndIf | Tok::EndEach => {
                self.depth = self
                    .depth
                    .checked_sub(1)
                    .ok_or_else(|| anyhow!("`{{{inner}}}` sem bloco aberto"))?;
            }
            _ => {}
        }
        self.flush();
        self.toks.push(tok);
        Ok(())
    }

    /// Lê `{ ... }` na posição atual e devolve o conteúdo, respeitando chaves aninhadas e strings.
    fn read_braced(&mut self) -> Result<String> {
        let start = self.pos;
        let mut depth = 0usize;
        let mut i = self.pos;
        while i < self.chars.len() {
            match self.chars[i] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos = i + 1;
                        return Ok(self.chars[start + 1..i].iter().collect());
                    }
                }
                '"' => {
                    i += 1;
                    while i < self.chars.len() && self.chars[i] != '"' {
                        if self.chars[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                '\'' if self.chars.get(i + 2) == Some(&'\'') => i += 2,
                '\'' if self.chars.get(i + 1) == Some(&'\\')
                    && self.chars.get(i + 3) == Some(&'\'') =>
                {
                    i += 3
                }
                _ => {}
            }
            i += 1;
        }
        let snippet: String = self.chars[start..].iter().take(30).collect();
        bail!("chave sem fechamento em `{snippet}`")
    }
}

/// As props são passadas por valor: o estado do pai continua dele.
fn clone_of(expr: &str) -> String {
    format!("::std::clone::Clone::clone(&({expr}))")
}

fn is_assignable(e: &Expr) -> bool {
    match e {
        Expr::Path(_) => true,
        Expr::Field(f) => is_assignable(&f.base),
        Expr::Index(i) => is_assignable(&i.expr),
        Expr::Paren(p) => is_assignable(&p.expr),
        _ => false,
    }
}

fn non_empty(s: &str, what: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() {
        bail!("expressão vazia em {what}");
    }
    Ok(s.to_string())
}

/// `iter as pat` ou `iter as pat, index`.
fn parse_each(rest: &str) -> Result<Tok> {
    let (iter, binding) = split_top_level(rest, " as ")
        .ok_or_else(|| anyhow!("`{{#each}}` precisa de `as`: {{#each lista as item}}"))?;
    let (binding, key) = match split_top_level(binding, " by ") {
        Some((b, k)) => (b, Some(non_empty(k, "`by` de {#each}")?)),
        None => (binding, None),
    };
    let (pat, index) = match split_top_level_last(binding, ',') {
        Some((p, i)) => (p, Some(i.trim().to_string())),
        None => (binding, None),
    };
    Ok(Tok::Each {
        iter: non_empty(iter, "{#each}")?,
        pat: non_empty(pat, "{#each}")?,
        index,
        key,
    })
}

/// Primeira ocorrência de `sep` fora de parênteses, colchetes, chaves e strings.
fn split_top_level<'a>(s: &'a str, sep: &str) -> Option<(&'a str, &'a str)> {
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' | '[' | '{' if !in_str => depth += 1,
            ')' | ']' | '}' if !in_str => depth -= 1,
            _ => {}
        }
        if depth == 0 && !in_str && s[i..].starts_with(sep) {
            return Some((&s[..i], &s[i + sep.len()..]));
        }
    }
    None
}

/// Última ocorrência de `sep` no nível zero.
fn split_top_level_last(s: &str, sep: char) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut last = None;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if c == sep && depth == 0 => last = Some(i),
            _ => {}
        }
    }
    last.map(|i| (&s[..i], &s[i + sep.len_utf8()..]))
}

// ---------------------------------------------------------------------------
// Árvore

type Toks = std::iter::Peekable<std::vec::IntoIter<Tok>>;

fn parse_expr(src: &str) -> Result<Expr> {
    syn::parse_str(src).with_context(|| format!("expressão inválida `{src}`"))
}

fn parse_pat(src: &str) -> Result<Pat> {
    Pat::parse_single
        .parse_str(src)
        .map_err(|e| anyhow!("padrão inválido `{src}`: {e}"))
}

/// Lê nós até um token de fechamento (`{:else…}`, `{/if}`, `{/each}`), sem consumi-lo.
fn parse_nodes(toks: &mut Toks) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    while let Some(tok) = toks.peek() {
        if matches!(tok, Tok::ElseIf(_) | Tok::Else | Tok::EndIf | Tok::EndEach) {
            break;
        }
        match toks.next().unwrap() {
            Tok::Raw(s) => nodes.push(Node::Raw(s)),
            Tok::Interp(e) => nodes.push(Node::Interp(parse_expr(&e)?)),
            Tok::If(cond) => nodes.push(parse_if(toks, cond)?),
            Tok::Component { path, props } => {
                let props = props
                    .iter()
                    .map(|(name, value)| {
                        let ident = syn::parse_str::<syn::Ident>(name)
                            .map_err(|_| anyhow!("nome de prop inválido `{name}`"))?;
                        Ok((ident, parse_expr(value)?))
                    })
                    .collect::<Result<_>>()?;
                let path = syn::parse_str::<syn::Path>(&path)
                    .map_err(|_| anyhow!("nome de componente inválido `{path}`"))?;
                nodes.push(Node::Component { path, props });
            }
            Tok::Each {
                iter,
                pat,
                index,
                key,
            } => {
                let body = parse_nodes(toks)?;
                match toks.next() {
                    Some(Tok::EndEach) => {}
                    _ => bail!("`{{#each}}` sem `{{/each}}`"),
                }
                nodes.push(Node::Each {
                    iter: parse_expr(&iter)?,
                    pat: parse_pat(&pat)?,
                    index: index.as_deref().map(parse_pat).transpose()?,
                    key: key.as_deref().map(parse_expr).transpose()?,
                    body,
                });
            }
            _ => unreachable!(),
        }
    }
    Ok(nodes)
}

fn parse_if(toks: &mut Toks, cond: String) -> Result<Node> {
    let mut branches = vec![(parse_expr(&cond)?, parse_nodes(toks)?)];
    let mut otherwise = None;
    loop {
        match toks.next() {
            Some(Tok::ElseIf(c)) => branches.push((parse_expr(&c)?, parse_nodes(toks)?)),
            Some(Tok::Else) => {
                otherwise = Some(parse_nodes(toks)?);
                match toks.next() {
                    Some(Tok::EndIf) => break,
                    _ => bail!("`{{:else}}` precisa ser seguido de `{{/if}}`"),
                }
            }
            Some(Tok::EndIf) => break,
            _ => bail!("`{{#if}}` sem `{{/if}}`"),
        }
    }
    Ok(Node::If {
        branches,
        otherwise,
    })
}

// ---------------------------------------------------------------------------
// Geração de código

/// Contadores usados para dar nomes únicos dentro de uma região.
#[derive(Default)]
struct Sites {
    components: usize,
    loops: usize,
}

fn contains_component(node: &Node) -> bool {
    match node {
        Node::Component { .. } => true,
        Node::Raw(_) | Node::Interp(_) => false,
        Node::If {
            branches,
            otherwise,
        } => {
            branches.iter().flat_map(|(_, b)| b).any(contains_component)
                || otherwise.iter().flatten().any(contains_component)
        }
        Node::Each { body, .. } => body.iter().any(contains_component),
    }
}

/// `track`: a região contém componentes, então cada instância precisa de uma chave
/// (`__key`) que a identifique entre renderizações.
fn gen_nodes(nodes: &[Node], track: bool, sites: &mut Sites) -> TokenStream {
    let mut out = TokenStream::new();
    for node in nodes {
        out.extend(match node {
            Node::Raw(s) => text_tokens(s),
            Node::Interp(e) => quote!(
                __out.push_str(&__wk_escape(&::std::string::ToString::to_string(&(#e))));
            ),
            Node::Component { path, props } => {
                let site = sites.components;
                sites.components += 1;
                let names = props.iter().map(|(n, _)| n);
                let values = props.iter().map(|(_, v)| v);
                quote!(
                    __out.push_str(&__wk_use(
                        &__pool,
                        #site,
                        &__key,
                        #path::Props { #(#names: #values),* },
                        #path::__wk_create,
                    ));
                )
            }
            Node::If {
                branches,
                otherwise,
            } => {
                let mut ts = TokenStream::new();
                for (i, (cond, body)) in branches.iter().enumerate() {
                    let body = gen_nodes(body, track, sites);
                    ts.extend(if i == 0 {
                        quote!(if #cond { #body })
                    } else {
                        quote!(else if #cond { #body })
                    });
                }
                if let Some(body) = otherwise {
                    let body = gen_nodes(body, track, sites);
                    ts.extend(quote!(else { #body }));
                }
                ts
            }
            Node::Each {
                iter,
                pat,
                index,
                key,
                body,
            } => {
                let body = gen_nodes(body, track, sites);
                // Chamadas e ranges são consumidos por valor; o resto (variáveis, campos…) é emprestado.
                let by_value = matches!(
                    iter,
                    Expr::Range(_)
                        | Expr::MethodCall(_)
                        | Expr::Call(_)
                        | Expr::Macro(_)
                        | Expr::Array(_)
                );
                let source = if by_value { quote!(#iter) } else { quote!(&#iter) };
                let counter = format_ident!("__n{}", sites.loops);
                sites.loops += 1;
                let (prelude, inner) = if track {
                    let key = key.as_ref().map_or_else(|| quote!(#counter), |k| quote!(#k));
                    (
                        quote!(let mut #counter = 0usize;),
                        (
                            quote!(let __key = ::std::format!("{}/{}", __key, #key);),
                            quote!(#counter += 1;),
                        ),
                    )
                } else {
                    (quote!(), (quote!(), quote!()))
                };
                let (enter, leave) = inner;
                match index {
                    Some(index) => quote!(
                        #prelude
                        for (#index, #pat) in ::std::iter::IntoIterator::into_iter(#source).enumerate() {
                            #enter
                            #body
                            #leave
                        }
                    ),
                    None => quote!(
                        #prelude
                        for #pat in #source {
                            #enter
                            #body
                            #leave
                        }
                    ),
                }
            }
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_html_is_untouched() {
        let t = compile(
            "<p>oi</p><script>if (a) { b(); }</script><style>p { color: red }</style><!-- {x} -->",
        )
        .unwrap();
        assert_eq!(
            t.html,
            "<p>oi</p><script>if (a) { b(); }</script><style>p { color: red }</style><!-- {x} -->"
        );
        assert!(t.regions.is_empty());
    }

    #[test]
    fn creates_regions_with_anchors() {
        let t = compile(
            "<p>{a}</p>{#if a > 1}x{:else}y{/if}<ul>{#each v as it, i}<li>{i}{it}</li>{/each}</ul>",
        )
        .unwrap();
        assert_eq!(t.regions.len(), 3);
        assert_eq!(
            t.html,
            "<p><!--wk:0--><!--/wk:0--></p><!--wk:1--><!--/wk:1--><ul><!--wk:2--><!--/wk:2--></ul>"
        );
    }

    #[test]
    fn escaped_braces_are_literal() {
        let t = compile("<p>{{a}}</p>").unwrap();
        assert_eq!(t.html, "<p>{a}</p>");
    }

    #[test]
    fn allows_nested_braces_and_strings_in_expressions() {
        let t = compile(r#"{format!("{}}", { 1 })}"#).unwrap();
        assert_eq!(t.regions.len(), 1);
    }

    #[test]
    fn dynamic_attributes_only_inside_blocks() {
        assert!(compile("<a href=\"{x}\">a</a>").is_err());
        assert!(compile("{#if c}<a href=\"{x}\">a</a>{/if}").is_ok());
    }

    #[test]
    fn reports_unbalanced_blocks() {
        assert!(compile("{#if a}x").is_err());
        assert!(compile("x{/if}").is_err());
        assert!(compile("{#each a}x{/each}").is_err());
        assert!(compile("{:else}").is_err());
    }
}

#[cfg(test)]
mod bind_tests {
    use super::*;

    #[test]
    fn extracts_binds() {
        let t = compile(r#"<input bind:value={name}> <input bind:scrollTop|scroll="{ s.y }">"#)
            .unwrap();
        assert_eq!(t.binds.len(), 2);
        assert_eq!(
            (t.binds[0].prop.as_str(), t.binds[0].event.as_str()),
            ("value", "input")
        );
        assert_eq!(t.binds[1].event, "scroll");
        assert_eq!(t.html, "<input data-wk-b0> <input data-wk-b1>");
    }

    #[test]
    fn rejects_bad_binds() {
        assert!(compile("<input bind:foo={x}>").is_err());
        assert!(compile("<input bind:value={f(x)}>").is_err());
        assert!(compile("{#if c}<input bind:value={x}>{/if}").is_err());
    }
}
