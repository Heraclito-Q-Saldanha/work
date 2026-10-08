use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::transpile::Page;

const TARGET: &str = "wasm32-unknown-unknown";

/// Compila o projeto em `project` para wasm e devolve o caminho do `.wasm` gerado.
/// O diretório de target é compartilhado (`target_dir`) para reaproveitar cache.
pub fn compile_wasm(project: &Path, target_dir: &Path) -> Result<PathBuf> {
    let mut child = Command::new("cargo")
        .args(["rustc", "--release", "--lib", "--target", TARGET])
        .args(["--message-format=json-render-diagnostics", "--crate-type", "cdylib"])
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(project)
        .stdout(Stdio::piped())
        .spawn()
        .context("executando cargo")?;

    let mut wasm = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line?) else {
            continue;
        };
        if msg["reason"] == "compiler-artifact" {
            for f in msg["filenames"].as_array().into_iter().flatten() {
                if let Some(f) = f.as_str().filter(|f| f.ends_with(".wasm")) {
                    wasm = Some(PathBuf::from(f));
                }
            }
        }
    }

    if !child.wait()?.success() {
        bail!("cargo build falhou (o target {TARGET} está instalado? `rustup target add {TARGET}`)");
    }
    wasm.context("cargo não produziu um .wasm (o projeto precisa de um src/lib.rs)")
}

/// Recria `dist` com o `.wasm` e as páginas HTML, preservando a estrutura de `src/`.
pub fn package(dist: &Path, wasm: &Path, pages: &[Page]) -> Result<()> {
    if dist.exists() {
        fs::remove_dir_all(dist)?;
    }
    fs::create_dir_all(dist)?;
    fs::copy(wasm, dist.join(wasm.file_name().context("wasm sem nome")?))?;
    for page in pages {
        let out = dist.join(&page.path);
        fs::create_dir_all(out.parent().unwrap())?;
        fs::write(out, &page.html)?;
    }
    Ok(())
}
