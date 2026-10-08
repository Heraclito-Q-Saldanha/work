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
pub fn ensure_wasm_bindgen(project: &Path) -> Result<()> {
    let manifest = read(project)?;
    if manifest
        .get("dependencies")
        .is_some_and(|d| d.get("wasm-bindgen").is_some())
    {
        return Ok(());
    }
    let status = Command::new("cargo")
        .args(["add", &format!("wasm-bindgen@={WASM_BINDGEN_VERSION}")])
        .current_dir(project)
        .status()
        .context("executando cargo add")?;
    if !status.success() {
        bail!("não foi possível adicionar wasm-bindgen ao projeto");
    }
    Ok(())
}

fn read(dir: &Path) -> Result<toml::Table> {
    let path = dir.join("Cargo.toml");
    fs::read_to_string(&path)
        .with_context(|| format!("lendo {}", path.display()))?
        .parse()
        .with_context(|| format!("parseando {}", path.display()))
}
