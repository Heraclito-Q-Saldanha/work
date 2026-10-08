mod builder;
mod loader;
mod manifest;
mod parser;
mod rust;
mod script;
mod template;
mod transpile;

use anyhow::{Context, Result, bail};
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
}

fn main() -> Result<()> {
    let Cargo::Wk(Wk { command }) = Cargo::parse();
    match command {
        Command::Build => build(),
    }
}

fn build() -> Result<()> {
    let root = std::env::current_dir()?;
    if !root.join("Cargo.toml").is_file() {
        bail!("Cargo.toml não encontrado em {}", root.display());
    }

    let tmp = tempfile::tempdir()?;
    let name = manifest::lib_name(&root)?;
    let pages = transpile::transpile_project(&root, tmp.path(), &name)?;
    manifest::ensure_wasm_bindgen(tmp.path())?;
    let wasm = builder::compile_wasm(tmp.path(), &root.join("target/wk"))
        .context("compilando projeto")?;

    let dist = root.join("dist");
    builder::package(&dist, &wasm, &pages)?;
    println!("{} página(s) HTML + wasm em {}", pages.len(), dist.display());
    Ok(())
}
