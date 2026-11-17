//! A tiny static file server for projects with no dev script: plain HTML in the project,
//! served on this machine only. Dotfiles and dot-directories (`.git`, `.taskmaster`, ...)
//! are never served, and there are no directory listings.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};

/// Where a static site starts, relative to its root: `index.html`, or else the first of
/// the usual folders that has one.
pub fn start_page(root: &Path) -> Option<&'static str> {
    ["index.html", "app/index.html", "public/index.html", "dist/index.html", "src/index.html"]
        .into_iter()
        .find(|page| root.join(page).is_file())
}

/// Whether the project has no script of its own that would serve it.
pub fn has_no_scripts(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("package.json")) else { return true };
    let Ok(package) = serde_json::from_str::<serde_json::Value>(&text) else { return true };
    !["dev", "start", "preview"].iter().any(|name| package["scripts"][name].is_string())
}

fn decode(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The file a request path names under `root`, if it may be served.
pub fn resolve(root: &Path, request: &str) -> Option<PathBuf> {
    let path = decode(request.split(['?', '#']).next().unwrap_or(""))?;
    let mut file = root.to_path_buf();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        if segment.starts_with('.') || segment.contains(['\\', '\0']) {
            return None;
        }
        file.push(segment);
    }
    if file.is_dir() {
        file.push("index.html");
    }
    // A link out of the project is not followed.
    let real = file.canonicalize().ok()?;
    (real.is_file() && real.starts_with(root.canonicalize().ok()?)).then_some(real)
}

fn content_type(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn serve_file(State(root): State<PathBuf>, uri: Uri) -> Response {
    let path = uri.path();
    if path == "/" && !root.join("index.html").is_file() {
        return match start_page(&root) {
            Some(page) => Redirect::temporary(&format!("/{page}")).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    match resolve(&root, path) {
        Some(file) => match tokio::fs::read(&file).await {
            Ok(body) => ([(header::CONTENT_TYPE, content_type(&file)), (header::CACHE_CONTROL, "no-store")], body).into_response(),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        },
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Serve `root` on `listener` until the task is dropped.
pub async fn serve(listener: tokio::net::TcpListener, root: PathBuf) {
    let app = Router::new().fallback(serve_file).with_state(root);
    let _ = axum::serve(listener, app).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tm-static-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for file in files {
            let path = dir.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        dir
    }

    #[test]
    fn the_start_page_is_picked_in_order() {
        assert_eq!(start_page(&site("a", &["index.html", "app/index.html"])), Some("index.html"));
        assert_eq!(start_page(&site("b", &["src/index.html", "public/index.html"])), Some("public/index.html"));
        assert_eq!(start_page(&site("c", &["app/index.html", "dist/index.html"])), Some("app/index.html"));
        assert_eq!(start_page(&site("d", &["readme.md"])), None);
    }

    #[test]
    fn dotfiles_and_escapes_are_not_served() {
        let root = site("e", &["app/index.html", ".git/config", ".taskmaster/config", "app/.env"]);
        assert!(resolve(&root, "/app/index.html").is_some());
        assert!(resolve(&root, "/app/").is_some());
        for path in ["/.git/config", "/.taskmaster/config", "/.git", "/.git/", "/app/.env", "/%2egit/config", "/app/../.git/config", "/app/%2e%2e/x", "/"] {
            assert!(resolve(&root, path).is_none(), "{path}");
        }
    }

    #[tokio::test]
    async fn it_answers_over_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = site("f", &["app/index.html", ".git/config"]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(serve(listener, root));
        let get = |path: &'static str| async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).await.unwrap();
            text
        };
        assert!(get("/").await.starts_with("HTTP/1.1 307"));
        assert!(get("/app/index.html").await.starts_with("HTTP/1.1 200"));
        assert!(get("/.git/config").await.starts_with("HTTP/1.1 404"));
        assert!(get("/app/").await.starts_with("HTTP/1.1 200"));
        task.abort();
    }
}
