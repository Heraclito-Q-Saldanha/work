//! `cargo wk new` e `cargo wk init`: geram um projeto novo, pronto para `cargo wk build`.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;

const PAGE: &str = r#"<script lang="rs">
	let mut count = 0;

	pub fn increment() {
		count += 1;
	}
</script>

<h1>Hello, wk!</h1>
<button onclick="increment()">Clicked {count} times</button>
"#;

const GITIGNORE: &str = "/target\n/dist\n";

/// Cria a pasta `name` e o projeto dentro dela.
pub fn new(parent: &Path, name: &str) -> Result<()> {
    let dir = parent.join(name);
    if dir.exists() {
        bail!("{} já existe", dir.display());
    }
    let package = package_name(&dir_name(&dir)?)?;
    fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;
    write_project(&dir, &package)
}

/// Cria o projeto em `dir`, que leva o nome da pasta.
pub fn init(dir: &Path) -> Result<()> {
    let package = package_name(&dir_name(dir)?)?;
    write_project(dir, &package)
}

fn dir_name(dir: &Path) -> Result<String> {
    let abs = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let name = dir
        .file_name()
        .or_else(|| abs.file_name())
        .and_then(|n| n.to_str())
        .map(String::from);
    name.context("não foi possível determinar o nome do projeto")
}

/// Converte para um nome de pacote válido: minúsculas, `[a-z0-9_-]`, sem começar por dígito.
fn package_name(raw: &str) -> Result<String> {
    let mut name: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_string();
    if name.is_empty() {
        bail!("`{raw}` não pode virar um nome de projeto");
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert_str(0, "wk-");
    }
    Ok(name)
}

fn write_project(dir: &Path, package: &str) -> Result<()> {
    let manifest = dir.join("Cargo.toml");
    let page = dir.join("src/routes/+page.wk");
    for path in [&manifest, &page] {
        if path.exists() {
            bail!("{} já existe", path.display());
        }
    }
    fs::create_dir_all(page.parent().unwrap())?;
    fs::write(
        &manifest,
        format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
    )?;
    fs::write(&page, PAGE)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, GITIGNORE)?;
    }
    println!("projeto `{package}` criado em {}", dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_are_sanitized() {
        assert_eq!(package_name("My App").unwrap(), "my-app");
        assert_eq!(package_name("9lives").unwrap(), "wk-9lives");
        assert!(package_name("@@@").is_err());
    }

    #[test]
    fn new_writes_a_project_and_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        new(tmp.path(), "demo").unwrap();
        let manifest = fs::read_to_string(tmp.path().join("demo/Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"demo\""));
        assert!(tmp.path().join("demo/src/routes/+page.wk").is_file());
        assert!(new(tmp.path(), "demo").is_err());
    }

    #[test]
    fn init_uses_the_folder_name_and_keeps_existing_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Cool_Thing");
        fs::create_dir(&dir).unwrap();
        init(&dir).unwrap();
        let manifest = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"cool_thing\""));
        assert!(init(&dir).is_err());
    }
}
