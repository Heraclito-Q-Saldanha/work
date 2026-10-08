mod builder;
mod parser;

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    input: PathBuf,
    #[arg(short, long, default_value = "release")]
    out_dir: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let source =
        fs::read_to_string(&cli.input).with_context(|| format!("lendo {}", cli.input.display()))?;
    let name = cli
        .input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("index");

    let parsed = parser::parse(&source)?;
    builder::build_release(&parsed, name, &cli.out_dir)?;
    println!("gerado em {}", cli.out_dir.display());

    Ok(())
}
