use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use walkdir::{DirEntry, WalkDir};

use crate::{loader, parser, rust, script, template};

const IGNORED_DIRS: [&str; 3] = ["target", "dist", ".git"];

/// Página HTML gerada a partir de um `.wk`, com caminho relativo a `src/`.
pub struct Page {
    pub path: PathBuf,
    pub html: String,
}

/// Copia o projeto para `dest` e processa os arquivos especiais de `src/`:
/// - `foo.wk` vira `foo.html` e, se houver `<script lang="rs">`, `foo.rs`;
/// - `foo.wk.rs` vira apenas `foo.rs`, sem página HTML.
///
/// Devolve as páginas HTML geradas, já carregando o glue `glue_name`.js.
pub fn transpile_project(root: &Path, dest: &Path, glue_name: &str) -> Result<Vec<Page>> {
    copy_project(root, dest)?;

    let src = dest.join("src");
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

    for file in files {
        match FileKind::of(&file) {
            FileKind::Page => {
                let source = fs::read_to_string(&file)
                    .with_context(|| format!("lendo {}", file.display()))?;
                let parsed =
                    parser::parse(&source).with_context(|| format!("em {}", file.display()))?;
                handlers.extend(parsed.handlers.iter().cloned());

                let path = file.strip_prefix(&src)?.with_extension("html");
                let template = template::compile(&parsed.html)
                    .with_context(|| format!("em {}", file.display()))?;
                let has_regions = !template.regions.is_empty();
                let main_fn = (parsed.rust.is_some() || has_regions).then(|| main_fn_name(&path));
                if let Some(main_fn) = &main_fn {
                    let code = script::transform(
                        parsed.rust.as_deref().unwrap_or_default(),
                        &parsed.handlers,
                        main_fn,
                        &template.regions,
                    )
                    .with_context(|| format!("em {}", file.display()))?;
                    write_rs(&file, &code)?;
                }
                fs::remove_file(&file)?;
                let html = loader::render_page(
                    &template.html,
                    &path,
                    glue_name,
                    main_fn.as_deref(),
                    has_regions,
                );
                pages.push(Page { path, html });
            }
            FileKind::RustOnly => rust_only.push(file),
            FileKind::Other => {}
        }
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
    Ok(pages)
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
    RustOnly,
    Other,
}

impl FileKind {
    fn of(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(".wk.rs") {
            Self::RustOnly
        } else if name.ends_with(".wk") {
            Self::Page
        } else {
            Self::Other
        }
    }
}

/// Escreve `code` em `foo.rs`, ao lado de `foo.wk` ou `foo.wk.rs`.
fn write_rs(origin: &Path, code: &str) -> Result<()> {
    let mut stem = origin.with_extension("");
    if stem.extension().is_some_and(|e| e == "wk") {
        stem.set_extension("");
    }
    let rs = stem.with_extension("rs");
    if rs.exists() {
        bail!("{} conflita com {}", rs.display(), origin.display());
    }
    fs::write(rs, code)?;
    Ok(())
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
