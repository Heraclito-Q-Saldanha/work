use std::path::Path;

use crate::{script::bind_fn_name, template::Bind};

/// Monta o documento HTML mínimo com `body` dentro de `<body>`, mais um módulo que carrega
/// o glue do wasm e expõe as exportações em `window`, para que atributos como
/// `onclick="foo()"` as enxerguem.
pub fn render_page(
    body: &str,
    page: &Path,
    glue_name: &str,
    main_fn: Option<&str>,
    binds: &[Bind],
) -> String {
    let depth = page.components().count().saturating_sub(1);
    let prefix = if depth == 0 {
        "./".to_string()
    } else {
        "../".repeat(depth)
    };
    let mut run_main = main_fn.map_or(String::new(), |f| format!("wasm.{f}();\n"));
    if let Some(main_fn) = main_fn.filter(|_| !binds.is_empty()) {
        let list: Vec<_> = binds
            .iter()
            .map(|b| serde_json::json!([b.id, b.event, bind_fn_name(main_fn, b.id), b.prop]))
            .collect();
        run_main.push_str(&format!(
            "for (const [id, event, fn, prop] of {}) {{\n  \
               const el = document.querySelector(`[data-wk-b${{id}}]`);\n  \
               el.addEventListener(event, () => wasm[fn](el[prop]));\n\
             }}\n",
            serde_json::Value::Array(list)
        ));
    }
    let script = format!(
        "<script type=\"module\">\n\
         {REGION_RUNTIME}\
         import init, * as wasm from \"{prefix}{glue_name}.js\";\n\
         await init({{ module_or_path: new URL(\"{prefix}{glue_name}_bg.wasm\", import.meta.url) }});\n\
         for (const [k, v] of Object.entries(wasm)) {{\n  \
           if (k !== \"default\" && k !== \"initSync\" && !k.startsWith(\"__wk_\")) window[k] = v;\n\
         }}\n\
         window.__wk.wasm = wasm;\n\
         {run_main}\
         </script>"
    );
    let content = indent(body.trim());
    let script = indent(&script);
    let content = if content.is_empty() {
        script
    } else {
        format!("{content}\n{script}")
    };
    format!("<!DOCTYPE html>\n<html>\n  <head></head>\n  <body>\n{content}\n  </body>\n</html>\n")
}

/// Ponte usada pelo wasm: atualiza o conteúdo entre os comentários `<!--wk:N-->` e
/// `<!--/wk:N-->`, propriedades de elementos ligados por `bind:` e despacha chamadas de
/// atributos de evento dos componentes (`__wk.c(id).nome(...)`).
const REGION_RUNTIME: &str = "\
window.__wk = {
  wasm: null, start: new Map(), end: new Map(), last: new WeakMap(),
  find(id) {
    const live = (n) => n && n.isConnected;
    if (!live(this.start.get(id)) || !live(this.end.get(id))) {
      this.start.clear();
      this.end.clear();
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_COMMENT);
      for (let n; (n = walker.nextNode());) {
        if (n.data.startsWith('wk:')) this.start.set(+n.data.slice(3), n);
        else if (n.data.startsWith('/wk:')) this.end.set(+n.data.slice(4), n);
      }
    }
    const start = this.start.get(id), end = this.end.get(id);
    return start && end ? [start, end] : null;
  },
  set(id, html) {
    const found = this.find(id);
    if (!found) return;
    const [start, end] = found;
    if (this.last.get(start) === html) return;
    this.last.set(start, html);
    const range = document.createRange();
    range.setStartAfter(start);
    range.setEndBefore(end);
    range.deleteContents();
    const fragment = range.createContextualFragment(html);
    const inits = this.hook(fragment);
    range.insertNode(fragment);
    for (const [instance, init] of inits) this.wasm.__wk_init(instance, init);
  },
  set_prop(id, prop, value) {
    const el = document.querySelector(`[data-wk-b${id}]`);
    if (el && el[prop] !== value) el[prop] = value;
  },
  c(instance) {
    return new Proxy({}, {
      get: (_, name) => (...args) => this.wasm.__wk_call(instance, name, args),
    });
  },
  // Liga os `bind:` dos componentes (`data-wk-bN=\"instância|N|propriedade|evento\"`) e devolve
  // as funções que levam o valor inicial da variável ao elemento, depois de inserido.
  hook(root) {
    const inits = [];
    for (const el of root.querySelectorAll('*')) {
      for (const attr of el.attributes) {
        if (!attr.name.startsWith('data-wk-b') || !attr.value) continue;
        const [instance, n, prop, event] = attr.value.split('|');
        el.addEventListener(event, () =>
          this.wasm.__wk_call(+instance, `__wk_bind_${n}`, [el[prop]]));
        inits.push([+instance, `__wk_init_${n}`]);
      }
    }
    return inits;
  },
};
";

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("    {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_fragment_in_document() {
        let out = render_page(
            "<p>fuu</p>\n",
            Path::new("a/b.html"),
            "app",
            Some("__wk_main_a__b"),
            &[],
        );
        assert!(
            out.starts_with("<!DOCTYPE html>\n<html>\n  <head></head>\n  <body>\n    <p>fuu</p>\n")
        );
        assert!(out.contains("\"../app.js\""));
        assert!(out.contains("wasm.__wk_main_a__b();"));
        assert!(out.ends_with("  </body>\n</html>\n"));
    }
}
