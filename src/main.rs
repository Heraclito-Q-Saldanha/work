mod builder;
mod loader;
mod manifest;
mod parser;
mod rust;
mod script;
mod template;
mod transpile;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Invocado como `cargo wk <comando>`.
#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum Cargo {
    Wk(Wk),
}

/// Compila projetos com arquivos .wk (HTML com `<script lang="rs">`).
#[derive(Args)]
#[command(version, about)]
struct Wk {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Gera o diretório dist/ com o .wasm e os .html do projeto
    Build,
    /// Espelha o projeto transpilado (sem compilar) para inspeção
    Expand {
        /// Diretório de saída (recriado a cada execução)
        #[arg(short, long, default_value = "target/wk-expand")]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    let Cargo::Wk(Wk { command }) = Cargo::parse();
    match command {
        Command::Build => build(),
        Command::Expand { out } => expand(out),
    }
}

fn project_root() -> Result<PathBuf> {
    let root = std::env::current_dir()?;
    if !root.join("Cargo.toml").is_file() {
        bail!("Cargo.toml não encontrado em {}", root.display());
    }
    Ok(root)
}

fn expand(out: PathBuf) -> Result<()> {
    let root = project_root()?;
    let out = root.join(out);
    if out.exists() {
        std::fs::remove_dir_all(&out).with_context(|| format!("limpando {}", out.display()))?;
    }
    let name = manifest::lib_name(&root)?;
    let pages = transpile::transpile_project(&root, &out, &name)?;
    for page in &pages {
        std::fs::write(out.join("src").join(&page.path), &page.html)?;
    }
    println!("projeto transpilado em {}", out.display());
    Ok(())
}

fn build() -> Result<()> {
    let root = project_root()?;

    let tmp = tempfile::tempdir()?;
    let name = manifest::lib_name(&root)?;
    let pages = transpile::transpile_project(&root, tmp.path(), &name)?;
    manifest::ensure_wasm_bindgen(tmp.path())?;
    let wasm =
        builder::compile_wasm(tmp.path(), &root.join("target/wk")).context("compilando projeto")?;

    let dist = root.join("dist");
    builder::package(&dist, &wasm, &pages)?;
    println!(
        "{} página(s) HTML + wasm em {}",
        pages.len(),
        dist.display()
    );
    Ok(())
}
