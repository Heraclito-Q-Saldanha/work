use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::builder::WASM_BINDGEN_VERSION;

/// Nome do artefato gerado pelo cargo para a lib (sem extensão).
pub fn lib_name(root: &Path) -> Result<String> {
    let manifest = read(root)?;
    let name = manifest
        .get("lib")
        .and_then(|l| l.get("name"))
        .or_else(|| manifest.get("package").and_then(|p| p.get("name")))
        .and_then(|n| n.as_str())
        .context("nome do pacote não encontrado no Cargo.toml")?;
    Ok(name.replace('-', "_"))
}

/// Garante que o projeto em `project` depende de `wasm-bindgen` na versão do CLI embutido.
/// Também adiciona `wasm-bindgen-futures` quando o código gerado usa `async`.
pub fn ensure_wasm_bindgen(project: &Path) -> Result<()> {
    ensure_dependency(
        project,
        "wasm-bindgen",
        &format!("wasm-bindgen@={WASM_BINDGEN_VERSION}"),
    )?;
    if uses_async(project) {
        ensure_dependency(project, "wasm-bindgen-futures", "wasm-bindgen-futures")?;
    }
    Ok(())
}

fn ensure_dependency(project: &Path, name: &str, spec: &str) -> Result<()> {
    let manifest = read(project)?;
    if manifest
        .get("dependencies")
        .is_some_and(|d| d.get(name).is_some())
    {
        return Ok(());
    }
    let status = Command::new("cargo")
        .args(["add", spec])
        .current_dir(project)
        .status()
        .context("executando cargo add")?;
    if !status.success() {
        bail!("não foi possível adicionar {name} ao projeto");
    }
    Ok(())
}

fn uses_async(project: &Path) -> bool {
    walkdir::WalkDir::new(project.join("src"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .filter(|e| e.file_name() != "__wk_rt.rs")
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .any(|code| code.contains("async"))
}

fn read(dir: &Path) -> Result<toml::Table> {
    let path = dir.join("Cargo.toml");
    fs::read_to_string(&path)
        .with_context(|| format!("lendo {}", path.display()))?
        .parse()
        .with_context(|| format!("parseando {}", path.display()))
}
