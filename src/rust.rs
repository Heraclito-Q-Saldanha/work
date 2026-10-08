use anyhow::{Result, anyhow};
use std::collections::BTreeSet;
use proc_macro2::LineColumn;
use syn::{Item, Stmt, spanned::Spanned};

pub const WASM_BINDGEN_ATTR: &str = "#[::wasm_bindgen::prelude::wasm_bindgen]";

/// Traduz um arquivo Rust inteiro (`.wk.rs`).
pub fn transform(code: &str, names: &BTreeSet<String>) -> Result<String> {
    let file = syn::parse_file(code).map_err(|e| anyhow!("Rust inválido: {e}"))?;
    let edits = collect_edits(&file.items, names, &Offsets::new(code));
    Ok(apply(code, edits))
}

/// Traduz o código de um `<script lang="rs">`, que passa a ser o corpo de uma função
/// exportada chamada `main_name`, executada quando a página carrega.
pub fn transform_script(code: &str, names: &BTreeSet<String>, main_name: &str) -> Result<String> {
    let wrapped = format!("pub fn {main_name}() {{\n{code}\n}}\n");
    let file = syn::parse_file(&wrapped).map_err(|e| anyhow!("Rust inválido: {e}"))?;
    let Some(Item::Fn(main)) = file.items.first() else {
        unreachable!("o wrapper é sempre uma função");
    };
    let nested: Vec<Item> = main
        .block
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Item(i) => Some(i.clone()),
            _ => None,
        })
        .collect();

    let mut edits = collect_edits(&nested, names, &Offsets::new(&wrapped));
    edits.push((0, 0, format!("{WASM_BINDGEN_ATTR}\n")));
    Ok(apply(&wrapped, edits))
}

/// (início, fim, texto): inserções têm início == fim.
type Edit = (usize, usize, String);

struct Offsets<'a> {
    code: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Offsets<'a> {
    fn new(code: &'a str) -> Self {
        let line_starts = std::iter::once(0)
            .chain(code.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        Self { code, line_starts }
    }

    fn of(&self, pos: LineColumn) -> usize {
        let start = self.line_starts[pos.line - 1];
        let in_line = self.code[start..]
            .char_indices()
            .nth(pos.column)
            .map_or(self.code.len() - start, |(i, _)| i);
        start + in_line
    }
}

/// - anota com `#[wasm_bindgen]` as funções cujo nome está em `names`;
/// - converte blocos `extern "js" { ... }` em `#[wasm_bindgen] extern "C" { ... }`.
fn collect_edits(items: &[Item], names: &BTreeSet<String>, offsets: &Offsets) -> Vec<Edit> {
    let mut edits = Vec::new();
    for item in items {
        match item {
            Item::Fn(f) if names.contains(&f.sig.ident.to_string()) => {
                let at = offsets.of(f.span().start());
                edits.push((at, at, format!("{WASM_BINDGEN_ATTR} ")));
            }
            Item::ForeignMod(m) if m.abi.name.as_ref().is_some_and(|n| n.value() == "js") => {
                let at = offsets.of(m.span().start());
                edits.push((at, at, format!("{WASM_BINDGEN_ATTR}\n")));
                if let Some(name) = &m.abi.name {
                    let lit = name.span();
                    edits.push((offsets.of(lit.start()), offsets.of(lit.end()), "\"C\"".into()));
                }
            }
            _ => {}
        }
    }
    edits
}

fn apply(code: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = code.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, &text);
    }
    out
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
    fn script_becomes_exported_main_function() {
        let names = BTreeSet::from(["add".to_string()]);
        let code = "let x = 1;\nextern \"js\" { fn alert(s: &str); }\nfn add() {}\n";
        let out = transform_script(code, &names, "__wk_main_index").unwrap();
        assert_eq!(
            out,
            format!(
                "{a}\npub fn __wk_main_index() {{\nlet x = 1;\n{a}\nextern \"C\" {{ fn alert(s: &str); }}\n{a} fn add() {{}}\n\n}}\n",
                a = WASM_BINDGEN_ATTR
            )
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
