use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use proc_macro2::LineColumn;
use syn::{Item, spanned::Spanned};

pub const WASM_BINDGEN_ATTR: &str = "#[::wasm_bindgen::prelude::wasm_bindgen]";

/// Traduz o Rust de um `.wk`:
/// - anota com `#[wasm_bindgen]` as funções de nível superior cujo nome está em `names`;
/// - converte blocos `extern "js" { ... }` em `#[wasm_bindgen] extern "C" { ... }`.
pub fn transform(code: &str, names: &BTreeSet<String>) -> Result<String> {
    let file = syn::parse_file(code).map_err(|e| anyhow!("Rust inválido: {e}"))?;

    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(code.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |pos: LineColumn| {
        let start = line_starts[pos.line - 1];
        let in_line = code[start..]
            .char_indices()
            .nth(pos.column)
            .map_or(code.len() - start, |(i, _)| i);
        start + in_line
    };

    // (início, fim, texto): inserções têm início == fim.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for item in &file.items {
        match item {
            Item::Fn(f) if names.contains(&f.sig.ident.to_string()) => {
                let at = offset(f.span().start());
                edits.push((at, at, format!("{WASM_BINDGEN_ATTR} ")));
            }
            Item::ForeignMod(m) if m.abi.name.as_ref().is_some_and(|n| n.value() == "js") => {
                let at = offset(m.span().start());
                edits.push((at, at, format!("{WASM_BINDGEN_ATTR}\n")));
                if let Some(name) = &m.abi.name {
                    let lit = name.span();
                    edits.push((offset(lit.start()), offset(lit.end()), "\"C\"".to_string()));
                }
            }
            _ => {}
        }
    }
    edits.sort_by(|a, b| b.0.cmp(&a.0));

    let mut out = code.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, &text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotates_only_called_functions() {
        let names = BTreeSet::from(["add".to_string()]);
        let code = "fn other() {}\n\n/// doc\npub fn add(a: i32) -> i32 { a }\n";
        let out = transform(code, &names).unwrap();
        assert_eq!(
            out,
            format!("fn other() {{}}\n\n{WASM_BINDGEN_ATTR} /// doc\npub fn add(a: i32) -> i32 {{ a }}\n")
        );
        syn::parse_file(&out).unwrap();
    }

    #[test]
    fn converts_extern_js_blocks() {
        let code = "extern \"js\" {\n    fn alert(s: &str);\n}\nextern \"C\" {}\n";
        let out = transform(code, &BTreeSet::new()).unwrap();
        assert_eq!(
            out,
            format!("{WASM_BINDGEN_ATTR}\nextern \"C\" {{\n    fn alert(s: &str);\n}}\nextern \"C\" {{}}\n")
        );
    }
}
