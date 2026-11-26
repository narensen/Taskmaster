//! Cross-file import, inheritance, implementation, call, and test edge resolution.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{Confidence, Edge, EdgeKind, Node, NodeKind, Shard, file_node_id};
use crate::parse::{ParsedFile, RawCall, RawImport};

/// Repo-wide symbol lookup index built from existing shards and newly parsed files.
pub struct SymbolCatalog {
    /// All known repo source paths -> `(lang, file_id)`.
    pub files: BTreeMap<String, (String, String)>,
    /// `path -> short_name -> Vec<node_id>` (for top-level symbols in that file).
    pub file_top: HashMap<String, HashMap<String, Vec<String>>>,
    /// `path -> "Type.method" -> Vec<node_id>` (for class/impl/trait methods in that file).
    pub file_qualified: HashMap<String, HashMap<String, Vec<String>>>,
    /// `exported_short_name -> Vec<(path, node_id)>` across the whole repo.
    pub exported_by_name: HashMap<String, Vec<(String, String)>>,
    /// `exported_"Type.method" -> Vec<(path, node_id)>` across the whole repo.
    pub exported_qualified: HashMap<String, Vec<(String, String)>>,
    /// Rust crate root directories (e.g. `""` or `"crates/foo"`).
    pub rust_roots: BTreeSet<String>,
}

impl SymbolCatalog {
    /// Build a catalog from existing read-only shards plus newly parsed files (which supersede
    /// any existing shard at the same path).
    pub fn build(existing: &BTreeMap<String, Shard>, parsed: &[ParsedFile]) -> Self {
        let mut cat = Self {
            files: BTreeMap::new(),
            file_top: HashMap::new(),
            file_qualified: HashMap::new(),
            exported_by_name: HashMap::new(),
            exported_qualified: HashMap::new(),
            rust_roots: BTreeSet::new(),
        };
        let parsed_paths: BTreeSet<&str> = parsed.iter().map(|p| p.path.as_str()).collect();

        for (path, shard) in existing {
            if parsed_paths.contains(path.as_str()) {
                continue;
            }
            cat.index_file(path, &shard.lang, &shard.nodes);
        }
        for pf in parsed {
            cat.index_file(&pf.path, &pf.lang, &pf.nodes);
        }
        for v in cat.exported_by_name.values_mut() {
            v.sort();
            v.dedup();
        }
        for v in cat.exported_qualified.values_mut() {
            v.sort();
            v.dedup();
        }
        cat
    }

    fn index_file(&mut self, path: &str, lang: &str, nodes: &[Node]) {
        let fid = file_node_id(lang, path);
        self.files.insert(path.to_string(), (lang.to_string(), fid));
        if let Some(root) = rust_crate_root_of(path) {
            self.rust_roots.insert(root);
        }
        for node in nodes {
            if node.kind == NodeKind::File || node.kind == NodeKind::Unit {
                continue;
            }
            let qual = qualified_of_id(&node.id);
            if let Some(q) = qual {
                self.file_qualified
                    .entry(path.to_string())
                    .or_default()
                    .entry(q.to_string())
                    .or_default()
                    .push(node.id.clone());
                if node.exported {
                    self.exported_qualified
                        .entry(q.to_string())
                        .or_default()
                        .push((path.to_string(), node.id.clone()));
                }
            }
            if node.parent.is_none() {
                self.file_top
                    .entry(path.to_string())
                    .or_default()
                    .entry(node.name.clone())
                    .or_default()
                    .push(node.id.clone());
                if node.exported {
                    self.exported_by_name
                        .entry(node.name.clone())
                        .or_default()
                        .push((path.to_string(), node.id.clone()));
                }
            } else if node.exported {
                self.exported_by_name
                    .entry(node.name.clone())
                    .or_default()
                    .push((path.to_string(), node.id.clone()));
            }
        }
    }

    /// Resolve one `ParsedFile` into a complete canonical `Shard` with resolved edges.
    pub fn resolve_shard(&self, pf: &ParsedFile, owner: Option<String>) -> Shard {
        let mut edges: Vec<Edge> = Vec::new();
        // local_name -> (target_file, remote_name)
        let mut imported_syms: HashMap<String, (String, String)> = HashMap::new();
        // module_alias -> target_file
        let mut imported_mods: HashMap<String, String> = HashMap::new();
        let mut wildcard_files: Vec<String> = Vec::new();
        let is_test = tm_core::is_test_path(&pf.path);

        // 1. Resolve imports.
        for imp in &pf.imports {
            if let Some(target_file) = self.resolve_import_path(&pf.path, &pf.lang, imp) {
                if let Some((_, target_fid)) = self.files.get(&target_file) {
                    edges.push(Edge {
                        from: pf.file_id.clone(),
                        to: target_fid.clone(),
                        kind: EdgeKind::Imports,
                        conf: Confidence::Import,
                    });
                }
                if let Some(alias) = &imp.module_alias {
                    imported_mods.insert(alias.clone(), target_file.clone());
                }
                if imp.wildcard {
                    wildcard_files.push(target_file.clone());
                }
                for (remote, local) in &imp.names {
                    imported_syms.insert(local.clone(), (target_file.clone(), remote.clone()));
                    if let Some(ids) = self.lookup_in_file(&target_file, remote) {
                        for tid in ids {
                            edges.push(Edge {
                                from: pf.file_id.clone(),
                                to: tid.clone(),
                                kind: EdgeKind::Imports,
                                conf: Confidence::Import,
                            });
                            if is_test && !tm_core::is_test_path(&target_file) {
                                edges.push(Edge {
                                    from: pf.file_id.clone(),
                                    to: tid.clone(),
                                    kind: EdgeKind::Tests,
                                    conf: Confidence::Import,
                                });
                            }
                        }
                    }
                }
            } else if pf.lang == "python" && imp.names.len() == 1 && imp.module.is_empty() && imp.relative_dots > 0 {
                // `from . import foo` where `foo` is a sibling module file!
                for (remote, local) in &imp.names {
                    let sub_imp = RawImport {
                        module: remote.clone(),
                        relative_dots: imp.relative_dots,
                        names: vec![],
                        module_alias: Some(local.clone()),
                        wildcard: false,
                    };
                    if let Some(target_file) = self.resolve_import_path(&pf.path, &pf.lang, &sub_imp) {
                        if let Some((_, target_fid)) = self.files.get(&target_file) {
                            edges.push(Edge {
                                from: pf.file_id.clone(),
                                to: target_fid.clone(),
                                kind: EdgeKind::Imports,
                                conf: Confidence::Import,
                            });
                        }
                        imported_mods.insert(local.clone(), target_file);
                    }
                }
            } else if pf.lang == "python" && !imp.names.is_empty() {
                // Handle `from pkg import submodule` where `pkg/submodule.py` is a file.
                for (remote, local) in &imp.names {
                    let combined = if imp.module.is_empty() {
                        remote.clone()
                    } else {
                        format!("{}.{remote}", imp.module)
                    };
                    let sub_imp = RawImport {
                        module: combined,
                        relative_dots: imp.relative_dots,
                        names: vec![],
                        module_alias: Some(local.clone()),
                        wildcard: false,
                    };
                    if let Some(target_file) = self.resolve_import_path(&pf.path, &pf.lang, &sub_imp) {
                        if let Some((_, target_fid)) = self.files.get(&target_file) {
                            edges.push(Edge {
                                from: pf.file_id.clone(),
                                to: target_fid.clone(),
                                kind: EdgeKind::Imports,
                                conf: Confidence::Import,
                            });
                        }
                        imported_mods.insert(local.clone(), target_file);
                    }
                }
            }
        }

        // 2. Resolve inheritance & implements relations.
        for rel in &pf.relations {
            if let Some((target_id, conf)) = self.resolve_type_target(
                &pf.path,
                &rel.target,
                &imported_syms,
                &wildcard_files,
            ) && target_id != rel.from_id
            {
                edges.push(Edge {
                    from: rel.from_id.clone(),
                    to: target_id,
                    kind: rel.kind,
                    conf,
                });
            }
        }

        // 3. Resolve calls (and test edges when inside a test file).
        for call in &pf.calls {
            if is_builtin(&call.callee) && call.receiver.is_none() {
                continue;
            }
            let targets = self.resolve_call(
                &pf.path,
                &pf.lang,
                call,
                &imported_syms,
                &imported_mods,
                &wildcard_files,
            );
            for (target_id, target_path, conf) in targets {
                if target_id == call.from_id {
                    continue;
                }
                edges.push(Edge {
                    from: call.from_id.clone(),
                    to: target_id.clone(),
                    kind: EdgeKind::Calls,
                    conf,
                });
                if is_test && !tm_core::is_test_path(&target_path) {
                    edges.push(Edge {
                        from: call.from_id.clone(),
                        to: target_id,
                        kind: EdgeKind::Tests,
                        conf,
                    });
                }
            }
        }

        let mut nodes = pf.nodes.clone();
        if let Some(ref own) = owner {
            for n in &mut nodes {
                if n.owner.is_none() {
                    n.owner = Some(own.clone());
                }
            }
        }
        let mut shard = Shard {
            path: pf.path.clone(),
            lang: pf.lang.clone(),
            blob: pf.blob.clone(),
            owner,
            nodes,
            edges,
        };
        shard.canonicalize();
        shard
    }

    fn resolve_type_target(
        &self,
        from_path: &str,
        target: &str,
        imported_syms: &HashMap<String, (String, String)>,
        wildcard_files: &[String],
    ) -> Option<(String, Confidence)> {
        if let Some(ids) = self.lookup_in_file(from_path, target)
            && let Some(first) = ids.first()
        {
            return Some((first.clone(), Confidence::Exact));
        }
        if let Some((tfile, remote)) = imported_syms.get(target)
            && let Some(ids) = self.lookup_in_file(tfile, remote)
            && let Some(first) = ids.first()
        {
            return Some((first.clone(), Confidence::Import));
        }
        for wfile in wildcard_files {
            if let Some(ids) = self.lookup_in_file(wfile, target)
                && let Some(first) = ids.first()
            {
                return Some((first.clone(), Confidence::Import));
            }
        }
        if let Some(hits) = self.exported_by_name.get(target)
            && hits.len() == 1
        {
            return Some((hits[0].1.clone(), Confidence::Name));
        }
        None
    }

    fn resolve_call(
        &self,
        from_path: &str,
        lang: &str,
        call: &RawCall,
        imported_syms: &HashMap<String, (String, String)>,
        imported_mods: &HashMap<String, String>,
        wildcard_files: &[String],
    ) -> Vec<(String, String, Confidence)> {
        // 1. Self / this method call inside the enclosing class/type -> exact same-file resolution.
        if let Some(ref recv) = call.receiver
            && matches!(recv.as_str(), "self" | "cls" | "this" | "Self")
            && let Some(ref ty) = call.enclosing_type
        {
            let qual = format!("{ty}.{}", call.callee);
            if let Some(ids) = self.lookup_qualified_in_file(from_path, &qual) {
                return ids
                    .iter()
                    .map(|id| (id.clone(), from_path.to_string(), Confidence::Exact))
                    .collect();
            }
        }

        // 2. Qualified call with receiver (module alias, class/type name, or typed local var).
        if let Some(ref recv) = call.receiver {
            // 2a. Rust scoped path like `crate::foo::bar` or `super::bar`
            if lang == "rust" && (recv.starts_with("crate::") || recv.starts_with("super::") || recv.starts_with("self::") || recv == "crate" || recv == "super") {
                let fake = RawImport {
                    module: recv.clone(),
                    relative_dots: 0,
                    names: vec![(call.callee.clone(), call.callee.clone())],
                    module_alias: None,
                    wildcard: false,
                };
                if let Some(tfile) = self.resolve_import_path(from_path, lang, &fake)
                    && let Some(ids) = self.lookup_in_file(&tfile, &call.callee)
                {
                    return ids.iter().map(|id| (id.clone(), tfile.clone(), Confidence::Import)).collect();
                }
            }

            // 2b. Imported module alias: `mod_alias.func()`
            if let Some(tfile) = imported_mods.get(recv)
                && let Some(ids) = self.lookup_in_file(tfile, &call.callee)
            {
                return ids.iter().map(|id| (id.clone(), tfile.clone(), Confidence::Import)).collect();
            }

            // 2c. Type-qualified call: `Type.method()` / `Type::method()` or `var.method()` with known `receiver_type`.
            let type_hint = call.receiver_type.as_deref().or(Some(recv.as_str()));
            if let Some(ty) = type_hint {
                let qual = format!("{ty}.{}", call.callee);
                if let Some(ids) = self.lookup_qualified_in_file(from_path, &qual) {
                    return ids
                        .iter()
                        .map(|id| (id.clone(), from_path.to_string(), Confidence::Exact))
                        .collect();
                }
                if let Some((tfile, remote_ty)) = imported_syms.get(ty) {
                    let remote_qual = format!("{remote_ty}.{}", call.callee);
                    if let Some(ids) = self.lookup_qualified_in_file(tfile, &remote_qual) {
                        return ids
                            .iter()
                            .map(|id| (id.clone(), tfile.clone(), Confidence::Import))
                            .collect();
                    }
                }
                for wfile in wildcard_files {
                    if let Some(ids) = self.lookup_qualified_in_file(wfile, &qual) {
                        return ids
                            .iter()
                            .map(|id| (id.clone(), wfile.clone(), Confidence::Import))
                            .collect();
                    }
                }
                if let Some(hits) = self.exported_qualified.get(&qual)
                    && hits.len() <= 2
                {
                    return hits
                        .iter()
                        .map(|(p, id)| (id.clone(), p.clone(), Confidence::Name))
                        .collect();
                }
            }

            // 2d. Fallback by method name across repo only if non-builtin and unique.
            if !is_common_method(&call.callee)
                && let Some(hits) = self.exported_by_name.get(&call.callee)
                && hits.len() == 1
            {
                return vec![(hits[0].1.clone(), hits[0].0.clone(), Confidence::Name)];
            }
            return Vec::new();
        }

        // 3. Direct call `foo(...)`:
        // 3a. Same-file definition -> Exact
        if let Some(ids) = self.lookup_in_file(from_path, &call.callee) {
            return ids
                .iter()
                .map(|id| (id.clone(), from_path.to_string(), Confidence::Exact))
                .collect();
        }
        // 3b. Import-table binding -> Import
        if let Some((tfile, remote)) = imported_syms.get(&call.callee)
            && let Some(ids) = self.lookup_in_file(tfile, remote)
        {
            return ids
                .iter()
                .map(|id| (id.clone(), tfile.clone(), Confidence::Import))
                .collect();
        }
        // 3c. Wildcard imports -> Import
        for wfile in wildcard_files {
            if let Some(ids) = self.lookup_in_file(wfile, &call.callee) {
                return ids
                    .iter()
                    .map(|id| (id.clone(), wfile.clone(), Confidence::Import))
                    .collect();
            }
        }
        // 3d. Repo exported-name fallback -> Name (when small/unambiguous)
        if !is_builtin(&call.callee)
            && let Some(hits) = self.exported_by_name.get(&call.callee)
            && hits.len() <= 2
        {
            return hits
                .iter()
                .map(|(p, id)| (id.clone(), p.clone(), Confidence::Name))
                .collect();
        }
        Vec::new()
    }

    fn lookup_in_file(&self, path: &str, name: &str) -> Option<&Vec<String>> {
        self.file_top.get(path)?.get(name)
    }

    fn lookup_qualified_in_file(&self, path: &str, qual: &str) -> Option<&Vec<String>> {
        self.file_qualified.get(path)?.get(qual)
    }

    /// Resolve a `RawImport` to an existing repo file path, if it points inside this repo.
    pub fn resolve_import_path(&self, from_path: &str, lang: &str, imp: &RawImport) -> Option<String> {
        let keys: BTreeSet<&str> = self.files.keys().map(String::as_str).collect();
        resolve_import_against_set(from_path, lang, imp, &keys, &self.rust_roots)
    }
}

/// Resolve an import specifier to a repo-relative file path against a set of known files.
pub fn resolve_import_against_set(
    from_path: &str,
    lang: &str,
    imp: &RawImport,
    known_files: &BTreeSet<&str>,
    rust_roots: &BTreeSet<String>,
) -> Option<String> {
    match lang {
        "python" => resolve_python_import(from_path, imp, known_files),
        "typescript" | "tsx" | "javascript" | "jsx" => resolve_js_import(from_path, &imp.module, known_files),
        "rust" => resolve_rust_import(from_path, &imp.module, known_files, rust_roots),
        _ => None,
    }
}

/// Predict the repo-relative target path of an import even if the file does not exist on disk yet.
/// Used by `seed_from_plan` to create `planned` nodes from contract test imports.
pub fn predict_import_target_path(from_path: &str, lang: &str, imp: &RawImport) -> Option<String> {
    match lang {
        "python" => {
            if imp.relative_dots > 0 {
                let base_dir = climb_python_dir(from_path, imp.relative_dots)?;
                if imp.module.is_empty() {
                    return None;
                }
                let rel = imp.module.replace('.', "/");
                let path = if base_dir.is_empty() {
                    format!("{rel}.py")
                } else {
                    format!("{base_dir}/{rel}.py")
                };
                Some(path)
            } else {
                let first = imp.module.split('.').next().unwrap_or("");
                if first.is_empty() || is_python_stdlib(first) {
                    return None;
                }
                Some(format!("{}.py", imp.module.replace('.', "/")))
            }
        }
        "typescript" | "tsx" | "javascript" | "jsx" => {
            let spec = imp.module.as_str();
            let base = if let Some(rest) = spec.strip_prefix("@/") {
                rest.to_string()
            } else if spec.starts_with("./") || spec.starts_with("../") {
                let dir = from_path.rsplit_once('/').map_or("", |(d, _)| d);
                normalize_segments(&format!("{dir}/{spec}"))?
            } else {
                return None;
            };
            if base.ends_with(".ts") || base.ends_with(".tsx") || base.ends_with(".js") || base.ends_with(".jsx") {
                Some(base)
            } else {
                let ext = if matches!(lang, "typescript" | "tsx") { "ts" } else { "js" };
                Some(format!("{base}.{ext}"))
            }
        }
        "rust" => {
            let module = imp.module.as_str();
            let rest = module.strip_prefix("crate::")?;
            let root = rust_crate_root_of(from_path).unwrap_or_default();
            let rel = rest.replace("::", "/");
            if root.is_empty() {
                Some(format!("src/{rel}.rs"))
            } else {
                Some(format!("{root}/src/{rel}.rs"))
            }
        }
        _ => None,
    }
}

fn resolve_python_import(from_path: &str, imp: &RawImport, known: &BTreeSet<&str>) -> Option<String> {
    let base_path = if imp.relative_dots > 0 {
        let dir = climb_python_dir(from_path, imp.relative_dots)?;
        if imp.module.is_empty() {
            dir
        } else if dir.is_empty() {
            imp.module.replace('.', "/")
        } else {
            format!("{dir}/{}", imp.module.replace('.', "/"))
        }
    } else {
        if imp.module.is_empty() {
            return None;
        }
        imp.module.replace('.', "/")
    };

    let candidates = [
        format!("{base_path}.py"),
        format!("{base_path}/__init__.py"),
        format!("src/{base_path}.py"),
        format!("src/{base_path}/__init__.py"),
    ];
    for c in candidates {
        if known.contains(c.as_str()) {
            return Some(c);
        }
    }
    // `import pkg.mod.Symbol` -> try parent module `pkg/mod.py`.
    if let Some((parent, _)) = base_path.rsplit_once('/') {
        for c in [format!("{parent}.py"), format!("{parent}/__init__.py")] {
            if known.contains(c.as_str()) {
                return Some(c);
            }
        }
    }
    None
}

fn climb_python_dir(from_path: &str, dots: usize) -> Option<String> {
    let mut parts: Vec<&str> = from_path.split('/').collect();
    parts.pop(); // drop filename
    let climbs = dots.saturating_sub(1);
    for _ in 0..climbs {
        parts.pop()?;
    }
    Some(parts.join("/"))
}

fn resolve_js_import(from_path: &str, spec: &str, known: &BTreeSet<&str>) -> Option<String> {
    const EXTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let bases: Vec<String> = if let Some(rest) = spec.strip_prefix("@/") {
        vec![rest.to_string(), format!("src/{rest}")]
    } else if spec.starts_with("./") || spec.starts_with("../") {
        let dir = from_path.rsplit_once('/').map_or("", |(d, _)| d);
        normalize_segments(&format!("{dir}/{spec}")).into_iter().collect()
    } else {
        return None;
    };
    for base in bases {
        let trimmed = base
            .strip_suffix(".js")
            .or_else(|| base.strip_suffix(".jsx"))
            .or_else(|| base.strip_suffix(".mjs"))
            .unwrap_or(&base);
        if known.contains(base.as_str()) {
            return Some(base);
        }
        for ext in EXTS {
            let c1 = format!("{trimmed}.{ext}");
            if known.contains(c1.as_str()) {
                return Some(c1);
            }
        }
        for ext in EXTS {
            let c2 = format!("{trimmed}/index.{ext}");
            if known.contains(c2.as_str()) {
                return Some(c2);
            }
        }
    }
    None
}

fn resolve_rust_import(
    from_path: &str,
    module: &str,
    known: &BTreeSet<&str>,
    _rust_roots: &BTreeSet<String>,
) -> Option<String> {
    let crate_root = rust_crate_root_of(from_path).unwrap_or_default();
    let src_root = if crate_root.is_empty() {
        "src".to_string()
    } else {
        format!("{crate_root}/src")
    };
    let from_dir = from_path.rsplit_once('/').map_or("", |(d, _)| d);
    let from_stem = from_path
        .rsplit('/')
        .next()
        .unwrap_or(from_path)
        .strip_suffix(".rs")
        .unwrap_or("");

    let rel_parts: Vec<&str> = if let Some(rest) = module.strip_prefix("crate::") {
        rest.split("::").collect()
    } else if module == "crate" {
        vec![]
    } else if let Some(rest) = module.strip_prefix("self::") {
        let mut base = if matches!(from_stem, "lib" | "main" | "mod") {
            from_dir.to_string()
        } else if from_dir.is_empty() {
            from_stem.to_string()
        } else {
            format!("{from_dir}/{from_stem}")
        };
        let sub = rest.replace("::", "/");
        if !base.is_empty() && !sub.is_empty() {
            base.push('/');
        }
        base.push_str(&sub);
        return probe_rust_candidates(&base, known);
    } else {
        let rest = module.strip_prefix("super::")?;
        let parent_dir = if matches!(from_stem, "lib" | "main" | "mod") {
            from_dir.rsplit_once('/').map_or("", |(p, _)| p)
        } else {
            from_dir
        };
        let sub = rest.replace("::", "/");
        let base = if parent_dir.is_empty() { sub } else { format!("{parent_dir}/{sub}") };
        return probe_rust_candidates(&base, known);
    };

    if rel_parts.is_empty() {
        for root_file in [format!("{src_root}/lib.rs"), format!("{src_root}/main.rs")] {
            if known.contains(root_file.as_str()) {
                return Some(root_file);
            }
        }
        return None;
    }

    // Try progressively shorter prefixes so `crate::foo::Bar` resolves to `src/foo.rs`.
    for len in (1..=rel_parts.len()).rev() {
        let joined = format!("{src_root}/{}", rel_parts[..len].join("/"));
        if let Some(found) = probe_rust_candidates(&joined, known) {
            return Some(found);
        }
    }
    None
}

fn probe_rust_candidates(base: &str, known: &BTreeSet<&str>) -> Option<String> {
    [format!("{base}.rs"), format!("{base}/mod.rs")]
        .into_iter()
        .find(|c| known.contains(c.as_str()))
}

fn rust_crate_root_of(path: &str) -> Option<String> {
    if let Some((prefix, _)) = path.split_once("/src/") {
        Some(prefix.to_string())
    } else if path.starts_with("src/") {
        Some(String::new())
    } else {
        None
    }
}

fn normalize_segments(path: &str) -> Option<String> {
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

fn qualified_of_id(id: &str) -> Option<&str> {
    let (_, rest) = id.split_once('#')?;
    Some(rest.split('~').next().unwrap_or(rest))
}

fn is_python_stdlib(top: &str) -> bool {
    matches!(
        top,
        "__future__"
            | "abc"
            | "argparse"
            | "ast"
            | "asyncio"
            | "base64"
            | "bisect"
            | "builtins"
            | "collections"
            | "contextlib"
            | "copy"
            | "csv"
            | "dataclasses"
            | "datetime"
            | "decimal"
            | "difflib"
            | "enum"
            | "errno"
            | "fnmatch"
            | "functools"
            | "glob"
            | "gzip"
            | "hashlib"
            | "heapq"
            | "hmac"
            | "html"
            | "http"
            | "importlib"
            | "inspect"
            | "io"
            | "itertools"
            | "json"
            | "logging"
            | "math"
            | "operator"
            | "os"
            | "pathlib"
            | "pickle"
            | "platform"
            | "pprint"
            | "pytest"
            | "queue"
            | "random"
            | "re"
            | "secrets"
            | "shlex"
            | "shutil"
            | "signal"
            | "socket"
            | "sqlite3"
            | "statistics"
            | "string"
            | "struct"
            | "subprocess"
            | "sys"
            | "tempfile"
            | "textwrap"
            | "threading"
            | "time"
            | "traceback"
            | "types"
            | "typing"
            | "unittest"
            | "urllib"
            | "uuid"
            | "warnings"
            | "weakref"
            | "zipfile"
    )
}

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "abs"
            | "all"
            | "any"
            | "bool"
            | "bytes"
            | "callable"
            | "chr"
            | "dict"
            | "dir"
            | "divmod"
            | "enumerate"
            | "eval"
            | "exec"
            | "filter"
            | "float"
            | "format"
            | "frozenset"
            | "getattr"
            | "globals"
            | "hasattr"
            | "hash"
            | "hex"
            | "id"
            | "input"
            | "int"
            | "isinstance"
            | "issubclass"
            | "iter"
            | "len"
            | "list"
            | "locals"
            | "map"
            | "max"
            | "min"
            | "next"
            | "object"
            | "open"
            | "ord"
            | "pow"
            | "print"
            | "property"
            | "range"
            | "repr"
            | "reversed"
            | "round"
            | "set"
            | "setattr"
            | "slice"
            | "sorted"
            | "staticmethod"
            | "str"
            | "sum"
            | "super"
            | "tuple"
            | "type"
            | "vars"
            | "zip"
            | "Exception"
            | "ValueError"
            | "TypeError"
            | "KeyError"
            | "RuntimeError"
            | "IndexError"
            | "NotImplementedError"
            | "AssertionError"
            | "Error"
            | "String"
            | "Number"
            | "Boolean"
            | "Array"
            | "Object"
            | "Map"
            | "Set"
            | "Promise"
            | "Date"
            | "RegExp"
            | "JSON"
            | "Math"
            | "console"
            | "parseInt"
            | "parseFloat"
            | "isNaN"
            | "require"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            | "Box"
            | "Vec"
            | "Arc"
            | "Rc"
    )
}

fn is_common_method(name: &str) -> bool {
    matches!(
        name,
        "get"
            | "set"
            | "add"
            | "push"
            | "pop"
            | "append"
            | "extend"
            | "insert"
            | "remove"
            | "delete"
            | "clear"
            | "keys"
            | "values"
            | "items"
            | "update"
            | "copy"
            | "clone"
            | "to_string"
            | "toString"
            | "into"
            | "from"
            | "default"
            | "new"
            | "len"
            | "is_empty"
            | "iter"
            | "into_iter"
            | "collect"
            | "map"
            | "filter"
            | "find"
            | "any"
            | "all"
            | "unwrap"
            | "expect"
            | "ok"
            | "err"
            | "is_some"
            | "is_none"
            | "is_ok"
            | "is_err"
            | "join"
            | "split"
            | "strip"
            | "trim"
            | "lower"
            | "upper"
            | "startswith"
            | "endswith"
            | "starts_with"
            | "ends_with"
            | "replace"
            | "format"
            | "read"
            | "write"
            | "close"
            | "sort"
            | "contains"
            | "includes"
            | "forEach"
            | "slice"
            | "log"
            | "warn"
            | "error"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_file;

    #[test]
    fn python_defs_methods_imports_inheritance_calls_and_duplicates() {
        let contract = parse_file(
            "tinydb/contract.py",
            r#""""Shared interfaces."""
class StorageBase:
    """Base storage."""
    def flush(self) -> None:
        pass
"#,
        )
        .unwrap();

        let btree = parse_file(
            "tinydb/btree.py",
            r#"class BTree:
    def insert_key(self, k: int) -> None:
        pass
"#,
        )
        .unwrap();

        let executor = parse_file(
            "tinydb/executor.py",
            r#""""Query executor."""
from tinydb.contract import StorageBase
from tinydb.btree import BTree

def helper(x: int) -> int:
    return x + 1

def dup() -> int:
    return 1

def dup() -> int:
    return 2

class Executor(StorageBase):
    """Runs queries."""
    def __init__(self, tree: BTree) -> None:
        self.tree = tree

    def execute(self, key: int) -> int:
        self._validate(key)
        self.tree.insert_key(key)
        return helper(key)

    def _validate(self, key: int) -> None:
        pass
"#,
        )
        .unwrap();

        let parsed = vec![contract, btree, executor];
        let cat = SymbolCatalog::build(&BTreeMap::new(), &parsed);
        let exec_shard = cat.resolve_shard(&parsed[2], Some("executor".into()));

        // Stable IDs including duplicate `~2`
        assert!(exec_shard.nodes.iter().any(|n| n.id == "py:tinydb/executor.py#dup"));
        assert!(exec_shard.nodes.iter().any(|n| n.id == "py:tinydb/executor.py#dup~2"));

        // Method parent + deterministic docstring
        let exec_cls = exec_shard.nodes.iter().find(|n| n.name == "Executor").unwrap();
        assert_eq!(exec_cls.kind, NodeKind::Class);
        assert_eq!(exec_cls.doc.as_deref(), Some("Runs queries."));
        let execute_fn = exec_shard.nodes.iter().find(|n| n.name == "execute").unwrap();
        assert_eq!(execute_fn.kind, NodeKind::Method);
        assert_eq!(execute_fn.parent.as_deref(), Some("py:tinydb/executor.py#Executor"));

        // Imports
        assert!(exec_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Imports
                && e.from == "py:tinydb/executor.py"
                && e.to == "py:tinydb/btree.py"
        }));

        // Inheritance
        assert!(exec_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Inherits
                && e.from == "py:tinydb/executor.py#Executor"
                && e.to == "py:tinydb/contract.py#StorageBase"
                && e.conf == Confidence::Import
        }));

        // Calls with confidence: same-file Exact, imported Import
        assert!(exec_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "py:tinydb/executor.py#Executor.execute"
                && e.to == "py:tinydb/executor.py#Executor._validate"
                && e.conf == Confidence::Exact
        }));
        assert!(exec_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "py:tinydb/executor.py#Executor.execute"
                && e.to == "py:tinydb/executor.py#helper"
                && e.conf == Confidence::Exact
        }));
        assert!(exec_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "py:tinydb/executor.py#Executor.execute"
                && e.to == "py:tinydb/btree.py#BTree.insert_key"
                && e.conf == Confidence::Import
        }));
    }

    #[test]
    fn typescript_tsx_and_javascript_parsing_and_resolution() {
        let iface = parse_file(
            "src/types.ts",
            r#"/** Serializable contract. */
export interface Serializable {
  serialize(): string;
}
export class BaseWidget {
  mount(): void {}
}
"#,
        )
        .unwrap();

        let helper = parse_file(
            "src/util.js",
            r#"export function computeTotal(n) {
  return n * 2;
}
"#,
        )
        .unwrap();

        let page = parse_file(
            "src/Card.tsx",
            r#"import { BaseWidget, Serializable } from "./types";
import { computeTotal } from "./util";

/** Renders a card widget. */
export class CardWidget extends BaseWidget implements Serializable {
  serialize(): string {
    return String(computeTotal(4));
  }
  render(): JSX.Element {
    return <div>{this.serialize()}</div>;
  }
}
"#,
        )
        .unwrap();

        let parsed = vec![iface, helper, page];
        let cat = SymbolCatalog::build(&BTreeMap::new(), &parsed);
        let card_shard = cat.resolve_shard(&parsed[2], Some("card".into()));

        let cls = card_shard.nodes.iter().find(|n| n.name == "CardWidget").unwrap();
        assert_eq!(cls.kind, NodeKind::Class);
        assert_eq!(cls.doc.as_deref(), Some("Renders a card widget."));
        let ser = card_shard.nodes.iter().find(|n| n.name == "serialize").unwrap();
        assert_eq!(ser.kind, NodeKind::Method);
        assert_eq!(ser.parent.as_deref(), Some("ts:src/Card.tsx#CardWidget"));

        assert!(card_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Inherits
                && e.from == "ts:src/Card.tsx#CardWidget"
                && e.to == "ts:src/types.ts#BaseWidget"
                && e.conf == Confidence::Import
        }));
        assert!(card_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Implements
                && e.from == "ts:src/Card.tsx#CardWidget"
                && e.to == "ts:src/types.ts#Serializable"
                && e.conf == Confidence::Import
        }));
        assert!(card_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "ts:src/Card.tsx#CardWidget.serialize"
                && e.to == "js:src/util.js#computeTotal"
                && e.conf == Confidence::Import
        }));
        assert!(card_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "ts:src/Card.tsx#CardWidget.render"
                && e.to == "ts:src/Card.tsx#CardWidget.serialize"
                && e.conf == Confidence::Exact
        }));
    }

    #[test]
    fn rust_structs_traits_impls_imports_and_calls() {
        let storage = parse_file(
            "src/storage.rs",
            r#"/// Persistent engine trait.
pub trait Engine {
    fn flush_all(&self);
}

pub fn open_disk() -> usize {
    42
}
"#,
        )
        .unwrap();

        let store = parse_file(
            "src/store.rs",
            r#"use crate::storage::{Engine, open_disk};

/// In-memory store.
pub struct MemStore {
    id: usize,
}

impl MemStore {
    pub fn build_store() -> Self {
        Self { id: open_disk() }
    }

    fn internal_step(&self) -> usize {
        self.id
    }
}

impl Engine for MemStore {
    fn flush_all(&self) {
        let _ = self.internal_step();
    }
}
"#,
        )
        .unwrap();

        let parsed = vec![storage, store];
        let cat = SymbolCatalog::build(&BTreeMap::new(), &parsed);
        let store_shard = cat.resolve_shard(&parsed[1], Some("store".into()));

        let mem = store_shard.nodes.iter().find(|n| n.name == "MemStore").unwrap();
        assert_eq!(mem.kind, NodeKind::Type);
        assert_eq!(mem.doc.as_deref(), Some("In-memory store."));

        let flush = store_shard.nodes.iter().find(|n| n.name == "flush_all").unwrap();
        assert_eq!(flush.kind, NodeKind::Method);
        assert_eq!(flush.parent.as_deref(), Some("rs:src/store.rs#MemStore"));

        assert!(store_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Implements
                && e.from == "rs:src/store.rs#MemStore"
                && e.to == "rs:src/storage.rs#Engine"
                && e.conf == Confidence::Import
        }));
        assert!(store_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "rs:src/store.rs#MemStore.build_store"
                && e.to == "rs:src/storage.rs#open_disk"
                && e.conf == Confidence::Import
        }));
        assert!(store_shard.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "rs:src/store.rs#MemStore.flush_all"
                && e.to == "rs:src/store.rs#MemStore.internal_step"
                && e.conf == Confidence::Exact
        }));
    }

    #[test]
    fn name_confidence_fallback_and_fallback_language() {
        let lib_py = parse_file("pkg/alpha.py", "def unique_special_calc(x):\n    return x\n").unwrap();
        let caller_py = parse_file("pkg/beta.py", "def run():\n    return unique_special_calc(5)\n").unwrap();
        let go_file = parse_file("cmd/main.go", "package main\nfunc ServeHttp() {}\ntype Config struct{}\n").unwrap();

        let parsed = vec![lib_py, caller_py, go_file];
        let cat = SymbolCatalog::build(&BTreeMap::new(), &parsed);
        let beta = cat.resolve_shard(&parsed[1], None);
        assert!(beta.edges.iter().any(|e| {
            e.kind == EdgeKind::Calls
                && e.from == "py:pkg/beta.py#run"
                && e.to == "py:pkg/alpha.py#unique_special_calc"
                && e.conf == Confidence::Name
        }));

        let go_shard = cat.resolve_shard(&parsed[2], None);
        assert!(go_shard.nodes.iter().any(|n| n.name == "ServeHttp" && n.kind == NodeKind::Function));
        assert!(go_shard.nodes.iter().any(|n| n.name == "Config" && n.kind == NodeKind::Type));
        assert!(go_shard.edges.is_empty(), "fallback extractor emits no call edges");
    }
}
