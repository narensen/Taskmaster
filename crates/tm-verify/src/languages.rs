//! File-to-file connections for languages other than JavaScript.
//!
//! Each function returns project files the source depends on. Package and standard-library
//! imports are ignored, because they are not files in this repository.

use std::collections::{BTreeMap, BTreeSet};

/// `parent file -> (module name -> child file)`, from Rust `mod` declarations.
pub type RustModules = BTreeMap<String, BTreeMap<String, String>>;
/// `namespace -> files that declare it`.
pub type CSharpNamespaces = BTreeMap<String, BTreeSet<String>>;

pub fn connections(
    file: &str,
    source: &str,
    sources: &BTreeMap<String, String>,
    rust: &RustModules,
    csharp: &CSharpNamespaces,
) -> BTreeSet<String> {
    match extension(file) {
        Some("rs") => rust_connections(file, source, sources, rust),
        Some("py") => python_connections(file, source, sources),
        Some("go") => go_connections(file, source, sources),
        Some("java" | "kt" | "kts") => jvm_connections(source, sources),
        Some("c" | "h" | "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx") => c_connections(file, source, sources),
        Some("rb") => ruby_connections(file, source, sources),
        Some("php") => php_connections(file, source, sources),
        Some("cs") => csharp_connections(file, source, csharp),
        Some("css" | "scss" | "sass") => css_connections(file, source, sources),
        _ => BTreeSet::new(),
    }
}

pub fn rust_modules(sources: &BTreeMap<String, String>) -> RustModules {
    let mut children: RustModules = BTreeMap::new();
    for (file, source) in sources {
        if extension(file) != Some("rs") {
            continue;
        }
        for (name, path) in rust_mod_decls(source) {
            let child = if path.is_empty() {
                rust_mod_file(file, &name, sources)
            } else {
                relative(file, &path).filter(|p| sources.contains_key(p))
            };
            if let Some(child) = child {
                children.entry(file.clone()).or_default().insert(name, child);
            }
        }
    }
    children
}

pub fn csharp_namespaces(sources: &BTreeMap<String, String>) -> CSharpNamespaces {
    let mut out: CSharpNamespaces = BTreeMap::new();
    for (file, source) in sources {
        if extension(file) != Some("cs") {
            continue;
        }
        if let Some(ns) = source.split_whitespace().skip_while(|w| *w != "namespace").nth(1) {
            let ns = ns.trim_end_matches(['{', ';']).trim();
            if !ns.is_empty() && ns.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                out.entry(ns.to_string()).or_default().insert(file.clone());
            }
        }
    }
    out
}

fn rust_mod_decls(source: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (index, _) in source.match_indices("mod ") {
        if !boundary(source, index) {
            continue;
        }
        let after = source[index + 4..].trim_start();
        let name: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if name.is_empty() {
            continue;
        }
        let rest = after[name.len()..].trim_start();
        if !rest.starts_with(';') {
            continue;
        }
        let path = path_attribute_before(source, index).unwrap_or_default();
        out.push((name, path));
    }
    out
}

/// `#[path = "file.rs"]` immediately above a `mod`, if present.
fn path_attribute_before(source: &str, mod_at: usize) -> Option<String> {
    let before = source[..mod_at].trim_end();
    let start = before.rfind("#[path")?;
    let attr = &before[start..];
    if attr.contains(';') || attr.contains('\n') && attr.matches('\n').count() > 2 {
        return None;
    }
    let quoted = attr.split('"').nth(1)?;
    Some(quoted.to_string())
}

fn rust_mod_file(file: &str, name: &str, sources: &BTreeMap<String, String>) -> Option<String> {
    let dir = module_dir(file);
    [join(&dir, &format!("{name}.rs")), join(&dir, &format!("{name}/mod.rs"))]
        .into_iter()
        .find(|c| sources.contains_key(c))
}

fn module_dir(file: &str) -> String {
    if file == "lib.rs" || file == "main.rs" || file == "mod.rs" {
        return String::new();
    }
    if let Some(dir) = file
        .strip_suffix("/mod.rs")
        .or_else(|| file.strip_suffix("/lib.rs"))
        .or_else(|| file.strip_suffix("/main.rs"))
    {
        return dir.to_string();
    }
    file.trim_end_matches(".rs").to_string()
}

fn rust_connections(
    file: &str,
    source: &str,
    sources: &BTreeMap<String, String>,
    modules: &RustModules,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(kids) = modules.get(file) {
        out.extend(kids.values().cloned());
    }
    let mut rest = source;
    while let Some(at) = rest.find("include!") {
        let after = &rest[at + "include!".len()..];
        if let Some(path) = after.split('"').nth(1)
            && let Some(target) = relative(file, path).filter(|p| sources.contains_key(p)) {
                out.insert(target);
            }
        rest = &rest[at + "include!".len()..];
    }
    for statement in rust_use_statements(source) {
        for path in expand_use(&statement) {
            if let Some(target) = resolve_rust_use(file, &path, modules, sources)
                && target != file {
                    out.insert(target);
                }
        }
    }
    out
}

fn rust_use_statements(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = source[i..].find("use") {
        let at = i + rel;
        if boundary(source, at) {
            let after = source[at + 3..].trim_start();
            let mut depth: i32 = 0;
            let mut end = after.len();
            for (n, c) in after.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => depth = depth.saturating_sub(1),
                    ';' if depth == 0 => {
                        end = n;
                        break;
                    }
                    _ => {}
                }
            }
            let stmt = after[..end].trim();
            if !stmt.is_empty() {
                out.push(stmt.to_string());
            }
        }
        i = at + 3;
    }
    out
}

fn expand_use(statement: &str) -> Vec<Vec<String>> {
    let statement = statement.split(" as ").next().unwrap_or(statement).trim();
    expand_path(statement)
}

fn expand_path(path: &str) -> Vec<Vec<String>> {
    let path = path.trim().trim_end_matches(',');
    if let Some(open) = path.find('{') {
        let prefix = path[..open].trim().trim_end_matches("::");
        let close = path.rfind('}').unwrap_or(path.len());
        let mut out = Vec::new();
        for part in split_commas(&path[open + 1..close]) {
            for mut segs in expand_path(part) {
                let mut all = segments(prefix);
                all.append(&mut segs);
                if !all.is_empty() {
                    out.push(all);
                }
            }
        }
        return out;
    }
    let segs = segments(path);
    if segs.is_empty() { Vec::new() } else { vec![segs] }
}

fn split_commas(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (n, c) in inner.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(inner[start..n].trim());
                start = n + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

fn segments(path: &str) -> Vec<String> {
    path.split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*")
        .map(str::to_string)
        .collect()
}

fn resolve_rust_use(
    file: &str,
    segs: &[String],
    modules: &RustModules,
    sources: &BTreeMap<String, String>,
) -> Option<String> {
    let (mut current, rest) = match segs.first().map(String::as_str) {
        Some("crate") => (crate_root(file, sources)?.to_string(), &segs[1..]),
        Some("self") => (file.to_string(), &segs[1..]),
        Some("super") => {
            let mut current = parent_module(file, modules)?.to_string();
            let mut i = 1;
            while segs.get(i).map(String::as_str) == Some("super") {
                current = parent_module(&current, modules)?.to_string();
                i += 1;
            }
            (current, &segs[i..])
        }
        _ => return None,
    };
    for seg in rest {
        match modules.get(&current).and_then(|kids| kids.get(seg)) {
            Some(child) => current = child.clone(),
            None => break,
        }
    }
    if sources.contains_key(&current) { Some(current) } else { None }
}

fn parent_module<'a>(file: &str, modules: &'a RustModules) -> Option<&'a str> {
    modules.iter().find(|(_, kids)| kids.values().any(|c| c == file)).map(|(parent, _)| parent.as_str())
}

fn crate_root<'a>(file: &'a str, sources: &'a BTreeMap<String, String>) -> Option<&'a str> {
    if is_crate_root(file) {
        return Some(file);
    }
    let mut dir = parent(file);
    loop {
        for name in ["lib.rs", "main.rs"] {
            let candidate = join(dir, name);
            if let Some((key, _)) = sources.get_key_value(&candidate) {
                return Some(key.as_str());
            }
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent(dir);
    }
}

fn is_crate_root(file: &str) -> bool {
    file.ends_with("/lib.rs")
        || file.ends_with("/main.rs")
        || file == "lib.rs"
        || file == "main.rs"
        || file.contains("/bin/") && file.ends_with(".rs") && !file.ends_with("/mod.rs")
        || parent(file) == "bin"
}

fn python_connections(file: &str, source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("import ") {
            for part in rest.split(',') {
                let module = part.split_whitespace().next().unwrap_or("");
                if let Some(target) = resolve_python(file, module, 0, sources) {
                    out.insert(target);
                }
            }
        } else if let Some(rest) = line.strip_prefix("from ") {
            let Some((module, names)) = rest.split_once(" import ") else { continue };
            let (level, module) = dots(module.trim());
            if let Some(target) = resolve_python(file, module, level, sources) {
                out.insert(target);
            }
            for name in names.split(',') {
                let name = name.split_whitespace().next().unwrap_or("");
                if name.is_empty() || name == "*" || name == "(" || name == ")" {
                    continue;
                }
                let combined = if module.is_empty() { name.to_string() } else { format!("{module}.{name}") };
                if let Some(target) = resolve_python(file, &combined, level, sources) {
                    out.insert(target);
                }
            }
        }
    }
    out
}

fn dots(module: &str) -> (usize, &str) {
    let level = module.chars().take_while(|c| *c == '.').count();
    (level, module[level..].trim())
}

fn resolve_python(file: &str, module: &str, level: usize, sources: &BTreeMap<String, String>) -> Option<String> {
    if module.is_empty() {
        return None;
    }
    let rels = python_candidates(module);
    if level > 0 {
        let mut dir = parent(file);
        for _ in 1..level {
            if dir.is_empty() {
                return None;
            }
            dir = parent(dir);
        }
        return rels.into_iter().map(|rel| join(dir, &rel)).find(|full| sources.contains_key(full));
    }
    for root in ["", "src"] {
        for rel in &rels {
            let full = join(root, rel);
            if sources.contains_key(&full) {
                return Some(full);
            }
        }
    }
    unique_suffix(&rels, sources)
}

fn python_candidates(module: &str) -> Vec<String> {
    let slash = module.replace('.', "/");
    vec![format!("{slash}.py"), format!("{slash}/__init__.py")]
}

fn go_connections(file: &str, source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let Some((mod_dir, module)) = go_module(file, sources) else { return BTreeSet::new() };
    let prefix = format!("{module}/");
    let mut out = BTreeSet::new();
    for path in go_import_paths(source) {
        let rel = if path == module {
            ""
        } else if let Some(rest) = path.strip_prefix(&prefix) {
            rest
        } else {
            continue;
        };
        let dir = join(&mod_dir, rel);
        for other in sources.keys() {
            if extension(other) == Some("go")
                && !other.ends_with("_test.go")
                && parent(other) == dir
                && other != file
            {
                out.insert(other.clone());
            }
        }
    }
    out
}

fn go_module(file: &str, sources: &BTreeMap<String, String>) -> Option<(String, String)> {
    let mut dir = parent(file);
    loop {
        let manifest = join(dir, "go.mod");
        if let Some(text) = sources.get(&manifest) {
            let module = text.lines().find_map(|line| line.trim().strip_prefix("module "))?;
            let module = module.split_whitespace().next().unwrap_or("");
            if module.is_empty() {
                return None;
            }
            return Some((dir.to_string(), module.to_string()));
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent(dir);
    }
}

fn go_import_paths(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("import") else { continue };
        let rest = rest.trim_start();
        if rest.starts_with('(') {
            for inner in lines.by_ref() {
                let inner = inner.trim();
                if inner.starts_with(')') {
                    break;
                }
                if let Some(path) = quoted(inner) {
                    out.push(path);
                }
            }
        } else if let Some(path) = quoted(rest) {
            out.push(path);
        }
    }
    out
}

fn jvm_connections(source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim().trim_end_matches(';').trim();
        let Some(rest) = line.strip_prefix("import ") else { continue };
        let rest = rest.trim().trim_start_matches("static ");
        let star = rest.ends_with(".*");
        let path = rest.trim_end_matches(".*").replace('.', "/");
        if path.is_empty() {
            continue;
        }
        if star {
            for key in sources.keys() {
                if matches!(extension(key), Some("java" | "kt" | "kts")) && parent(key).ends_with(&path) {
                    out.insert(key.clone());
                }
            }
        } else {
            let rels = [format!("{path}.java"), format!("{path}.kt"), format!("{path}.kts")];
            if let Some(found) = unique_suffix(&rels, sources) {
                out.insert(found);
            }
        }
    }
    out
}

fn c_connections(file: &str, source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("#include") else { continue };
        let rest = rest.trim();
        if let Some(path) = rest.strip_prefix('"').and_then(|s| s.split('"').next()) {
            if let Some(target) = relative(file, path).filter(|p| sources.contains_key(p)) {
                out.insert(target);
            } else if let Some(target) = unique_suffix(&[path.to_string()], sources) {
                out.insert(target);
            }
        } else if let Some(path) = rest.strip_prefix('<').and_then(|s| s.split('>').next())
            && let Some(target) = unique_suffix(&[path.to_string()], sources) {
                out.insert(target);
            }
    }
    out
}

fn ruby_connections(file: &str, source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        if let Some(path) = string_after(line, "require_relative") {
            let path = ensure_ext(&path, "rb");
            if let Some(target) = relative(file, &path).filter(|p| sources.contains_key(p)) {
                out.insert(target);
            }
        } else if let Some(path) = string_after(line, "require") {
            let path = ensure_ext(&path, "rb");
            if let Some(target) = unique_suffix(&[path], sources) {
                out.insert(target);
            }
        }
    }
    out
}

fn php_connections(file: &str, source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        for keyword in ["require_once", "include_once", "require", "include"] {
            if let Some(path) = string_after(line, keyword) {
                if let Some(target) = relative(file, &path).filter(|p| sources.contains_key(p)) {
                    out.insert(target);
                } else if let Some(target) = unique_suffix(&[path], sources) {
                    out.insert(target);
                }
                break;
            }
        }
        if let Some(rest) = line.strip_prefix("use ") {
            let rest = rest.trim().trim_start_matches('\\').trim_end_matches(';').trim();
            let path = rest.split_whitespace().next().unwrap_or("").replace('\\', "/");
            if path.is_empty() || path.contains('{') {
                continue;
            }
            if let Some(target) = unique_suffix(&[format!("{path}.php")], sources) {
                out.insert(target);
            }
        }
    }
    out
}

fn csharp_connections(file: &str, source: &str, namespaces: &CSharpNamespaces) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim().trim_end_matches(';').trim();
        let Some(rest) = line.strip_prefix("using ") else { continue };
        let rest = rest.trim().trim_start_matches("static ");
        let ns = rest.split_whitespace().next().unwrap_or("");
        if let Some(files) = namespaces.get(ns) {
            out.extend(files.iter().filter(|f| f.as_str() != file).cloned());
        }
    }
    out
}

fn css_connections(file: &str, source: &str, sources: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        let path = string_after(line, "@import")
            .or_else(|| string_after(line, "@use"))
            .or_else(|| line.trim().strip_prefix("@import").and_then(first_quoted));
        let Some(path) = path else { continue };
        let path = path.trim_start_matches("url(").trim_end_matches(')').trim().trim_matches(['\'', '"']);
        if path.is_empty() || path.contains(':') {
            continue;
        }
        for candidate in css_candidates(file, path) {
            if sources.contains_key(&candidate) {
                out.insert(candidate);
                break;
            }
        }
    }
    out
}

fn css_candidates(file: &str, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(rel) = relative(file, path) {
        out.push(rel.clone());
        if extension(path).is_none() {
            for ext in ["css", "scss", "sass"] {
                out.push(format!("{rel}.{ext}"));
            }
        }
    }
    out
}

fn string_after(line: &str, keyword: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(keyword)?;
    if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    first_quoted(rest)
}

fn first_quoted(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|c| *c == b'"' || *c == b'\'')?;
    let quote = bytes[start];
    let end = bytes[start + 1..].iter().position(|c| *c == quote)?;
    Some(text[start + 1..start + 1 + end].to_string())
}

fn quoted(text: &str) -> Option<String> {
    let start = text.find('"')?;
    let end = text[start + 1..].find('"')?;
    Some(text[start + 1..start + 1 + end].to_string())
}

fn ensure_ext(path: &str, ext: &str) -> String {
    if extension(path).is_some() { path.to_string() } else { format!("{path}.{ext}") }
}

fn unique_suffix(rels: &[String], sources: &BTreeMap<String, String>) -> Option<String> {
    let mut found = None;
    for key in sources.keys() {
        if rels.iter().any(|rel| key == rel || key.ends_with(&format!("/{rel}"))) {
            if found.is_some() {
                return None;
            }
            found = Some(key.clone());
        }
    }
    found
}

fn relative(file: &str, spec: &str) -> Option<String> {
    let dir = parent(file);
    normalize(&join(dir, spec))
}

fn join(dir: &str, rest: &str) -> String {
    if dir.is_empty() || rest.is_empty() {
        format!("{dir}{rest}")
    } else if rest.starts_with('/') {
        rest.trim_start_matches('/').to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn extension(path: &str) -> Option<&str> {
    path.rsplit_once('.').and_then(|(stem, ext)| if stem.contains('/') || !stem.is_empty() { Some(ext) } else { None })
}

fn boundary(source: &str, index: usize) -> bool {
    let prev = source[..index].chars().next_back().unwrap_or(' ');
    !prev.is_ascii_alphanumeric() && prev != '_'
}

/// Resolve `.` and `..`. `None` when the path would leave the project.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}
