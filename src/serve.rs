//! `cargo wk run`: servidor estático para o `dist/` e observação de mudanças no projeto.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

use crate::transpile::IGNORED_DIRS;

type Snapshot = BTreeMap<PathBuf, (SystemTime, u64)>;

/// Sobe o servidor em segundo plano e devolve quando ele já está ouvindo.
pub fn start(dist: PathBuf, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("não foi possível ouvir a porta {port}"))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let dist = dist.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &dist);
            });
        }
    });
    Ok(())
}

fn handle(mut stream: TcpStream, dist: &Path) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    if method != "GET" && method != "HEAD" {
        return respond(&mut stream, 405, "text/plain", b"method not allowed", true);
    }
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let (status, mime, body) =
        match resolve(dist, path).and_then(|f| std::fs::read(&f).ok().map(|b| (f, b))) {
            Some((file, body)) => (200, mime_of(&file), body),
            None => (404, "text/plain", b"not found".to_vec()),
        };
    respond(&mut stream, status, mime, &body, method == "GET")
}

/// Mapeia a URL para um arquivo dentro de `dist`; pastas servem o seu `index.html`.
fn resolve(dist: &Path, url: &str) -> Option<PathBuf> {
    let rel = Path::new(url.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let mut file = dist.join(rel);
    if file.is_dir() {
        file.push("index.html");
    }
    file.is_file().then_some(file)
}

fn mime_of(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    body: &[u8],
    with_body: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if with_body {
        stream.write_all(body)?;
    }
    stream.flush()
}

/// Estado (data de modificação e tamanho) dos arquivos observados do projeto.
pub fn snapshot(root: &Path) -> Snapshot {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0
                || !(e.file_type().is_dir() && IGNORED_DIRS.iter().any(|d| e.file_name() == *d))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.into_path(), (meta.modified().ok()?, meta.len())))
        })
        .collect()
}

/// Bloqueia até que algo mude em relação a `before` e devolve o novo estado.
/// Espera o projeto ficar estável, para não compilar no meio de uma gravação.
pub fn wait_for_change(root: &Path, before: &Snapshot) -> Snapshot {
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let mut now = snapshot(root);
        if &now != before {
            loop {
                std::thread::sleep(Duration::from_millis(150));
                let next = snapshot(root);
                if next == now {
                    return now;
                }
                now = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn resolve_serves_index_and_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("about")).unwrap();
        std::fs::write(tmp.path().join("index.html"), "x").unwrap();
        std::fs::write(tmp.path().join("about/index.html"), "y").unwrap();
        assert!(resolve(tmp.path(), "/").unwrap().ends_with("index.html"));
        assert!(
            resolve(tmp.path(), "/about")
                .unwrap()
                .ends_with("about/index.html")
        );
        assert!(resolve(tmp.path(), "/../x").is_none());
        assert!(resolve(tmp.path(), "/nope").is_none());
    }

    #[test]
    fn server_answers_requests() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("index.html"), "hello").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        start(tmp.path().to_path_buf(), port).unwrap();
        let mut out = String::new();
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200") && out.ends_with("hello"));
    }

    #[test]
    fn snapshot_ignores_build_output_and_sees_edits() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("dist")).unwrap();
        std::fs::write(tmp.path().join("dist/a"), "1").unwrap();
        std::fs::write(tmp.path().join("b"), "1").unwrap();
        let before = snapshot(tmp.path());
        assert_eq!(before.len(), 1);
        std::fs::write(tmp.path().join("b"), "22").unwrap();
        assert_ne!(snapshot(tmp.path()), before);
    }
}
