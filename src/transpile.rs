use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use walkdir::{DirEntry, WalkDir};

use crate::{loader, parser, rust, script, template};

const RUNTIME: &str = include_str!("runtime/wk_rt.rs");

const IGNORED_DIRS: [&str; 3] = ["target", "dist", ".git"];

/// Página HTML gerada a partir de um `.wk`, com caminho relativo a `src/`.
pub struct Page {
    pub path: PathBuf,
    pub html: String,
}

/// Arquivo que define uma rota, dentro de `src/routes/`.
const PAGE_FILE: &str = "+page.wk";

/// Copia o projeto para `dest` e processa os arquivos especiais de `src/`:
/// - `routes/**/+page.wk` vira `index.html` na mesma pasta (relativa a `routes/`) e, se houver
///   código Rust ou template dinâmico, `+page.rs`;
/// - `Foo.wk` fora de `routes/` é um componente e vira o módulo `Foo.rs`, sem página HTML;
/// - `foo.wk.rs` vira apenas `foo.rs`, sem página HTML;
/// - `src/lib.rs` (e um `mod.rs` para cada pasta sem o seu) é gerado, declarando os módulos
///   do projeto; o runtime compartilhado vai para `src/__wk_rt.rs`.
///
/// Devolve as páginas HTML geradas, já carregando o glue `glue_name`.js.
pub fn transpile_project(root: &Path, dest: &Path, glue_name: &str) -> Result<Vec<Page>> {
    copy_project(root, dest)?;

    let src = dest.join("src");
    if src.join("lib.rs").exists() {
        bail!(
            "src/lib.rs é gerado pelo cargo-wk; remova-o (os módulos são declarados automaticamente)"
        );
    }
    let routes = src.join("routes");
    let files: Vec<PathBuf> = WalkDir::new(&src)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|e| e.file_type().is_file())
        .map(DirEntry::into_path)
        .collect();

    let mut pages = Vec::new();
    let mut handlers = BTreeSet::new();
    let mut rust_only = Vec::new();
    let mut components = Vec::new();
    let mut route_mods: Vec<(String, PathBuf)> = Vec::new();

    for file in files {
        match FileKind::of(&file, &routes) {
            FileKind::Page => {
                let source = fs::read_to_string(&file)
                    .with_context(|| format!("lendo {}", file.display()))?;
                let parsed =
                    parser::parse(&source).with_context(|| format!("em {}", file.display()))?;
                handlers.extend(parsed.handlers.iter().cloned());

                let route_dir = file.parent().unwrap().strip_prefix(&routes)?;
                let path = route_dir.join("index.html");
                let template = template::compile(&parsed.html)
                    .with_context(|| format!("em {}", file.display()))?;
                let is_dynamic = !template.regions.is_empty() || !template.binds.is_empty();
                let main_fn = (parsed.rust.is_some() || is_dynamic).then(|| main_fn_name(&path));
                if let Some(main_fn) = &main_fn {
                    let code = script::transform(
                        parsed.rust.as_deref().unwrap_or_default(),
                        &parsed.handlers,
                        main_fn,
                        &template.regions,
                        &template.binds,
                    )
                    .with_context(|| format!("em {}", file.display()))?;
                    let rs = write_rs(&file, &code)?;
                    route_mods.push((main_fn.clone(), rs.strip_prefix(&src)?.to_path_buf()));
                }
                fs::remove_file(&file)?;
                let html = loader::render_page(
                    &template.html,
                    &path,
                    glue_name,
                    main_fn.as_deref(),
                    &template.binds,
                );
                pages.push(Page { path, html });
            }
            FileKind::Component => components.push(file),
            FileKind::RustOnly => rust_only.push(file),
            FileKind::Other => {}
        }
    }

    for file in components {
        let source =
            fs::read_to_string(&file).with_context(|| format!("lendo {}", file.display()))?;
        let code = component_code(&file, &source, &mut handlers)
            .with_context(|| format!("em {}", file.display()))?;
        write_rs(&file, &code)?;
        fs::remove_file(&file)?;
    }

    // Os `.wk.rs` não têm HTML próprio: expõem as funções chamadas por qualquer página.
    for file in rust_only {
        let code =
            fs::read_to_string(&file).with_context(|| format!("lendo {}", file.display()))?;
        let code =
            rust::transform(&code, &handlers).with_context(|| format!("em {}", file.display()))?;
        write_rs(&file, &code)?;
        fs::remove_file(&file)?;
    }
    fs::write(src.join("__wk_rt.rs"), RUNTIME)?;
    write_modules(&src, &route_mods)?;
    Ok(pages)
}

/// Traduz um componente `Nome.wk` para o módulo Rust com `Props` e `__wk_create`.
fn component_code(file: &Path, source: &str, handlers: &mut BTreeSet<String>) -> Result<String> {
    let name = file
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if !is_component_name(name) {
        bail!("o nome de um componente deve ser um identificador começando com maiúscula");
    }
    let parsed = parser::parse(source)?;
    let code = parsed.rust.as_deref().unwrap_or_default();
    // Só as funções do próprio script são redirecionadas para a instância; as demais
    // chamadas continuam globais (por exemplo, as de um `.wk.rs`).
    let own = script::fn_names(code)?;
    let called: BTreeSet<String> = own.intersection(&parsed.handlers).cloned().collect();
    handlers.extend(parsed.handlers.difference(&own).cloned());
    let template = template::compile_with(
        &parsed.html,
        &template::Options {
            component_handlers: Some(&called),
        },
    )?;
    script::transform_component(code, &called, &template)
}

fn is_component_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Nome único da função de entrada de uma página, derivado do seu caminho.
fn main_fn_name(page: &Path) -> String {
    let sanitized: String = page
        .with_extension("")
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' => "__".to_string(),
            c if c.is_ascii_alphanumeric() => c.to_string(),
            _ => "_".to_string(),
        })
        .collect();
    format!("__wk_main_{sanitized}")
}

enum FileKind {
    Page,
    Component,
    RustOnly,
    Other,
}

impl FileKind {
    fn of(path: &Path, routes: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(".wk.rs") {
            Self::RustOnly
        } else if name == PAGE_FILE && path.starts_with(routes) {
            Self::Page
        } else if name.ends_with(".wk") && !path.starts_with(routes) {
            Self::Component
        } else {
            Self::Other
        }
    }
}

/// Declara os módulos do projeto: `src/lib.rs` e, para cada pasta com Rust e sem `mod.rs`,
/// um `mod.rs`. Os módulos das páginas apontam para o seu `+page.rs` (o nome não é um
/// módulo válido); o resto de `routes/` não é módulo.
fn write_modules(src: &Path, route_mods: &[(String, PathBuf)]) -> Result<()> {
    let mut code = String::from("// Gerado pelo cargo-wk.\n");
    code.push_str(&module_decls(src, true)?);
    for (main_fn, rs) in route_mods {
        let module = main_fn.trim_start_matches("__wk_main_");
        let path = rs.to_string_lossy().replace('\\', "/");
        code.push_str(&format!("#[path = \"{path}\"]\nmod __route_{module};\n"));
    }
    fs::write(src.join("lib.rs"), code)?;
    Ok(())
}

/// Linhas `pub mod x;` dos módulos em `dir`, gerando o `mod.rs` das subpastas que precisarem.
fn module_decls(dir: &Path, root: bool) -> Result<String> {
    let mut files = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_stem().and_then(|n| n.to_str()).map(String::from) else {
            continue;
        };
        if path.is_dir() {
            let ignored = IGNORED_DIRS.contains(&name.as_str()) || (root && name == "routes");
            if !ignored && has_rust(&path) {
                dirs.insert(name);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && !["lib", "main", "mod"].contains(&name.as_str())
        {
            files.insert(name);
        }
    }

    let mut out = String::new();
    for name in files.iter().chain(dirs.difference(&files)) {
        let is_ident = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !is_ident {
            continue;
        }
        let sub = dir.join(name);
        if dirs.contains(name) && !files.contains(name) && !sub.join("mod.rs").is_file() {
            let decls = module_decls(&sub, false)?;
            fs::write(
                sub.join("mod.rs"),
                format!("// Gerado pelo cargo-wk.\n{decls}"),
            )?;
        }
        if name.chars().any(|c| c.is_ascii_uppercase()) {
            out.push_str("#[allow(non_snake_case)]\n");
        }
        out.push_str(&format!("pub mod {name};\n"));
    }
    Ok(out)
}

fn has_rust(dir: &Path) -> bool {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .any(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "rs"))
}

/// Escreve `code` em `foo.rs`, ao lado de `foo.wk` ou `foo.wk.rs`.
fn write_rs(origin: &Path, code: &str) -> Result<PathBuf> {
    let mut stem = origin.with_extension("");
    if stem.extension().is_some_and(|e| e == "wk") {
        stem.set_extension("");
    }
    // `+page.wk` vira `+page.rs`: `with_extension` não tocaria no `+page` sem extensão.
    let rs = stem.with_extension("rs");
    if rs.exists() {
        bail!("{} conflita com {}", rs.display(), origin.display());
    }
    fs::write(&rs, code)?;
    Ok(rs)
}

fn copy_project(root: &Path, dest: &Path) -> Result<()> {
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        e.depth() == 0
            || !(e.file_type().is_dir() && IGNORED_DIRS.iter().any(|d| e.file_name() == *d))
    });
    for entry in walker {
        let entry = entry?;
        let target = dest.join(entry.path().strip_prefix(root)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
