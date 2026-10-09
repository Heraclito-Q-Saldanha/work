mod builder;
mod loader;
mod manifest;
mod parser;
mod rust;
mod scaffold;
mod script;
mod serve;
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
    /// Cria um projeto novo em uma pasta `<nome>`
    New {
        /// Nome do projeto (e da pasta)
        name: String,
    },
    /// Cria um projeto na pasta atual, com o nome da pasta
    Init,
    /// Compila, serve o dist/ e recompila a cada mudança no projeto
    Run {
        /// Porta do servidor
        #[arg(short, long, default_value_t = 8080)]
        port: u16,
    },
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
        Command::New { name } => scaffold::new(&std::env::current_dir()?, &name),
        Command::Init => scaffold::init(&std::env::current_dir()?),
        Command::Run { port } => run(port),
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
        std::fs::write(out.join("src/routes").join(&page.path), &page.html)?;
    }
    println!("projeto transpilado em {}", out.display());
    Ok(())
}

fn build() -> Result<()> {
    build_project(&project_root()?)
}

fn run(port: u16) -> Result<()> {
    let root = project_root()?;
    serve::start(root.join("dist"), port)?;
    println!("servindo dist/ em http://localhost:{port} (Ctrl+C para sair)");
    loop {
        let before = serve::snapshot(&root);
        if let Err(e) = build_project(&root) {
            eprintln!("Error: {e:?}");
        }
        println!("aguardando mudanças...");
        // Mudanças feitas durante a compilação também disparam uma nova.
        if serve::snapshot(&root) != before {
            continue;
        }
        serve::wait_for_change(&root, &before);
        println!("mudança detectada, recompilando...");
    }
}

fn build_project(root: &std::path::Path) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let name = manifest::lib_name(root)?;
    let pages = transpile::transpile_project(root, tmp.path(), &name)?;
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
