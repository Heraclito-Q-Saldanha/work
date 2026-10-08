use std::path::Path;

/// Monta o documento HTML mínimo com `body` dentro de `<body>`, mais um módulo que carrega
/// o glue do wasm e expõe as exportações em `window`, para que atributos como
/// `onclick="foo()"` as enxerguem.
pub fn render_page(body: &str, page: &Path, glue_name: &str, main_fn: Option<&str>, regions: bool) -> String {
    let depth = page.components().count().saturating_sub(1);
    let prefix = if depth == 0 { "./".to_string() } else { "../".repeat(depth) };
    let run_main = main_fn.map_or(String::new(), |f| format!("wasm.{f}();\n"));
    let region_runtime = if regions { REGION_RUNTIME } else { "" };
    let script = format!(
        "<script type=\"module\">\n\
         {region_runtime}\
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

/// Ponte usada pelo wasm para atualizar o conteúdo entre os comentários `<!--wk:N-->` e `<!--/wk:N-->`.
const REGION_RUNTIME: &str = "\
window.__wk = {
  start: null, end: null, last: new Map(),
  set(id, html) {
    if (this.last.get(id) === html) return;
    this.last.set(id, html);
    if (!this.start) {
      this.start = new Map();
      this.end = new Map();
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_COMMENT);
      for (let n; (n = walker.nextNode());) {
        if (n.data.startsWith('wk:')) this.start.set(+n.data.slice(3), n);
        else if (n.data.startsWith('/wk:')) this.end.set(+n.data.slice(4), n);
      }
    }
    const range = document.createRange();
    range.setStartAfter(this.start.get(id));
    range.setEndBefore(this.end.get(id));
    range.deleteContents();
    range.insertNode(range.createContextualFragment(html));
  },
};
";

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
        let out = render_page("<p>fuu</p>\n", Path::new("a/b.html"), "app", Some("__wk_main_a__b"), false);
        assert!(out.starts_with("<!DOCTYPE html>\n<html>\n  <head></head>\n  <body>\n    <p>fuu</p>\n"));
        assert!(out.contains("\"../app.js\""));
        assert!(out.contains("wasm.__wk_main_a__b();"));
        assert!(out.ends_with("  </body>\n</html>\n"));
    }
}
