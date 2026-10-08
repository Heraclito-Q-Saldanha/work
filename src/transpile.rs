use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use walkdir::{DirEntry, WalkDir};

use crate::{loader, parser, rust};

const IGNORED_DIRS: [&str; 3] = ["target", "dist", ".git"];

/// Página HTML gerada a partir de um `.wk`, com caminho relativo a `src/`.
pub struct Page {
    pub path: PathBuf,
    pub html: String,
}

/// Copia o projeto para `dest`, converte cada `.wk` em `.rs` (se houver Rust)
/// e devolve as páginas HTML geradas, já carregando o glue `glue_name`.js.
pub fn transpile_project(root: &Path, dest: &Path, glue_name: &str) -> Result<Vec<Page>> {
    copy_project(root, dest)?;

    let src = dest.join("src");
    let wk_files: Vec<PathBuf> = WalkDir::new(&src)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|e| e.path().extension().is_some_and(|x| x == "wk"))
        .map(DirEntry::into_path)
        .collect();

    let mut pages = Vec::new();
    for wk in wk_files {
        let source = fs::read_to_string(&wk).with_context(|| format!("lendo {}", wk.display()))?;
        let parsed = parser::parse(&source).with_context(|| format!("em {}", wk.display()))?;

        if let Some(code) = parsed.rust {
            let code = rust::transform(&code, &parsed.handlers)
                .with_context(|| format!("em {}", wk.display()))?;
            let rs = wk.with_extension("rs");
            if rs.exists() {
                bail!("{} conflita com {}", rs.display(), wk.display());
            }
            fs::write(&rs, code)?;
        }
        fs::remove_file(&wk)?;
        let path = wk.strip_prefix(&src)?.with_extension("html");
        let html = loader::render_page(&parsed.html, &path, glue_name);
        pages.push(Page { path, html });
    }
    Ok(pages)
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
