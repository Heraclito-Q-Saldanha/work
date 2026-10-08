use std::path::Path;

/// Monta o documento HTML mínimo com `body` dentro de `<body>`, mais um módulo que carrega
/// o glue do wasm e expõe as exportações em `window`, para que atributos como
/// `onclick="foo()"` as enxerguem.
pub fn render_page(body: &str, page: &Path, glue_name: &str, main_fn: Option<&str>) -> String {
    let depth = page.components().count().saturating_sub(1);
    let prefix = if depth == 0 { "./".to_string() } else { "../".repeat(depth) };
    let run_main = main_fn.map_or(String::new(), |f| format!("wasm.{f}();\n"));
    let script = format!(
        "<script type=\"module\">\n\
         import init, * as wasm from \"{prefix}{glue_name}.js\";\n\
         await init({{ module_or_path: new URL(\"{prefix}{glue_name}_bg.wasm\", import.meta.url) }});\n\
         for (const [k, v] of Object.entries(wasm)) {{\n  \
           if (k !== \"default\" && k !== \"initSync\" && !k.startsWith(\"__wk_\")) window[k] = v;\n\
         }}\n\
         {run_main}\
         </script>"
    );
    let content = indent(body.trim());
    let script = indent(&script);
    let content = if content.is_empty() { script } else { format!("{content}\n{script}") };
    format!("<!DOCTYPE html>\n<html>\n  <head></head>\n  <body>\n{content}\n  </body>\n</html>\n")
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| if l.trim().is_empty() { String::new() } else { format!("    {l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_fragment_in_document() {
        let out = render_page("<p>fuu</p>\n", Path::new("a/b.html"), "app", Some("__wk_main_a__b"));
        assert!(out.starts_with("<!DOCTYPE html>\n<html>\n  <head></head>\n  <body>\n    <p>fuu</p>\n"));
        assert!(out.contains("\"../app.js\""));
        assert!(out.contains("wasm.__wk_main_a__b();"));
        assert!(out.ends_with("  </body>\n</html>\n"));
    }
}
