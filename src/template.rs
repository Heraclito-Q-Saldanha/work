//! Sintaxe de template do HTML de um `.wk`:
//!
//! - `{expr}`: valor de uma expressão Rust (escapado);
//! - `{#if cond}…{:else if cond}…{:else}…{/if}`;
//! - `{#each iter as item}…{/each}` e `{#each iter as item, i}…{/each}`;
//! - `{{` e `}}` produzem `{` e `}` literais;
//! - `bind:prop={var}` (ou `bind:prop|evento={var}`) em uma tag de nível superior: liga a
//!   propriedade `prop` do elemento à variável nos dois sentidos.
//!
//! Cada trecho dinâmico de nível superior vira uma *região*: um par de comentários
//! `<!--wk:N-->…<!--/wk:N-->` no HTML e um bloco Rust que devolve o HTML da região.
//! Dentro de uma região, tudo (inclusive atributos) pode usar a sintaxe.

use anyhow::{Context, Result, anyhow, bail};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Block, Expr, Pat, parse::Parser};

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
    },
    EndEach,
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
        body: Vec<Node>,
    },
}

pub fn compile(html: &str) -> Result<Template> {
    let (tokens, binds) = tokenize(html)?;
    let mut iter = tokens.into_iter().peekable();
    let nodes = parse_nodes(&mut iter)?;
    if let Some(tok) = iter.next() {
        bail!("`{}` sem bloco correspondente", describe(&tok));
    }

    let mut out = String::new();
    let mut regions = Vec::new();
    for node in nodes {
        match node {
            Node::Raw(s) => out.push_str(&s),
            dynamic => {
                let id = regions.len();
                out.push_str(&format!("<!--wk:{id}--><!--/wk:{id}-->"));
                let body = gen_nodes(std::slice::from_ref(&dynamic));
                let render = syn::parse2(quote!({
                    let mut __out = ::std::string::String::new();
                    #body
                    __out
                }))
                .map_err(|e| anyhow!("erro interno ao gerar região: {e}"))?;
                regions.push(Region { id, render });
            }
        }
    }
    Ok(Template {
        html: out,
        regions,
        binds,
    })
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
        if prop.is_empty()
            || !prop.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
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
            bail!("`bind:{prop}={{{}}}` precisa de uma variável, campo ou índice", inner.trim());
        }
        let id = self.binds.len();
        self.binds.push(Bind {
            id,
            prop,
            event,
            target,
        });
        self.raw.push_str(&format!("data-wk-b{id}"));
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
    let (pat, index) = match split_top_level_last(binding, ',') {
        Some((p, i)) => (p, Some(i.trim().to_string())),
        None => (binding, None),
    };
    Ok(Tok::Each {
        iter: non_empty(iter, "{#each}")?,
        pat: non_empty(pat, "{#each}")?,
        index,
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
            Tok::Each { iter, pat, index } => {
                let body = parse_nodes(toks)?;
                match toks.next() {
                    Some(Tok::EndEach) => {}
                    _ => bail!("`{{#each}}` sem `{{/each}}`"),
                }
                nodes.push(Node::Each {
                    iter: parse_expr(&iter)?,
                    pat: parse_pat(&pat)?,
                    index: index.as_deref().map(parse_pat).transpose()?,
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

fn gen_nodes(nodes: &[Node]) -> TokenStream {
    let parts = nodes.iter().map(|node| match node {
        Node::Raw(s) => quote!(__out.push_str(#s);),
        Node::Interp(e) => quote!(
            __out.push_str(&__wk_escape(&::std::string::ToString::to_string(&(#e))));
        ),
        Node::If { branches, otherwise } => {
            let mut ts = TokenStream::new();
            for (i, (cond, body)) in branches.iter().enumerate() {
                let body = gen_nodes(body);
                ts.extend(if i == 0 {
                    quote!(if #cond { #body })
                } else {
                    quote!(else if #cond { #body })
                });
            }
            if let Some(body) = otherwise {
                let body = gen_nodes(body);
                ts.extend(quote!(else { #body }));
            }
            ts
        }
        Node::Each { iter, pat, index, body } => {
            let body = gen_nodes(body);
            // Chamadas e ranges são consumidos por valor; o resto (variáveis, campos…) é emprestado.
            let by_value = matches!(
                iter,
                Expr::Range(_) | Expr::MethodCall(_) | Expr::Call(_) | Expr::Macro(_) | Expr::Array(_)
            );
            let source = if by_value { quote!(#iter) } else { quote!(&#iter) };
            match index {
                Some(index) => quote!(
                    for (#index, #pat) in ::std::iter::IntoIterator::into_iter(#source).enumerate() { #body }
                ),
                None => quote!(for #pat in #source { #body }),
            }
        }
    });
    quote!(#(#parts)*)
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
        assert_eq!((t.binds[0].prop.as_str(), t.binds[0].event.as_str()), ("value", "input"));
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
