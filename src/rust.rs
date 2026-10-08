use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use syn::{Item, spanned::Spanned};

pub const WASM_BINDGEN_ATTR: &str = "#[::wasm_bindgen::prelude::wasm_bindgen]";

/// Anota com `#[wasm_bindgen]` as funções de nível superior cujo nome está em `names`.
pub fn export_functions(code: &str, names: &BTreeSet<String>) -> Result<String> {
    let file = syn::parse_file(code).map_err(|e| anyhow!("Rust inválido: {e}"))?;

    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(code.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |line: usize, column: usize| {
        let start = line_starts[line - 1];
        let in_line = code[start..]
            .char_indices()
            .nth(column)
            .map_or(code.len() - start, |(i, _)| i);
        start + in_line
    };

    let mut offsets: Vec<usize> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) if names.contains(&f.sig.ident.to_string()) => {
                let start = f.span().start();
                Some(offset(start.line, start.column))
            }
            _ => None,
        })
        .collect();
    offsets.sort_unstable_by(|a, b| b.cmp(a));

    let mut out = code.to_string();
    for at in offsets {
        out.insert_str(at, &format!("{WASM_BINDGEN_ATTR} "));
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
        let out = export_functions(code, &names).unwrap();
        assert_eq!(
            out,
            format!("fn other() {{}}\n\n{WASM_BINDGEN_ATTR} /// doc\npub fn add(a: i32) -> i32 {{ a }}\n")
        );
        syn::parse_file(&out).unwrap();
    }
}
