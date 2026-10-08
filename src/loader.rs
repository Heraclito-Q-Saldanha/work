use std::path::Path;

/// Insere no HTML um módulo que carrega o glue do wasm e expõe as exportações em `window`,
/// para que atributos como `onclick="foo()"` as enxerguem.
pub fn inject(html: &str, page: &Path, glue_name: &str) -> String {
    let depth = page.components().count().saturating_sub(1);
    let prefix = if depth == 0 { "./".to_string() } else { "../".repeat(depth) };
    let script = format!(
        "<script type=\"module\">\n\
         import init, * as wasm from \"{prefix}{glue_name}.js\";\n\
         await init({{ module_or_path: new URL(\"{prefix}{glue_name}_bg.wasm\", import.meta.url) }});\n\
         for (const [k, v] of Object.entries(wasm)) {{\n  \
           if (k !== \"default\" && k !== \"initSync\") window[k] = v;\n\
         }}\n\
         </script>\n"
    );
    match html.to_ascii_lowercase().rfind("</body>") {
        Some(at) => format!("{}{script}{}", &html[..at], &html[at..]),
        None => format!("{html}{script}"),
    }
}
