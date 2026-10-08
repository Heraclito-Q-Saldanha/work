use std::cell::RefCell;

use anyhow::{Context, Result};
use lol_html::{RewriteStrSettings, element, rewrite_str, text};

#[derive(Debug, PartialEq, Eq)]
pub struct ParsedWk {
    pub html: String,
    pub rust: Option<String>,
}

pub fn parse(source: &str) -> Result<ParsedWk> {
    let scripts = RefCell::new(Vec::<String>::new());
    let html = rewrite_str(
        source,
        RewriteStrSettings::new()
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
    Ok(ParsedWk { html, rust })
}
