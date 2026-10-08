use std::{fs, path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::parser::ParsedWk;

const CRATE_NAME: &str = "wk_module";
const TARGET: &str = "wasm32-unknown-unknown";

pub fn build_release(parsed: &ParsedWk, name: &str, out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)?;
    if let Some(rust) = &parsed.rust {
        let wasm = compile_wasm(rust)?;
        fs::write(out_dir.join(format!("{name}.wasm")), wasm)?;
    }
    fs::write(out_dir.join(format!("{name}.html")), &parsed.html)?;
    Ok(())
}

fn compile_wasm(rust: &str) -> Result<Vec<u8>> {
    let tmp = tempfile::tempdir()?;
    let proj = tmp.path();
    fs::create_dir(proj.join("src"))?;
    fs::write(proj.join("Cargo.toml"), manifest())?;
    fs::write(proj.join("src/lib.rs"), rust)?;

    let status = Command::new("cargo")
        .args(["build", "--release", "--target", TARGET])
        .current_dir(proj)
        .status()
        .context("executando cargo")?;
    if !status.success() {
        bail!(
            "cargo build falhou (o target {TARGET} está instalado? `rustup target add {TARGET}`)"
        );
    }

    let wasm = proj
        .join("target")
        .join(TARGET)
        .join("release")
        .join(format!("{CRATE_NAME}.wasm"));
    fs::read(&wasm).with_context(|| format!("lendo {}", wasm.display()))
}

fn manifest() -> String {
    format!(
        "[package]\nname = \"{CRATE_NAME}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n\n\
         [profile.release]\nopt-level = \"s\"\nlto = true\n"
    )
}
