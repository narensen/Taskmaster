//! Warm language-server loop, code intelligence (`definition`, `references`, `outline`,
//! `symbols`, `diagnostics`), and deterministic per-unit context packs (Items 2.1 & 2.2).
//!
//! When a language server binary (`typescript-language-server`, `pyright-langserver`,
//! `basedpyright-langserver`, or `rust-analyzer`) is available on `PATH`, [`LspManager`]
//! keeps one warm JSON-RPC stdio process per `(root, language)`, feeds edited files via
//! `textDocument/didOpen` / `didChange` overlays, restarts crashed servers automatically,
//! and falls back cleanly to zero-dependency structural analysis & fast mechanical checks
//! when no LSP binary is installed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspLanguage {
    TypeScript,
    Python,
    Rust,
}

impl LspLanguage {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" => Some(Self::TypeScript),
            "py" => Some(Self::Python),
            "rs" => Some(Self::Rust),
            _ => None,
        }
    }

    pub fn language_id(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Rust => "rust",
        }
    }

    /// Candidate `(binary, args)` pairs tried in order.
    pub fn candidates(self) -> &'static [(&'static str, &'static [&'static str])] {
        match self {
            Self::TypeScript => &[("typescript-language-server", &["--stdio"])],
            Self::Python => &[
                ("basedpyright-langserver", &["--stdio"]),
                ("pyright-langserver", &["--stdio"]),
            ],
            Self::Rust => &[("rust-analyzer", &[])],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub severity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub exported: bool,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceHit {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
}

struct LspSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    versions: BTreeMap<String, i32>,
    diagnostics: BTreeMap<String, Vec<Diagnostic>>,
    restarts: u32,
}

impl LspSession {
    fn spawn(root: &Path, bin: &str, args: &[&str], restarts: u32) -> anyhow::Result<Self> {
        let mut child = Command::new(bin)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("missing lsp stdin"))?;
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| anyhow::anyhow!("missing lsp stdout"))?);
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            versions: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
            restarts,
        };
        let root_uri = path_to_uri(root);
        let init_id = session.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {},
                        "synchronization": { "didSave": true }
                    }
                }
            }),
        )?;
        let _ = session.read_until_response(init_id)?;
        session.send_notification("initialized", json!({}))?;
        Ok(session)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send_raw(&mut self, payload: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_vec(payload)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send_raw(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn read_message(&mut self) -> anyhow::Result<Value> {
        let mut content_length: usize = 0;
        let mut line = String::new();
        loop {
            line.clear();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                anyhow::bail!("lsp stdout closed");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some(val) = trimmed.strip_prefix("Content-Length:") {
                content_length = val.trim().parse()?;
            }
        }
        let mut buf = vec![0u8; content_length];
        self.stdout.read_exact(&mut buf)?;
        Ok(serde_json::from_slice(&buf)?)
    }

    fn read_until_response(&mut self, target_id: u64) -> anyhow::Result<Value> {
        for _ in 0..64 {
            let msg = self.read_message()?;
            if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
                self.record_publish_diagnostics(&msg["params"]);
                continue;
            }
            if msg.get("id").and_then(Value::as_u64) == Some(target_id) {
                return Ok(msg);
            }
        }
        anyhow::bail!("lsp timed out waiting for response {target_id}")
    }

    fn record_publish_diagnostics(&mut self, params: &Value) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else { return };
        let path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
        let mut list = Vec::new();
        if let Some(items) = params.get("diagnostics").and_then(Value::as_array) {
            for item in items {
                let line = item["range"]["start"]["line"].as_u64().unwrap_or(0) as usize + 1;
                let column = item["range"]["start"]["character"].as_u64().unwrap_or(0) as usize + 1;
                let severity = match item["severity"].as_u64().unwrap_or(1) {
                    1 => "error",
                    2 => "warning",
                    3 => "info",
                    _ => "hint",
                }
                .to_string();
                let code = item.get("code").and_then(|c| {
                    c.as_str()
                        .map(str::to_string)
                        .or_else(|| c.as_i64().map(|n| n.to_string()))
                });
                let message = item["message"].as_str().unwrap_or("").to_string();
                let source = item["source"].as_str().unwrap_or("lsp").to_string();
                list.push(Diagnostic { path: path.clone(), line, column, severity, code, message, source });
            }
        }
        self.diagnostics.insert(path, list);
    }

    fn sync_overlay(&mut self, file: &Path, lang: LspLanguage, content: &str) -> anyhow::Result<()> {
        let uri = path_to_uri(file);
        if let Some(ver) = self.versions.get_mut(&uri) {
            *ver += 1;
            let version = *ver;
            self.send_notification(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": content }]
                }),
            )
        } else {
            self.versions.insert(uri.clone(), 1);
            self.send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": lang.language_id(),
                        "version": 1,
                        "text": content
                    }
                }),
            )
        }
    }
}

impl Drop for LspSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type LspCommand = (String, Vec<String>);

/// Manages warm per-run LSP servers with automatic crash restart and clean fallback.
#[derive(Clone, Default)]
pub struct LspManager {
    sessions: Arc<Mutex<BTreeMap<(PathBuf, LspLanguage), LspSession>>>,
    overrides: Arc<Mutex<BTreeMap<LspLanguage, LspCommand>>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the command used for a language (used in tests with a fake LSP binary).
    pub fn with_command(self, lang: LspLanguage, bin: impl Into<String>, args: Vec<String>) -> Self {
        self.overrides.lock().unwrap().insert(lang, (bin.into(), args));
        self
    }

    /// Ensure a warm LSP server is running for `(root, lang)`, restarting if it crashed.
    /// Returns `Ok(true)` if a warm server is active, `Ok(false)` if no binary is installed.
    pub fn ensure_warm(&self, root: &Path, lang: LspLanguage) -> anyhow::Result<bool> {
        let key = (root.to_path_buf(), lang);
        let mut sessions = self.sessions.lock().unwrap();
        let mut restarts = 0u32;
        if let Some(existing) = sessions.get_mut(&key) {
            if existing.is_alive() {
                return Ok(true);
            }
            restarts = existing.restarts + 1;
            sessions.remove(&key);
        }
        let custom = self.overrides.lock().unwrap().get(&lang).cloned();
        if let Some((bin, args)) = custom {
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let sess = LspSession::spawn(root, &bin, &arg_refs, restarts)?;
            sessions.insert(key, sess);
            return Ok(true);
        }
        for (bin, args) in lang.candidates() {
            if let Ok(sess) = LspSession::spawn(root, bin, args, restarts) {
                sessions.insert(key, sess);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Number of times a server had to be restarted after a crash.
    pub fn restart_count(&self, root: &Path, lang: LspLanguage) -> u32 {
        self.sessions
            .lock()
            .unwrap()
            .get(&(root.to_path_buf(), lang))
            .map(|s| s.restarts)
            .unwrap_or(0)
    }

    /// Push a file overlay and collect diagnostics via LSP (restarting on crash),
    /// falling back to deterministic static diagnostics when no LSP binary is available.
    pub fn diagnostics_for_files(&self, root: &Path, rel_paths: &[String]) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for rel in rel_paths {
            let full = root.join(rel);
            let Ok(content) = std::fs::read_to_string(&full) else { continue };
            let Some(lang) = LspLanguage::from_path(&full) else { continue };
            let mut got_lsp = false;
            if let Ok(true) = self.ensure_warm(root, lang) {
                let key = (root.to_path_buf(), lang);
                let mut sessions = self.sessions.lock().unwrap();
                if let Some(sess) = sessions.get_mut(&key) {
                    if sess.sync_overlay(&full, lang, &content).is_ok() {
                        // Ask pull diagnostics if supported; ignore method-not-found cleanly.
                        if let Ok(req_id) = sess.send_request(
                            "textDocument/diagnostic",
                            json!({ "textDocument": { "uri": path_to_uri(&full) } }),
                        )
                            && let Ok(resp) = sess.read_until_response(req_id)
                        {
                            if let Some(items) = resp["result"]["items"].as_array() {
                                let mut list = Vec::new();
                                for item in items {
                                    list.push(Diagnostic {
                                        path: rel.clone(),
                                        line: item["range"]["start"]["line"].as_u64().unwrap_or(0) as usize + 1,
                                        column: item["range"]["start"]["character"].as_u64().unwrap_or(0) as usize + 1,
                                        severity: match item["severity"].as_u64().unwrap_or(1) {
                                            1 => "error",
                                            2 => "warning",
                                            _ => "info",
                                        }
                                        .to_string(),
                                        code: item.get("code").and_then(Value::as_str).map(str::to_string),
                                        message: item["message"].as_str().unwrap_or("").to_string(),
                                        source: item["source"].as_str().unwrap_or("lsp").to_string(),
                                    });
                                }
                                out.extend(list);
                                got_lsp = true;
                            } else if let Some(published) = sess.diagnostics.get(&full.to_string_lossy().to_string()) {
                                out.extend(published.iter().cloned().map(|mut d| {
                                    d.path = rel.clone();
                                    d
                                }));
                                got_lsp = true;
                            }
                        }
                    } else {
                        // Server died mid-call; drop so next call restarts it cleanly.
                        sessions.remove(&key);
                    }
                }
            }
            if !got_lsp {
                out.extend(fallback_syntax_diagnostics(rel, &content, lang));
            }
        }
        out
    }

    /// Kill all warm sessions for a project root.
    pub fn shutdown_root(&self, root: &Path) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|(r, _), _| r != root);
    }
}

fn path_to_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", abs.to_string_lossy())
}

/// Zero-dependency structural sanity checks used when no LSP binary is installed.
pub fn fallback_syntax_diagnostics(rel_path: &str, content: &str, lang: LspLanguage) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    // Check unbalanced delimiters (braces/parens) ignoring string literals & line comments.
    if matches!(lang, LspLanguage::TypeScript | LspLanguage::Rust) {
        let mut brace_depth: i32 = 0;
        let mut paren_depth: i32 = 0;
        for (idx, line) in content.lines().enumerate() {
            let code = strip_line_comment(line);
            let mut in_str: Option<char> = None;
            let mut escaped = false;
            for ch in code.chars() {
                if let Some(q) = in_str {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == q {
                        in_str = None;
                    }
                    continue;
                }
                match ch {
                    '"' | '\'' | '`' => in_str = Some(ch),
                    '{' => brace_depth += 1,
                    '}' => {
                        brace_depth -= 1;
                        if brace_depth < 0 {
                            out.push(Diagnostic {
                                path: rel_path.to_string(),
                                line: idx + 1,
                                column: 1,
                                severity: "error".into(),
                                code: Some("SYNTAX_BRACE".into()),
                                message: "Unexpected closing `}`".into(),
                                source: "fallback".into(),
                            });
                            brace_depth = 0;
                        }
                    }
                    '(' => paren_depth += 1,
                    ')' => paren_depth -= 1,
                    _ => {}
                }
            }
        }
        if brace_depth > 0 {
            out.push(Diagnostic {
                path: rel_path.to_string(),
                line: content.lines().count().max(1),
                column: 1,
                severity: "error".into(),
                code: Some("SYNTAX_BRACE".into()),
                message: format!("Unclosed `{{` ({brace_depth} remaining)"),
                source: "fallback".into(),
            });
        }
        if paren_depth != 0 {
            out.push(Diagnostic {
                path: rel_path.to_string(),
                line: content.lines().count().max(1),
                column: 1,
                severity: "error".into(),
                code: Some("SYNTAX_PAREN".into()),
                message: "Unbalanced parentheses `(` / `)`".into(),
                source: "fallback".into(),
            });
        }
    }
    out
}

fn strip_line_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    let mut prev = '\0';
    for (idx, ch) in line.char_indices() {
        if let Some(q) = in_str {
            if ch == q && prev != '\\' {
                in_str = None;
            }
        } else if matches!(ch, '"' | '\'' | '`') {
            in_str = Some(ch);
        } else if ch == '/' && prev == '/' {
            return &line[..idx.saturating_sub(1)];
        }
        prev = ch;
    }
    line
}

// ---------------------------------------------------------------------------
// Item 2.2: Code intelligence (outline, symbols, definition, references) and
// deterministic per-unit Context Packs.
// ---------------------------------------------------------------------------

/// Extract top-level symbols (functions, types, classes, interfaces, structs, traits, enums, consts)
/// from one source file.
pub fn outline_file(root: &Path, rel_path: &str) -> Vec<SymbolInfo> {
    let full = root.join(rel_path);
    let Ok(content) = std::fs::read_to_string(&full) else { return Vec::new() };
    extract_symbols(rel_path, &content)
}

/// Pure symbol extractor supporting TS/JS, Rust, and Python source text.
pub fn extract_symbols(rel_path: &str, content: &str) -> Vec<SymbolInfo> {
    let mut out = Vec::new();
    let ext = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') || line.starts_with("/*") {
            continue;
        }
        match ext.as_str() {
            "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" => {
                let (exported, rest) = if let Some(r) = line.strip_prefix("export default ") {
                    (true, r)
                } else if let Some(r) = line.strip_prefix("export ") {
                    (true, r)
                } else {
                    (false, line)
                };
                let rest = rest.strip_prefix("async ").unwrap_or(rest);
                for (prefix, kind) in [
                    ("function ", "function"),
                    ("interface ", "interface"),
                    ("type ", "type"),
                    ("class ", "class"),
                    ("enum ", "enum"),
                    ("const ", "const"),
                ] {
                    if let Some(after) = rest.strip_prefix(prefix)
                        && let Some(name) = leading_ident(after)
                    {
                        out.push(SymbolInfo {
                            name: name.to_string(),
                            kind: kind.to_string(),
                            path: rel_path.to_string(),
                            line: idx + 1,
                            exported,
                            signature: line.trim_end_matches('{').trim().to_string(),
                        });
                        break;
                    }
                }
            }
            "rs" => {
                let (exported, rest) = if let Some(r) = line.strip_prefix("pub(crate) ") {
                    (true, r)
                } else if let Some(r) = line.strip_prefix("pub ") {
                    (true, r)
                } else {
                    (false, line)
                };
                let rest = rest.strip_prefix("async ").unwrap_or(rest);
                for (prefix, kind) in [
                    ("fn ", "fn"),
                    ("struct ", "struct"),
                    ("enum ", "enum"),
                    ("trait ", "trait"),
                    ("type ", "type"),
                    ("const ", "const"),
                ] {
                    if let Some(after) = rest.strip_prefix(prefix)
                        && let Some(name) = leading_ident(after)
                    {
                        out.push(SymbolInfo {
                            name: name.to_string(),
                            kind: kind.to_string(),
                            path: rel_path.to_string(),
                            line: idx + 1,
                            exported,
                            signature: line.trim_end_matches('{').trim().to_string(),
                        });
                        break;
                    }
                }
            }
            "py" if !raw.starts_with(' ') && !raw.starts_with('\t') => {
                let rest = line.strip_prefix("async ").unwrap_or(line);
                for (prefix, kind) in [("def ", "def"), ("class ", "class")] {
                    if let Some(after) = rest.strip_prefix(prefix)
                        && let Some(name) = leading_ident(after)
                    {
                        out.push(SymbolInfo {
                            name: name.to_string(),
                            kind: kind.to_string(),
                            path: rel_path.to_string(),
                            line: idx + 1,
                            exported: !name.starts_with('_'),
                            signature: line.trim_end_matches(':').trim().to_string(),
                        });
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn leading_ident(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(trimmed.len());
    if end == 0 { None } else { Some(&trimmed[..end]) }
}

/// Collect workspace source files (skipping build/vendor directories).
pub fn workspace_source_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_sources(root, root, &mut out);
    out.sort();
    out
}

fn walk_sources(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | ".flow" | ".taskmaster" | "node_modules" | "target" | "dist" | "build" | ".venv" | "__pycache__"
        ) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_sources(root, &path, out);
        } else if LspLanguage::from_path(&path).is_some()
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Find definitions of `symbol` across the project (exported definitions ranked first).
pub fn find_definitions(root: &Path, symbol: &str) -> Vec<SymbolInfo> {
    let mut hits = Vec::new();
    for rel in workspace_source_files(root) {
        for sym in outline_file(root, &rel) {
            if sym.name == symbol {
                hits.push(sym);
            }
        }
    }
    hits.sort_by(|a, b| b.exported.cmp(&a.exported).then_with(|| a.path.cmp(&b.path)).then_with(|| a.line.cmp(&b.line)));
    hits
}

/// Find word-boundary references to `symbol` across the project.
pub fn find_references(root: &Path, symbol: &str, limit: usize) -> Vec<ReferenceHit> {
    if symbol.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for rel in workspace_source_files(root) {
        let Ok(content) = std::fs::read_to_string(root.join(&rel)) else { continue };
        for (idx, line) in content.lines().enumerate() {
            let mut offset = 0;
            while let Some(pos) = line[offset..].find(symbol) {
                let start = offset + pos;
                let end = start + symbol.len();
                let left_ok = start == 0 || !is_ident_byte(line.as_bytes()[start - 1]);
                let right_ok = end >= line.len() || !is_ident_byte(line.as_bytes()[end]);
                if left_ok && right_ok {
                    hits.push(ReferenceHit {
                        path: rel.clone(),
                        line: idx + 1,
                        column: start + 1,
                        snippet: line.trim().chars().take(140).collect(),
                    });
                    if hits.len() >= limit {
                        return hits;
                    }
                }
                offset = end;
            }
        }
    }
    hits
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Build a deterministic, sorted Tier-1 per-unit Context Pack containing the unit's
/// contracts, imported neighbour signatures, and exported symbols (Item 2.2).
pub fn build_context_pack(
    root: &Path,
    contract_paths: &[String],
    neighbour_files: &[String],
    max_bytes: usize,
) -> String {
    let mut files: BTreeSet<String> = BTreeSet::new();
    for p in contract_paths {
        files.insert(p.clone());
    }
    for p in neighbour_files {
        files.insert(p.clone());
    }
    let mut out = String::new();
    for rel in files {
        let symbols = outline_file(root, &rel);
        let exported: Vec<&SymbolInfo> = symbols.iter().filter(|s| s.exported).collect();
        if exported.is_empty() {
            continue;
        }
        let mut block = format!("### `{rel}`\n");
        for sym in exported {
            block.push_str(&format!("- L{}: `{}`\n", sym.line, sym.signature));
        }
        if out.len() + block.len() > max_bytes {
            break;
        }
        out.push_str(&block);
    }
    if out.is_empty() {
        return String::new();
    }
    format!("## Unit context pack (contracts & neighbour signatures)\n\n{out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_symbols_definitions_references_and_context_pack() {
        let dir = std::env::temp_dir().join(format!("tm-lsp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/contracts.ts"),
            "export interface CartItem {\n  id: string;\n  price: number;\n}\nexport function totalPrice(items: CartItem[]): number { return 0; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/page.ts"),
            "import { CartItem, totalPrice } from './contracts';\nexport const sum = (xs: CartItem[]) => totalPrice(xs);\n",
        )
        .unwrap();

        let defs = find_definitions(&dir, "CartItem");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "src/contracts.ts");
        assert!(defs[0].exported);

        let refs = find_references(&dir, "totalPrice", 20);
        assert_eq!(refs.len(), 3);

        let pack = build_context_pack(&dir, &["src/contracts.ts".into()], &["src/page.ts".into()], 4096);
        assert!(pack.contains("export interface CartItem"));
        assert!(pack.contains("export function totalPrice"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_diagnostics_catches_unclosed_braces_without_lsp() {
        let diags = fallback_syntax_diagnostics("src/broken.ts", "export function f() {\n  return 1;\n", LspLanguage::TypeScript);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.as_deref(), Some("SYNTAX_BRACE"));
    }

    #[test]
    fn lsp_manager_restarts_crashed_server_and_reads_diagnostics() {
        // Fake JSON-RPC LSP written in python3: answers initialize & textDocument/diagnostic.
        let dir = std::env::temp_dir().join(format!("tm-lsp-fake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake_lsp.py");
        std::fs::write(
            &script,
            r#"import sys, json
def read_msg():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: return None
        if line in (b'\r\n', b'\n'): break
        if line.lower().startswith(b'content-length:'):
            length = int(line.split(b':', 1)[1].strip())
    return json.loads(sys.stdin.buffer.read(length))
def send(obj):
    raw = json.dumps(obj).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(raw)}\r\n\r\n".encode() + raw)
    sys.stdout.buffer.flush()
while True:
    m = read_msg()
    if m is None: break
    method = m.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/diagnostic":
        send({"jsonrpc": "2.0", "id": m["id"], "result": {"items": [
            {"range": {"start": {"line": 0, "character": 0}}, "severity": 1, "code": "TS2322", "message": "Type mismatch", "source": "fake-ts"}
        ]}})
"#,
        )
        .unwrap();
        std::fs::write(dir.join("app.ts"), "export const x: number = 'oops';\n").unwrap();

        let mgr = LspManager::new().with_command(
            LspLanguage::TypeScript,
            "python3",
            vec![script.to_string_lossy().to_string()],
        );
        let diags = mgr.diagnostics_for_files(&dir, &["app.ts".into()]);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.as_deref(), Some("TS2322"));
        assert_eq!(diags[0].source, "fake-ts");

        // Simulate a server crash by killing its child process, then confirm ensure_warm restarts it.
        {
            let mut sessions = mgr.sessions.lock().unwrap();
            let sess = sessions.get_mut(&(dir.clone(), LspLanguage::TypeScript)).unwrap();
            let _ = sess.child.kill();
            let _ = sess.child.wait();
        }
        assert!(mgr.ensure_warm(&dir, LspLanguage::TypeScript).unwrap());
        assert_eq!(mgr.restart_count(&dir, LspLanguage::TypeScript), 1);
        mgr.shutdown_root(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
