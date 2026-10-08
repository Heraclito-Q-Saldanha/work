use std::{cell::RefCell, collections::BTreeSet};

use anyhow::{Context, Result};
use lol_html::{RewriteStrSettings, element, rewrite_str, text};

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedWk {
    pub html: String,
    pub rust: Option<String>,
    /// Funções chamadas em atributos de evento inline (`onclick="foo()"`).
    pub handlers: BTreeSet<String>,
}

pub fn parse(source: &str) -> Result<ParsedWk> {
    let scripts = RefCell::new(Vec::<String>::new());
    let handlers = RefCell::new(BTreeSet::new());
    let html = rewrite_str(
        source,
        RewriteStrSettings::new()
            .append_element_content_handler(element!("*", |el| {
                for attr in el.attributes() {
                    if attr.name().starts_with("on") {
                        handlers
                            .borrow_mut()
                            .extend(called_functions(&attr.value()));
                    }
                }
                Ok(())
            }))
            .append_element_content_handler(element!("script[lang='rs']", |el| {
                scripts.borrow_mut().push(String::new());
                el.remove();
                Ok(())
            }))
            .append_element_content_handler(text!("script[lang='rs']", |t| {
                if let Some(last) = scripts.borrow_mut().last_mut() {
                    last.push_str(t.as_str());
                }
                Ok(())
            })),
    )
    .context("falha ao parsear HTML")?;

    let scripts = scripts.into_inner();
    let rust = (!scripts.is_empty()).then(|| scripts.join("\n"));
    Ok(ParsedWk {
        html,
        rust,
        handlers: handlers.into_inner(),
    })
}

/// Identificadores usados como chamada de função (`nome(`) em um trecho de JS,
/// ignorando chamadas de método (`obj.nome(`).
fn called_functions(js: &str) -> Vec<String> {
    let chars: Vec<char> = js.chars().collect();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !is_ident(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && is_ident(chars[i]) {
            i += 1;
        }
        let ident: String = chars[start..i].iter().collect();
        let after = chars[i..].iter().find(|c| !c.is_whitespace());
        let before = chars[..start].iter().rev().find(|c| !c.is_whitespace());
        if after == Some(&'(')
            && before != Some(&'.')
            && !ident.starts_with(|c: char| c.is_ascii_digit())
        {
            found.push(ident);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_strips_rust_scripts() {
        let p = parse(r#"<p>a</p><script lang="rs">fn a(){}</script><script>js()</script><script lang="rs">fn b(){}</script>"#).unwrap();
        assert_eq!(p.html, "<p>a</p><script>js()</script>");
        assert_eq!(p.rust.as_deref(), Some("fn a(){}\nfn b(){}"));
    }

    #[test]
    fn no_rust_scripts() {
        let p = parse("<p>oi</p>").unwrap();
        assert_eq!(p.html, "<p>oi</p>");
        assert_eq!(p.rust, None);
    }

    #[test]
    fn collects_event_handler_calls() {
        let p = parse(r#"<button onclick="add(1, 2); console.log(x)" onmouseover="hi ()">x</button><a href="f(1)">y</a>"#).unwrap();
        assert_eq!(p.handlers.into_iter().collect::<Vec<_>>(), ["add", "hi"]);
    }
}
