//! Incremental graph building, single-file updates, planner seeding, and freshness verification.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

use globset::{Glob, GlobMatcher};
use tm_core::Plan;

use crate::model::{
    BUILDER_VERSION, Confidence, Edge, EdgeKind, Meta, Node, NodeKind, NodeStatus, PlanSeed,
    SCHEMA_VERSION, Shard, UnitSeed, file_node_id, git_blob_sha, is_flow_path, normalize_rel_path,
    sig_hash, symbol_node_id, unit_node_id,
};
use crate::parse::{ParsedFile, detect_language, parse_file};
use crate::resolve::{SymbolCatalog, predict_import_target_path, resolve_import_against_set};
use crate::store::{
    FlowDb, ensure_flow_dir, read_all_shards, read_meta, read_plan_seed, read_shard, remove_shard,
    write_meta, write_plan_seed, write_shard,
};

/// Maximum source file size indexed (skip huge generated/minified blobs).
const MAX_SOURCE_BYTES: u64 = 512 * 1024;

const IGNORED_DIRS: [&str; 16] = [
    ".git",
    ".flow",
    ".taskmaster",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    "vendor",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildReport {
    pub parsed: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: usize,
    pub total_files: usize,
    pub total_nodes: usize,
    pub total_edges: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateReport {
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub flipped_from_planned: Vec<String>,
    pub flipped_files: Vec<String>,
    pub moves: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub contracts: usize,
    pub units: usize,
    pub planned_nodes: usize,
    pub planned_files: usize,
}

/// Discover every tracked/non-ignored source file under `tree`.
pub fn discover_source_files(tree: &Path) -> Vec<String> {
    let raw = git_ls_files(tree).unwrap_or_else(|| walk_source_files(tree));
    let mut out: Vec<String> = raw
        .into_iter()
        .map(|p| normalize_rel_path(&p))
        .filter(|p| is_indexable_source(tree, p))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn is_indexable_source(tree: &Path, rel: &str) -> bool {
    if rel.is_empty() || is_flow_path(rel) || has_ignored_dir(rel) {
        return false;
    }
    if detect_language(rel).is_none() {
        return false;
    }
    let full = tree.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&full) else {
        return false;
    };
    meta.is_file() && meta.len() <= MAX_SOURCE_BYTES
}

fn has_ignored_dir(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    dirs.iter().any(|d| IGNORED_DIRS.contains(d))
}

fn git_ls_files(tree: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn walk_source_files(tree: &Path) -> Vec<String> {
    fn rec(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if kind.is_dir() {
                if !IGNORED_DIRS.contains(&name.as_str()) {
                    rec(root, &path, out);
                }
            } else if kind.is_file()
                && let Ok(rel) = path.strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    rec(tree, tree, &mut out);
    out
}

fn git_head_and_tree(tree: &Path) -> (String, String) {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(tree)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let head = run(&["rev-parse", "--verify", "--quiet", "HEAD"]);
    let tree_sha = if head.is_empty() {
        String::new()
    } else {
        run(&["rev-parse", "--verify", "--quiet", "HEAD^{tree}"])
    };
    (head, tree_sha)
}

/// Candidate paths changed since `old_head` (committed + dirty/untracked).
fn git_changed_since(tree: &Path, old_head: &str) -> Option<BTreeSet<String>> {
    if old_head.is_empty() {
        return None;
    }
    let check = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["cat-file", "-e", &format!("{old_head}^{{commit}}")])
        .output()
        .ok()?;
    if !check.status.success() {
        return None;
    }
    let mut changed = BTreeSet::new();
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["diff", "--name-only", "-z", old_head, "HEAD"])
        .output()
        && out.status.success()
    {
        for p in String::from_utf8_lossy(&out.stdout).split('\0').filter(|s| !s.is_empty()) {
            let norm = normalize_rel_path(p);
            if !is_flow_path(&norm) {
                changed.insert(norm);
            }
        }
    }
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["status", "--porcelain", "-z", "--untracked-files=all"])
        .output()
        && out.status.success()
    {
        for entry in String::from_utf8_lossy(&out.stdout).split('\0') {
            if entry.len() > 3 {
                let norm = normalize_rel_path(&entry[3..]);
                if !is_flow_path(&norm) {
                    changed.insert(norm);
                }
            }
        }
    }
    Some(changed)
}

struct OwnerMatchers {
    units: Vec<(String, GlobMatcher)>,
    contracts: BTreeSet<String>,
}

impl OwnerMatchers {
    fn from_seed(seed: Option<&PlanSeed>) -> Self {
        let mut units = Vec::new();
        let mut contracts = BTreeSet::new();
        if let Some(seed) = seed {
            for u in &seed.units {
                if let Ok(g) = Glob::new(&u.owned_glob) {
                    units.push((u.id.clone(), g.compile_matcher()));
                }
            }
            for c in &seed.contracts {
                contracts.insert(normalize_rel_path(c));
            }
        }
        Self { units, contracts }
    }

    fn owner_of(&self, path: &str) -> Option<String> {
        self.units
            .iter()
            .find(|(_, m)| m.is_match(path))
            .map(|(id, _)| id.clone())
    }

    fn is_contract(&self, path: &str) -> bool {
        self.contracts.contains(path)
    }
}

/// Incrementally build or refresh `.flow/` against `meta.json`'s `head` + dirty files
/// (and blob hashes of tracked source files). Rebuilds `.flow/index.sqlite`.
pub fn build(tree: &Path) -> anyhow::Result<BuildReport> {
    ensure_flow_dir(tree)?;
    let existing_meta = read_meta(tree)?;
    let mut plan_seed = read_plan_seed(tree)?;
    let matchers = OwnerMatchers::from_seed(plan_seed.as_ref());

    let full_rebuild = match &existing_meta {
        Some(m) => m.schema_version != SCHEMA_VERSION || m.builder_version != BUILDER_VERSION,
        None => true,
    };

    let mut shards = if full_rebuild {
        BTreeMap::new()
    } else {
        read_all_shards(tree)?
    };
    let tracked = discover_source_files(tree);
    let tracked_set: BTreeSet<String> = tracked.iter().cloned().collect();

    // Remove shards for deleted source files.
    let mut removed = Vec::new();
    let stale_shard_paths: Vec<String> = shards
        .keys()
        .filter(|p| !tracked_set.contains(*p))
        .cloned()
        .collect();
    let mut removed_nodes_by_body: HashMap<String, Vec<String>> = HashMap::new();
    for path in stale_shard_paths {
        if let Some(old) = shards.remove(&path) {
            record_body_hashes(&old.nodes, &mut removed_nodes_by_body);
        }
        remove_shard(tree, &path)?;
        removed.push(path);
    }

    // Decide which tracked files need parsing (incremental via meta.head + dirty + blob check).
    let git_candidates = if full_rebuild {
        None
    } else {
        existing_meta
            .as_ref()
            .and_then(|m| git_changed_since(tree, &m.head))
    };

    let mut to_parse: Vec<String> = Vec::new();
    let mut unchanged = 0usize;
    for path in &tracked {
        let needs_check = full_rebuild
            || !shards.contains_key(path)
            || git_candidates.as_ref().is_none_or(|c| c.contains(path));
        if !needs_check {
            // Still verify on-disk blob matches stored shard blob (cheap single file read).
            if let Some(existing) = shards.get(path)
                && let Ok(bytes) = std::fs::read(tree.join(path))
                && git_blob_sha(&bytes) == existing.blob
            {
                unchanged += 1;
                continue;
            }
        } else if !full_rebuild
            && let Some(existing) = shards.get(path)
            && let Ok(bytes) = std::fs::read(tree.join(path))
            && git_blob_sha(&bytes) == existing.blob
        {
            unchanged += 1;
            continue;
        }
        to_parse.push(path.clone());
    }

    // Parse only files that changed or have no shard.
    let mut parsed_files: Vec<ParsedFile> = Vec::new();
    for path in &to_parse {
        if let Some(old) = shards.get(path) {
            record_body_hashes(&old.nodes, &mut removed_nodes_by_body);
        }
        let Ok(source) = std::fs::read_to_string(tree.join(path)) else { continue };
        if let Some(pf) = parse_file(path, &source) {
            parsed_files.push(pf);
        }
    }

    let mut moves = existing_meta
        .as_ref()
        .map(|m| m.moves.clone())
        .unwrap_or_default();

    let mut parsed_paths = Vec::new();
    if !parsed_files.is_empty() {
        let catalog = SymbolCatalog::build(&shards, &parsed_files);
        for pf in &parsed_files {
            let owner = matchers.owner_of(&pf.path);
            let mut shard = catalog.resolve_shard(pf, owner);
            if matchers.is_contract(&shard.path) {
                mark_contract_nodes(&mut shard);
            }
            detect_moves(&shard.nodes, &removed_nodes_by_body, &mut moves);
            write_shard(tree, &shard)?;
            parsed_paths.push(shard.path.clone());
            shards.insert(shard.path.clone(), shard);
        }
    }

    // Reconcile planned nodes & unit owns edges if a plan seed exists.
    let mut plan_changed = false;
    if let Some(seed) = plan_seed.as_mut()
        && reconcile_plan_seed(seed, &shards, &matchers)
    {
        write_plan_seed(tree, seed)?;
        plan_changed = true;
    }

    // Avoid rewriting meta.json on a clean no-op rebuild (e.g. right after committing `.flow/`).
    if full_rebuild
        || existing_meta.is_none()
        || !parsed_paths.is_empty()
        || !removed.is_empty()
        || plan_changed
    {
        let meta = build_meta(tree, &shards, moves);
        write_meta(tree, &meta)?;
    }
    let db = FlowDb::open_or_rebuild(tree)?;

    Ok(BuildReport {
        parsed: parsed_paths,
        removed,
        unchanged,
        total_files: shards.len(),
        total_nodes: db.node_count()?,
        total_edges: db.edge_count()?,
    })
}

/// Re-parse ONLY `paths` (rewriting only those shards and removing deleted shards).
/// Untouched shards on disk are never rewritten.
pub fn update_files(tree: &Path, paths: &[String]) -> anyhow::Result<UpdateReport> {
    ensure_flow_dir(tree)?;
    let mut existing_meta = read_meta(tree)?.unwrap_or_default();
    let mut plan_seed = read_plan_seed(tree)?;
    let matchers = OwnerMatchers::from_seed(plan_seed.as_ref());
    let mut shards = read_all_shards(tree)?;

    let mut unique: Vec<String> = paths
        .iter()
        .map(|p| normalize_rel_path(p))
        .filter(|p| !p.is_empty() && !is_flow_path(p) && !has_ignored_dir(p))
        .collect();
    unique.sort();
    unique.dedup();

    let mut removed = Vec::new();
    let mut parsed_files = Vec::new();
    let mut removed_nodes_by_body: HashMap<String, Vec<String>> = HashMap::new();

    for path in &unique {
        let full = tree.join(path);
        if !full.is_file() {
            if let Some(old) = shards.remove(path) {
                record_body_hashes(&old.nodes, &mut removed_nodes_by_body);
            }
            if remove_shard(tree, path)? {
                removed.push(path.clone());
            }
            continue;
        }
        if detect_language(path).is_none() {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&full) else { continue };
        if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
            continue;
        }
        if let Some(old) = shards.get(path) {
            record_body_hashes(&old.nodes, &mut removed_nodes_by_body);
        }
        let Ok(source) = std::fs::read_to_string(&full) else { continue };
        if let Some(pf) = parse_file(path, &source) {
            parsed_files.push(pf);
        }
    }

    let mut updated = Vec::new();
    let mut new_moves = BTreeMap::new();
    if !parsed_files.is_empty() {
        let catalog = SymbolCatalog::build(&shards, &parsed_files);
        for pf in &parsed_files {
            let owner = matchers.owner_of(&pf.path);
            let mut shard = catalog.resolve_shard(pf, owner);
            if matchers.is_contract(&shard.path) {
                mark_contract_nodes(&mut shard);
            }
            detect_moves(&shard.nodes, &removed_nodes_by_body, &mut new_moves);
            write_shard(tree, &shard)?;
            updated.push(shard.path.clone());
            shards.insert(shard.path.clone(), shard);
        }
    }

    for (k, v) in &new_moves {
        existing_meta.moves.insert(k.clone(), v.clone());
    }

    let mut flipped = Vec::new();
    let mut flipped_files = Vec::new();
    if let Some(seed) = plan_seed.as_mut() {
        let before_planned_syms: BTreeSet<String> = seed
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Planned && n.kind != NodeKind::File)
            .map(|n| n.id.clone())
            .collect();
        let before_planned_files: BTreeSet<String> = seed
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Planned && n.kind == NodeKind::File)
            .map(|n| n.id.clone())
            .collect();
        let changed = reconcile_plan_seed(seed, &shards, &matchers);
        let after_planned: BTreeSet<String> = seed
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Planned)
            .map(|n| n.id.clone())
            .collect();
        for id in before_planned_syms.difference(&after_planned) {
            flipped.push(id.clone());
        }
        for id in before_planned_files.difference(&after_planned) {
            flipped_files.push(id.clone());
        }
        if changed {
            write_plan_seed(tree, seed)?;
        }
    }

    let meta = build_meta(tree, &shards, existing_meta.moves);
    write_meta(tree, &meta)?;
    let _ = FlowDb::rebuild(tree)?;

    Ok(UpdateReport {
        updated,
        removed,
        flipped_from_planned: flipped,
        flipped_files,
        moves: new_moves,
    })
}

/// Seed `.flow/` from a locked `tm_core::Plan`:
/// 1. Parse contract files with `status: "contract"` and lock-time `sig_hash`.
/// 2. Create synthetic `unit` nodes with `owns` edges for files matching each unit's `owned_glob`.
/// 3. Create `planned` nodes for symbols imported by locked contract tests (flipping to `built`
///    with the exact same stable ID once code lands), plus `tests` and `contract_of` edges.
pub fn seed_from_plan(tree: &Path, plan: &Plan) -> anyhow::Result<SeedReport> {
    ensure_flow_dir(tree)?;
    let units: Vec<UnitSeed> = plan
        .pages
        .iter()
        .map(|p| UnitSeed {
            id: p.id.clone(),
            owned_glob: p.owned_glob.clone(),
            depends_on: p.depends_on.clone(),
            is_ui: plan.is_ui(p),
            route: p.route.clone(),
        })
        .collect();
    let contracts: Vec<String> = plan
        .contracts
        .iter()
        .map(|c| normalize_rel_path(&c.path))
        .collect();
    let contract_tests: Vec<String> = plan
        .contracts
        .iter()
        .filter(|c| c.is_test())
        .map(|c| normalize_rel_path(&c.path))
        .collect();

    let mut seed = PlanSeed {
        units: units.clone(),
        contracts: contracts.clone(),
        contract_tests: contract_tests.clone(),
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    let matchers = OwnerMatchers::from_seed(Some(&seed));

    // Read existing shards and parse every contract file that exists on disk.
    let mut shards = read_all_shards(tree)?;
    let mut parsed_contracts = Vec::new();
    for cpath in &contracts {
        let full = tree.join(cpath);
        if !full.is_file() {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&full) else { continue };
        if let Some(pf) = parse_file(cpath, &source) {
            parsed_contracts.push(pf);
        }
    }

    let catalog = SymbolCatalog::build(&shards, &parsed_contracts);
    for pf in &parsed_contracts {
        let mut shard = catalog.resolve_shard(pf, None);
        mark_contract_nodes(&mut shard);
        write_shard(tree, &shard)?;
        shards.insert(shard.path.clone(), shard);
    }

    // Tag existing shards with their plan owner if newly assigned.
    let existing_paths: Vec<String> = shards.keys().cloned().collect();
    for path in existing_paths {
        let owner = matchers.owner_of(&path);
        if let Some(shard) = shards.get_mut(&path)
            && shard.owner != owner
        {
            shard.owner = owner.clone();
            for n in &mut shard.nodes {
                n.owner = owner.clone();
            }
            write_shard(tree, shard)?;
        }
    }

    // 2. Create synthetic `unit` nodes.
    for u in &units {
        let uid = unit_node_id(&u.id);
        let sig = format!("unit {} owns `{}`", u.id, u.owned_glob);
        seed.nodes.push(Node {
            id: uid,
            kind: NodeKind::Unit,
            name: u.id.clone(),
            path: String::new(),
            parent: None,
            span: [0, 0],
            sig_hash: sig_hash(&sig),
            body_hash: String::new(),
            sig,
            exported: true,
            doc: None,
            owner: Some(u.id.clone()),
            status: NodeStatus::Built,
        });
    }

    // Collect every already-built/contract node ID.
    let existing_node_ids: BTreeSet<String> = shards
        .values()
        .flat_map(|s| s.nodes.iter().map(|n| n.id.clone()))
        .collect();
    let known_files: BTreeSet<&str> = shards.keys().map(String::as_str).collect();
    let rust_roots: BTreeSet<String> = shards
        .keys()
        .filter_map(|p| {
            if let Some((prefix, _)) = p.split_once("/src/") {
                Some(prefix.to_string())
            } else if p.starts_with("src/") {
                Some(String::new())
            } else {
                None
            }
        })
        .collect();

    // 3. Create planned file nodes from each unit's owned glob (and files named in brief/context
    //    matching that glob), so every wave-1 unit has planned file nodes even when acceptance
    //    tests import nothing (e.g. subprocess CLI tests).
    let mut planned_seen: BTreeSet<String> = BTreeSet::new();
    let mut planned_count = 0usize;
    let mut planned_files_count = 0usize;
    // Track expected files per unit: `unit_id -> Vec<(path, lang)>` (non-test first).
    let mut unit_expected_files: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    // Map normalized file path -> (unit_id, lang) for cross-unit resolution.
    let mut expected_file_owner: BTreeMap<String, (String, String)> = BTreeMap::new();
    // Map module dotted name (e.g. `tinydb.btree`) -> expected file path.
    let mut module_to_file: BTreeMap<String, String> = BTreeMap::new();

    for page in &plan.pages {
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for path in expand_glob_concrete_paths(&page.owned_glob) {
            candidates.insert(path);
        }
        for wu in &page.units {
            for path in expand_glob_concrete_paths(&wu.owned_glob) {
                candidates.insert(path);
            }
        }
        let unit_text = unit_plan_text(page);
        if let Ok(glob) = Glob::new(page.owned_glob.trim()) {
            let matcher = glob.compile_matcher();
            for mentioned in extract_paths_from_text(&unit_text) {
                if matcher.is_match(&mentioned) {
                    candidates.insert(mentioned);
                }
            }
        }
        let mut files_for_unit: Vec<(String, String)> = Vec::new();
        for path in candidates {
            if matchers.is_contract(&path) || is_flow_path(&path) || has_ignored_dir(&path) {
                continue;
            }
            let Some(lang) = detect_language(&path).map(|l| l.name().to_string()) else {
                continue;
            };
            let fid = file_node_id(&lang, &path);
            seed.edges.push(Edge {
                from: unit_node_id(&page.id),
                to: fid.clone(),
                kind: EdgeKind::Owns,
                conf: Confidence::Exact,
            });
            if !existing_node_ids.contains(&fid) && planned_seen.insert(fid.clone()) {
                let name = Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path)
                    .to_string();
                seed.nodes.push(Node {
                    id: fid.clone(),
                    kind: NodeKind::File,
                    name,
                    path: path.clone(),
                    parent: None,
                    span: [0, 0],
                    sig_hash: sig_hash(&path),
                    body_hash: String::new(),
                    sig: path.clone(),
                    exported: true,
                    doc: None,
                    owner: Some(page.id.clone()),
                    status: NodeStatus::Planned,
                });
                planned_files_count += 1;
            }
            if let Some(mod_name) = python_module_name(&path) {
                module_to_file.insert(mod_name, path.clone());
            }
            expected_file_owner.insert(path.clone(), (page.id.clone(), lang.clone()));
            files_for_unit.push((path, lang));
        }
        // Sort production files before test files so primary file selection prefers production code.
        files_for_unit.sort_by(|a, b| {
            tm_core::is_test_path(&a.0)
                .cmp(&tm_core::is_test_path(&b.0))
                .then_with(|| a.0.cmp(&b.0))
        });
        unit_expected_files.insert(page.id.clone(), files_for_unit);
    }

    // 4. Create planned nodes from contract test imports + `tests` edges (and subprocess `-m <pkg>` links).
    for pf in &parsed_contracts {
        if !tm_core::is_test_path(&pf.path) {
            continue;
        }
        // Build map of local imported symbol -> test functions that call it.
        let mut callers_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
        for call in &pf.calls {
            callers_by_name
                .entry(call.callee.as_str())
                .or_default()
                .push(call.from_id.as_str());
            if let Some(ref recv) = call.receiver {
                callers_by_name
                    .entry(recv.as_str())
                    .or_default()
                    .push(call.from_id.as_str());
            }
        }

        for imp in &pf.imports {
            let target_path = resolve_import_against_set(&pf.path, &pf.lang, imp, &known_files, &rust_roots)
                .or_else(|| predict_import_target_path(&pf.path, &pf.lang, imp));
            let Some(target_path) = target_path else { continue };
            let target_path = normalize_rel_path(&target_path);
            if matchers.is_contract(&target_path) || tm_core::is_test_path(&target_path) {
                continue;
            }
            let Some(target_lang) = detect_language(&target_path).map(|l| l.name()) else {
                continue;
            };
            let target_owner = matchers.owner_of(&target_path);

            for (remote, local) in &imp.names {
                if remote == "default" || remote == "*" {
                    continue;
                }
                let sym_id = symbol_node_id(target_lang, &target_path, remote, 1);
                if !existing_node_ids.contains(&sym_id) && planned_seen.insert(sym_id.clone()) {
                    let kind = if remote.chars().next().is_some_and(char::is_uppercase) {
                        NodeKind::Class
                    } else {
                        NodeKind::Function
                    };
                    let sig = format!("planned {remote} ({target_path})");
                    seed.nodes.push(Node {
                        id: sym_id.clone(),
                        kind,
                        name: remote.clone(),
                        path: target_path.clone(),
                        parent: None,
                        span: [0, 0],
                        sig_hash: sig_hash(&sig),
                        body_hash: String::new(),
                        sig,
                        exported: true,
                        doc: None,
                        owner: target_owner.clone(),
                        status: NodeStatus::Planned,
                    });
                    planned_count += 1;
                }
                if let Some(ref owner_id) = target_owner {
                    seed.edges.push(Edge {
                        from: unit_node_id(owner_id),
                        to: file_node_id(target_lang, &target_path),
                        kind: EdgeKind::Owns,
                        conf: Confidence::Exact,
                    });
                }
                seed.edges.push(Edge {
                    from: pf.file_id.clone(),
                    to: sym_id.clone(),
                    kind: EdgeKind::Tests,
                    conf: Confidence::Import,
                });
                if let Some(test_fns) = callers_by_name.get(local.as_str()) {
                    for tf in test_fns {
                        seed.edges.push(Edge {
                            from: (*tf).to_string(),
                            to: sym_id.clone(),
                            kind: EdgeKind::Tests,
                            conf: Confidence::Import,
                        });
                    }
                }
            }
        }

        // Subprocess acceptance test fallback (e.g. `python -m tinydb`): link test file -> `<pkg>/__main__.py` or `<pkg>/cli.py`.
        if let Ok(src_text) = std::fs::read_to_string(tree.join(&pf.path)) {
            for pkg in extract_python_cli_modules(&src_text) {
                for candidate in [format!("{pkg}/__main__.py"), format!("{pkg}/cli.py"), format!("{pkg}/__init__.py")] {
                    if let Some((_, lang)) = expected_file_owner.get(&candidate) {
                        seed.edges.push(Edge {
                            from: pf.file_id.clone(),
                            to: file_node_id(lang, &candidate),
                            kind: EdgeKind::Tests,
                            conf: Confidence::Import,
                        });
                        break;
                    }
                }
            }
        }
    }

    // 5. Collect parsed contract symbols (non-test contracts) so we can link them via `contract_of`
    //    and `imports` edges and never mistakenly re-create them as planned symbols.
    let mut contract_top_nodes: Vec<Node> = Vec::new();
    let mut contract_members_by_parent: BTreeMap<String, Vec<Node>> = BTreeMap::new();
    let mut contract_sym_names: BTreeSet<String> = BTreeSet::new();
    for shard in shards.values() {
        if !matchers.is_contract(&shard.path) || tm_core::is_test_path(&shard.path) {
            continue;
        }
        for node in &shard.nodes {
            if node.kind == NodeKind::File {
                continue;
            }
            if let Some(ref parent) = node.parent {
                contract_members_by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push(node.clone());
            } else {
                contract_sym_names.insert(node.name.clone());
                contract_top_nodes.push(node.clone());
            }
        }
    }

    // 6. Create planned symbol nodes declared in each unit's brief/context/done, plus explicit
    //    cross-unit `import <Sym> from <pkg.mod>` mentions across all units.
    //    Track planned top-level exported symbols per unit for cross-unit neighbour wiring.
    let mut planned_top_by_unit: BTreeMap<String, Vec<Node>> = BTreeMap::new();
    let mut planned_classes_by_unit: BTreeMap<String, Vec<Node>> = BTreeMap::new();

    let add_planned_symbol = |seed: &mut PlanSeed,
                                  planned_seen: &mut BTreeSet<String>,
                                  planned_count: &mut usize,
                                  planned_top_by_unit: &mut BTreeMap<String, Vec<Node>>,
                                  planned_classes_by_unit: &mut BTreeMap<String, Vec<Node>>,
                                  unit_id: &str,
                                  lang: &str,
                                  path: &str,
                                  name: &str,
                                  kind: NodeKind,
                                  parent: Option<String>,
                                  sig_override: Option<String>| -> String {
        let qual = match &parent {
            Some(pid) => {
                let pqual = pid.split_once('#').map_or(pid.as_str(), |(_, q)| q);
                format!("{pqual}.{name}")
            }
            None => name.to_string(),
        };
        let sym_id = symbol_node_id(lang, path, &qual, 1);
        if !existing_node_ids.contains(&sym_id) && planned_seen.insert(sym_id.clone()) {
            let sig = sig_override.unwrap_or_else(|| format!("planned {qual} ({path})"));
            let node = Node {
                id: sym_id.clone(),
                kind,
                name: name.to_string(),
                path: path.to_string(),
                parent: parent.clone(),
                span: [0, 0],
                sig_hash: sig_hash(&sig),
                body_hash: String::new(),
                sig,
                exported: true,
                doc: None,
                owner: Some(unit_id.to_string()),
                status: NodeStatus::Planned,
            };
            if parent.is_none() {
                planned_top_by_unit
                    .entry(unit_id.to_string())
                    .or_default()
                    .push(node.clone());
                if matches!(kind, NodeKind::Class | NodeKind::Type) {
                    planned_classes_by_unit
                        .entry(unit_id.to_string())
                        .or_default()
                        .push(node.clone());
                }
            }
            seed.nodes.push(node);
            *planned_count += 1;
        }
        sym_id
    };

    // 6a. Extract declared symbols from each unit's own brief/context/done.
    for page in &plan.pages {
        let Some(files) = unit_expected_files.get(&page.id) else { continue };
        let prod_files: Vec<(String, String)> = files
            .iter()
            .filter(|(p, _)| !tm_core::is_test_path(p))
            .cloned()
            .collect();
        if prod_files.is_empty() {
            continue;
        }
        let positive_text = strip_negated_import_clauses(&unit_plan_text(page));
        for (target_path, target_lang, sym_name, kind) in
            extract_unit_declared_symbols(&positive_text, &prod_files, &contract_sym_names)
        {
            add_planned_symbol(
                &mut seed,
                &mut planned_seen,
                &mut planned_count,
                &mut planned_top_by_unit,
                &mut planned_classes_by_unit,
                &page.id,
                &target_lang,
                &target_path,
                &sym_name,
                kind,
                None,
                None,
            );
        }
    }

    // 6b. Extract explicit `import <Sym> from <pkg.mod>` / `<Sym> from <pkg.mod>` references
    //     mentioned in any unit's brief/context (creates target planned symbol if missing + `imports` edge).
    for page in &plan.pages {
        let positive_text = strip_negated_import_clauses(&unit_plan_text(page));
        let my_primary_file = unit_expected_files
            .get(&page.id)
            .and_then(|fs| fs.first())
            .map(|(p, l)| (p.clone(), file_node_id(l, p)));
        for mention in extract_import_mentions(&positive_text, &module_to_file, &expected_file_owner) {
            let Some((target_owner, target_lang)) = expected_file_owner.get(&mention.target_path).cloned() else {
                continue;
            };
            if contract_sym_names.contains(&mention.symbol) {
                continue;
            }
            let kind = if mention.symbol.chars().next().is_some_and(char::is_uppercase) {
                NodeKind::Class
            } else {
                NodeKind::Function
            };
            let sym_id = add_planned_symbol(
                &mut seed,
                &mut planned_seen,
                &mut planned_count,
                &mut planned_top_by_unit,
                &mut planned_classes_by_unit,
                &target_owner,
                &target_lang,
                &mention.target_path,
                &mention.symbol,
                kind,
                None,
                None,
            );
            // Determine importer file node: either an explicitly named sibling file in the sentence
            // (e.g. "`tinydb/storage.py` will import `BTree` from `tinydb.btree`"), or this unit's primary file.
            let importer_fid = mention
                .importer_path
                .as_ref()
                .and_then(|ip| expected_file_owner.get(ip).map(|(_, l)| file_node_id(l, ip)))
                .or_else(|| {
                    if target_owner != page.id {
                        my_primary_file.as_ref().map(|(_, fid)| fid.clone())
                    } else {
                        None
                    }
                });
            if let Some(from_fid) = importer_fid
                && from_fid != file_node_id(&target_lang, &mention.target_path)
            {
                seed.edges.push(Edge {
                    from: from_fid,
                    to: sym_id,
                    kind: EdgeKind::Imports,
                    conf: Confidence::Import,
                });
            }
        }
    }

    // 7. Link contract symbols (interfaces/Protocols/traits + members, and used contract types/functions)
    //    to owning units via `contract_of` and `imports` edges, creating planned member nodes where an
    //    implementing class is known.
    for cnode in &contract_top_nodes {
        let is_interface = is_contract_interface(cnode);
        let members = contract_members_by_parent.get(&cnode.id).cloned().unwrap_or_default();

        for page in &plan.pages {
            let raw_text = unit_plan_text(page);
            let positive_text = strip_negated_import_clauses(&raw_text);
            let mentions_sym = contains_word(&positive_text, &cnode.name);
            if !mentions_sym {
                continue;
            }
            let primary_file = unit_expected_files
                .get(&page.id)
                .and_then(|fs| fs.first())
                .cloned();
            let primary_fid = primary_file.as_ref().map(|(p, l)| file_node_id(l, p));
            let implements = is_interface && unit_implements_contract(&positive_text, &cnode.name, &page.id);

            if implements {
                // Pick the unit's planned implementing class if available, else its primary planned file.
                let impl_class = planned_classes_by_unit
                    .get(&page.id)
                    .and_then(|cs| pick_implementing_class(cs, &cnode.name))
                    .cloned();
                let target_id = impl_class
                    .as_ref()
                    .map(|c| c.id.clone())
                    .or_else(|| primary_fid.clone())
                    .unwrap_or_else(|| unit_node_id(&page.id));

                seed.edges.push(Edge {
                    from: cnode.id.clone(),
                    to: target_id.clone(),
                    kind: EdgeKind::ContractOf,
                    conf: Confidence::Import,
                });
                seed.edges.push(Edge {
                    from: cnode.id.clone(),
                    to: unit_node_id(&page.id),
                    kind: EdgeKind::ContractOf,
                    conf: Confidence::Import,
                });
                if let Some(ref fid) = primary_fid {
                    seed.edges.push(Edge {
                        from: fid.clone(),
                        to: cnode.id.clone(),
                        kind: EdgeKind::Imports,
                        conf: Confidence::Import,
                    });
                }
                for member in &members {
                    if let Some(ref cls) = impl_class {
                        let lang = detect_language(&cls.path).map_or("python", |l| l.name());
                        let planned_member_id = add_planned_symbol(
                            &mut seed,
                            &mut planned_seen,
                            &mut planned_count,
                            &mut planned_top_by_unit,
                            &mut planned_classes_by_unit,
                            &page.id,
                            lang,
                            &cls.path,
                            &member.name,
                            NodeKind::Method,
                            Some(cls.id.clone()),
                            (!member.sig.is_empty()).then(|| member.sig.clone()),
                        );
                        seed.edges.push(Edge {
                            from: member.id.clone(),
                            to: planned_member_id,
                            kind: EdgeKind::ContractOf,
                            conf: Confidence::Import,
                        });
                    } else if let Some(ref fid) = primary_fid {
                        seed.edges.push(Edge {
                            from: member.id.clone(),
                            to: fid.clone(),
                            kind: EdgeKind::ContractOf,
                            conf: Confidence::Import,
                        });
                    }
                }
            } else if let Some(ref fid) = primary_fid {
                // Unit uses this contract symbol (type, exception, dataclass, Protocol consumer, etc.).
                seed.edges.push(Edge {
                    from: fid.clone(),
                    to: cnode.id.clone(),
                    kind: EdgeKind::Imports,
                    conf: Confidence::Import,
                });
            }
        }
    }

    // 8. Link cross-unit symbol mentions across units (e.g. unit A's brief/context names unit B's
    //    planned or built exported symbol, or unit A depends on unit B).
    let mut all_unit_symbols: Vec<(String, Node)> = Vec::new();
    for (uid, nodes) in &planned_top_by_unit {
        for n in nodes {
            all_unit_symbols.push((uid.clone(), n.clone()));
        }
    }
    for shard in shards.values() {
        if matchers.is_contract(&shard.path) || tm_core::is_test_path(&shard.path) {
            continue;
        }
        let Some(ref uid) = shard.owner else { continue };
        for n in &shard.nodes {
            if n.kind != NodeKind::File && n.parent.is_none() && n.exported {
                all_unit_symbols.push((uid.clone(), n.clone()));
            }
        }
    }
    // Count how many distinct units define each short name so only unambiguous names create edges.
    let mut name_owners: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for (uid, n) in &all_unit_symbols {
        if n.name.len() >= 3 {
            name_owners.entry(n.name.as_str()).or_default().insert(uid.as_str());
        }
    }
    for page in &plan.pages {
        let Some(my_files) = unit_expected_files.get(&page.id) else { continue };
        let Some((my_path, my_lang)) = my_files.first() else { continue };
        let my_fid = file_node_id(my_lang, my_path);
        let positive_text = strip_negated_import_clauses(&unit_plan_text(page));
        for (other_uid, sym) in &all_unit_symbols {
            if other_uid == &page.id || sym.name.len() < 3 {
                continue;
            }
            if name_owners.get(sym.name.as_str()).is_some_and(|owners| owners.len() > 1) {
                continue;
            }
            if contains_word(&positive_text, &sym.name) {
                seed.edges.push(Edge {
                    from: my_fid.clone(),
                    to: sym.id.clone(),
                    kind: EdgeKind::Imports,
                    conf: Confidence::Import,
                });
            }
        }
    }

    // 9. Reconcile `owns`, `contract_of`, and flip any already-built nodes.
    reconcile_plan_seed(&mut seed, &shards, &matchers);

    write_plan_seed(tree, &seed)?;
    let prev_moves = read_meta(tree)?.map(|m| m.moves).unwrap_or_default();
    let meta = build_meta(tree, &shards, prev_moves);
    write_meta(tree, &meta)?;
    let _ = FlowDb::rebuild(tree)?;

    Ok(SeedReport {
        contracts: parsed_contracts.len(),
        units: units.len(),
        planned_nodes: planned_count,
        planned_files: planned_files_count,
    })
}

/// Check every file in `files` (or all tracked source files when `files` is empty) against
/// its stored shard blob hash; automatically re-parse any stale/missing/deleted paths and
/// return the sorted list of repaired paths.
pub fn ensure_fresh(tree: &Path, files: &[String]) -> anyhow::Result<Vec<String>> {
    let stale = if files.is_empty() {
        verify_all(tree)?
    } else {
        let mut stale = Vec::new();
        for raw in files {
            let path = normalize_rel_path(raw);
            if path.is_empty() || is_flow_path(&path) || has_ignored_dir(&path) {
                continue;
            }
            let full = tree.join(&path);
            let existing = read_shard(tree, &path)?;
            if !full.is_file() {
                if existing.is_some() {
                    stale.push(path);
                }
                continue;
            }
            if detect_language(&path).is_none() {
                continue;
            }
            let Ok(bytes) = std::fs::read(&full) else { continue };
            if bytes.len() as u64 > MAX_SOURCE_BYTES {
                continue;
            }
            let disk_blob = git_blob_sha(&bytes);
            if existing.as_ref().is_none_or(|s| s.blob != disk_blob) {
                stale.push(path);
            }
        }
        stale.sort();
        stale.dedup();
        stale
    };
    if !stale.is_empty() {
        update_files(tree, &stale)?;
    }
    Ok(stale)
}

/// Pure blob check across every tracked source file and every stored shard.
/// Returns the sorted list of stale, missing-shard, or orphaned-shard paths without mutating disk.
pub fn verify_all(tree: &Path) -> anyhow::Result<Vec<String>> {
    let tracked = discover_source_files(tree);
    let tracked_set: BTreeSet<&str> = tracked.iter().map(String::as_str).collect();
    let shards = read_all_shards(tree)?;
    let mut stale = Vec::new();

    for path in &tracked {
        let Ok(bytes) = std::fs::read(tree.join(path)) else { continue };
        let disk_blob = git_blob_sha(&bytes);
        match shards.get(path) {
            Some(shard) if shard.blob == disk_blob => {}
            _ => stale.push(path.clone()),
        }
    }
    for shard_path in shards.keys() {
        if !tracked_set.contains(shard_path.as_str()) {
            stale.push(shard_path.clone());
        }
    }
    stale.sort();
    stale.dedup();
    Ok(stale)
}

fn mark_contract_nodes(shard: &mut Shard) {
    for node in &mut shard.nodes {
        if node.kind != NodeKind::File {
            node.status = NodeStatus::Contract;
        }
    }
}

fn reconcile_plan_seed(
    seed: &mut PlanSeed,
    shards: &BTreeMap<String, Shard>,
    matchers: &OwnerMatchers,
) -> bool {
    let before = seed.clone();
    let built_ids: BTreeSet<&str> = shards
        .values()
        .flat_map(|s| s.nodes.iter().map(|n| n.id.as_str()))
        .collect();

    // Planned nodes whose IDs are now built in shards flip to built (remove planned overlay).
    seed.nodes
        .retain(|n| n.status != NodeStatus::Planned || !built_ids.contains(n.id.as_str()));

    // Ensure `unit:<id> --owns--> file` edges for all existing files matching owned_glob.
    for shard in shards.values() {
        if let Some(owner) = matchers.owner_of(&shard.path) {
            seed.edges.push(Edge {
                from: unit_node_id(&owner),
                to: file_node_id(&shard.lang, &shard.path),
                kind: EdgeKind::Owns,
                conf: Confidence::Exact,
            });
        }
    }

    // Record `contract_of` edges from contract interface symbols to implementing/inheriting symbols.
    let contract_sym_ids: BTreeSet<&str> = shards
        .values()
        .filter(|s| matchers.is_contract(&s.path) && !tm_core::is_test_path(&s.path))
        .flat_map(|s| {
            s.nodes
                .iter()
                .filter(|n| n.kind != NodeKind::File)
                .map(|n| n.id.as_str())
        })
        .collect();
    if !contract_sym_ids.is_empty() {
        for shard in shards.values() {
            if matchers.is_contract(&shard.path) {
                continue;
            }
            for edge in &shard.edges {
                if matches!(edge.kind, EdgeKind::Implements | EdgeKind::Inherits)
                    && contract_sym_ids.contains(edge.to.as_str())
                {
                    seed.edges.push(Edge {
                        from: edge.to.clone(),
                        to: edge.from.clone(),
                        kind: EdgeKind::ContractOf,
                        conf: edge.conf,
                    });
                }
            }
        }
    }

    seed.canonicalize();
    *seed != before
}

fn build_meta(tree: &Path, shards: &BTreeMap<String, Shard>, moves: BTreeMap<String, String>) -> Meta {
    let (head, tree_sha) = git_head_and_tree(tree);
    let mut languages: BTreeMap<String, usize> = BTreeMap::new();
    for s in shards.values() {
        *languages.entry(s.lang.clone()).or_insert(0) += 1;
    }
    Meta {
        schema_version: SCHEMA_VERSION,
        builder_version: BUILDER_VERSION.to_string(),
        head,
        tree: tree_sha,
        languages,
        file_count: shards.len(),
        moves,
    }
}

fn record_body_hashes(nodes: &[Node], map: &mut HashMap<String, Vec<String>>) {
    for n in nodes {
        if n.kind != NodeKind::File && !n.body_hash.is_empty() {
            map.entry(n.body_hash.clone()).or_default().push(n.id.clone());
        }
    }
}

fn detect_moves(
    new_nodes: &[Node],
    removed_by_body: &HashMap<String, Vec<String>>,
    moves: &mut BTreeMap<String, String>,
) {
    let new_ids: BTreeSet<&str> = new_nodes.iter().map(|n| n.id.as_str()).collect();
    for n in new_nodes {
        if n.kind == NodeKind::File || n.body_hash.is_empty() {
            continue;
        }
        if let Some(old_ids) = removed_by_body.get(&n.body_hash) {
            let candidates: Vec<&String> = old_ids
                .iter()
                .filter(|old| old.as_str() != n.id.as_str() && !new_ids.contains(old.as_str()))
                .collect();
            if candidates.len() == 1 {
                moves.insert(candidates[0].clone(), n.id.clone());
            }
        }
    }
}

/// Expand brace expressions like `{tinydb/btree.py,tests/test_btree.py}` or `src/{a,b}.ts`
/// into concrete repo-relative paths (ignoring wildcard entries containing `*`, `?`, `[`, `]`).
fn expand_glob_concrete_paths(glob: &str) -> Vec<String> {
    fn expand_braces(pattern: &str, depth: usize) -> Vec<String> {
        if depth > 4 {
            return vec![pattern.to_string()];
        }
        let bytes = pattern.as_bytes();
        let mut open = None;
        let mut depth_brace = 0usize;
        let mut splits = Vec::new();
        let mut close = None;
        for (idx, &b) in bytes.iter().enumerate() {
            match b {
                b'{' => {
                    if depth_brace == 0 {
                        open = Some(idx);
                        splits.clear();
                    }
                    depth_brace += 1;
                }
                b',' if depth_brace == 1 => {
                    splits.push(idx);
                }
                b'}' if depth_brace > 0 => {
                    depth_brace -= 1;
                    if depth_brace == 0 {
                        close = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        let (Some(o), Some(c)) = (open, close) else {
            return vec![pattern.to_string()];
        };
        let prefix = &pattern[..o];
        let suffix = &pattern[c + 1..];
        let mut parts = Vec::new();
        let mut prev = o + 1;
        for s in splits {
            parts.push(&pattern[prev..s]);
            prev = s + 1;
        }
        parts.push(&pattern[prev..c]);

        let mut out = Vec::new();
        for part in parts {
            let joined = format!("{prefix}{}{suffix}", part.trim());
            for expanded in expand_braces(&joined, depth + 1) {
                if out.len() >= 64 {
                    break;
                }
                out.push(expanded);
            }
        }
        out
    }

    let mut out = Vec::new();
    for raw in expand_braces(glob.trim(), 0) {
        let p = normalize_rel_path(raw.trim());
        if p.is_empty()
            || p.contains('*')
            || p.contains('?')
            || p.contains('[')
            || p.contains(']')
            || p.contains('{')
            || p.contains('}')
        {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out.dedup();
    out
}

fn unit_plan_text(page: &tm_core::PagePlan) -> String {
    let mut out = String::with_capacity(page.brief.len() + page.context.len() + page.done.len() + 32);
    out.push_str(&page.brief);
    out.push('\n');
    out.push_str(&page.context);
    out.push('\n');
    out.push_str(&page.done);
    for wu in &page.units {
        out.push('\n');
        out.push_str(&wu.brief);
        out.push('\n');
        out.push_str(&wu.context);
        out.push('\n');
        out.push_str(&wu.done);
    }
    out
}

/// Extract relative source file paths mentioned in prose (e.g. `` `tinydb/btree.py` ``).
fn extract_paths_from_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '(' | ')' | ',' | ';' | '<' | '>' | '[' | ']')
    }) {
        let trimmed = raw.trim_matches(|c: char| matches!(c, '.' | ':' | '!'));
        let norm = normalize_rel_path(trimmed);
        if norm.contains('/')
            && !norm.starts_with('/')
            && !norm.contains("://")
            && detect_language(&norm).is_some()
        {
            out.push(norm);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn python_module_name(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".py")?;
    let trimmed = stem.strip_suffix("/__init__").unwrap_or(stem);
    let trimmed = trimmed.strip_prefix("src/").unwrap_or(trimmed);
    (!trimmed.is_empty()).then(|| trimmed.replace('/', "."))
}

/// Extract Python package names invoked via `"-m", "<pkg>"` in subprocess acceptance tests.
fn extract_python_cli_modules(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let tokens: Vec<&str> = source
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '(' | ')'))
        .map(|t| t.trim_matches(|c: char| c == '"' || c == '\''))
        .filter(|t| !t.is_empty())
        .collect();
    for w in tokens.windows(2) {
        if w[0] == "-m"
            && is_valid_ident(w[1])
            && !matches!(w[1], "pytest" | "unittest" | "pip" | "venv" | "json")
        {
            out.push(w[1].to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Drop negated import clauses such as `"do not import storage, btree, or parser."` or
/// `"never import tinydb.storage"` so they do not produce false planned cross-unit edges.
fn strip_negated_import_clauses(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut keep = vec![true; text.len()];
    for marker in ["do not import", "don't import", "never import", "must not import"] {
        let mut offset = 0usize;
        while let Some(pos) = lower[offset..].find(marker) {
            let start = offset + pos;
            let rest = &text[start..];
            let end_rel = rest
                .find(['.', ';', '\n', ')'])
                .map(|p| p + 1)
                .unwrap_or(rest.len());
            let end = (start + end_rel).min(text.len());
            for flag in &mut keep[start..end] {
                *flag = false;
            }
            offset = end;
        }
    }
    let mut out = String::with_capacity(text.len());
    for (idx, ch) in text.char_indices() {
        if keep[idx] {
            out.push(ch);
        }
    }
    out
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_noise_symbol(name: &str) -> bool {
    if name.len() < 2 || name.starts_with("__") {
        return true;
    }
    matches!(
        name,
        "int"
            | "float"
            | "str"
            | "bool"
            | "bytes"
            | "list"
            | "dict"
            | "tuple"
            | "set"
            | "None"
            | "True"
            | "False"
            | "Any"
            | "Path"
            | "Sequence"
            | "Callable"
            | "Optional"
            | "Union"
            | "Literal"
            | "Protocol"
            | "Enum"
            | "Exception"
            | "ValueError"
            | "TypeError"
            | "KeyError"
            | "RuntimeError"
            | "SystemExit"
            | "TextIO"
            | "SELECT"
            | "FROM"
            | "WHERE"
            | "INSERT"
            | "INTO"
            | "VALUES"
            | "UPDATE"
            | "DELETE"
            | "CREATE"
            | "TABLE"
            | "DROP"
            | "ORDER"
            | "GROUP"
            | "LIMIT"
            | "INNER"
            | "JOIN"
            | "COUNT"
            | "SUM"
            | "AVG"
            | "MIN"
            | "MAX"
            | "NULL"
            | "INTEGER"
            | "TEXT"
            | "REAL"
            | "PRIMARY"
            | "KEY"
            | "AND"
            | "OR"
            | "NOT"
            | "ASC"
            | "DESC"
            | "EOF"
            | "CLI"
            | "REPL"
            | "SQL"
            | "AST"
            | "JSON"
            | "UTF"
            | "API"
            | "CRUD"
            | "DDL"
            | "DML"
            | "CRC32"
            | "README"
            | "TODO"
            | "NOTE"
            | "self"
            | "cls"
            | "this"
            | "class"
            | "function"
            | "struct"
            | "enum"
            | "trait"
            | "interface"
            | "export"
            | "expose"
            | "import"
            | "from"
            | "return"
            | "pass"
            | "len"
            | "print"
            | "super"
            | "range"
    )
}

/// Find the owned production file most recently named before `byte_pos` in `text`,
/// falling back to the single production file when the unit owns only one.
fn active_file_at<'a>(text: &str, byte_pos: usize, prod_files: &'a [(String, String)]) -> Option<&'a (String, String)> {
    if prod_files.len() == 1 {
        return prod_files.first();
    }
    let prefix = &text[..byte_pos.min(text.len())];
    let mut best: Option<(usize, &'a (String, String))> = None;
    for entry in prod_files {
        let (path, _) = entry;
        if let Some(at) = prefix.rfind(path.as_str())
            && best.is_none_or(|(b, _)| at > b)
        {
            best = Some((at, entry));
        }
        if let Some(mod_name) = python_module_name(path)
            && let Some(at) = prefix.rfind(mod_name.as_str())
            && best.is_none_or(|(b, _)| at > b)
        {
            best = Some((at, entry));
        }
    }
    best.map(|(_, entry)| entry).or_else(|| prod_files.first())
}

/// Deterministically extract planned top-level classes/types/functions declared by a unit's
/// `brief`/`context`/`done` text (excluding already-defined contract symbols and noise).
fn extract_unit_declared_symbols(
    text: &str,
    prod_files: &[(String, String)],
    contract_syms: &BTreeSet<String>,
) -> Vec<(String, String, String, NodeKind)> {
    let mut results: Vec<(String, String, String, NodeKind)> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();

    let mut record = |byte_pos: usize, name: &str, kind: NodeKind| {
        if !is_valid_ident(name) || is_noise_symbol(name) || contract_syms.contains(name) {
            return;
        }
        let Some((path, lang)) = active_file_at(text, byte_pos, prod_files) else {
            return;
        };
        if seen.insert((path.clone(), name.to_string())) {
            results.push((path.clone(), lang.clone(), name.to_string(), kind));
        }
    };

    // Strip backticks while maintaining approximate byte offsets by replacing `` ` `` with space.
    let clean: String = text.chars().map(|c| if c == '`' { ' ' } else { c }).collect();
    let words: Vec<(usize, &str)> = {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < clean.len() {
            let ch = clean[i..].chars().next().unwrap();
            if ch.is_whitespace() {
                i += ch.len_utf8();
                continue;
            }
            let start = i;
            while i < clean.len() && !clean[i..].chars().next().unwrap().is_whitespace() {
                i += clean[i..].chars().next().unwrap().len_utf8();
            }
            out.push((start, &clean[start..i]));
        }
        out
    };

    // Pass 1: Explicit keyword declarations (`class Foo`, `struct Bar`, `interface Baz`, `trait Qux`, `def f(`, `fn f(`, `function f(`).
    for idx in 0..words.len() {
        let (pos, tok) = words[idx];
        let trimmed = tok.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | ':' | '"' | '\''));
        let kind_opt = match trimmed {
            "class" => Some(NodeKind::Class),
            "struct" | "enum" | "trait" | "interface" => Some(NodeKind::Type),
            "def" | "fn" | "function" => Some(NodeKind::Function),
            _ => None,
        };
        if let Some(kind) = kind_opt
            && let Some((_, next_tok)) = words.get(idx + 1)
        {
            let sym = next_tok
                .split(['(', ':', '<', ',', ';', '.', ')'])
                .next()
                .unwrap_or("")
                .trim_matches(|c: char| matches!(c, '"' | '\''));
            // Avoid phrases like "class hierarchy" or "function calls" (for class/type require UpperCase).
            let ok = match kind {
                NodeKind::Class | NodeKind::Type => sym.chars().next().is_some_and(char::is_uppercase),
                NodeKind::Function => next_tok.contains('('),
                _ => false,
            };
            if ok {
                record(pos, sym, kind);
            }
        }
    }

    // Pass 2: Export/expose/implement clauses (`export tokenize(sql: str)`, `implement format_result(...), run_file(...), and main(...)`,
    //         `export Database`).
    for idx in 0..words.len() {
        let (pos, tok) = words[idx];
        let low = tok
            .trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | ':' | '.'))
            .to_ascii_lowercase();
        if !matches!(
            low.as_str(),
            "export" | "exports" | "exporting" | "expose" | "exposes" | "exposing" | "implement" | "implements" | "implementing"
        ) {
            continue;
        }
        // Scan forward up to sentence end (`. ` / `\n`) or 24 tokens for declared signatures / CamelCase exports.
        let limit = (idx + 28).min(words.len());
        let mut j = idx + 1;
        let mut paren_depth = 0usize;
        while j < limit {
            let (wpos, wtok) = words[j];
            // Stop at sentence boundary outside parentheses (e.g. a word ending with `.` that is not a file path or `...`).
            let ends_sentence = paren_depth == 0
                && wtok.ends_with('.')
                && !wtok.ends_with("...")
                && detect_language(wtok.trim_end_matches('.')).is_none();
            if paren_depth == 0 {
                // Skip `from <module>` targets (those are imports, handled separately).
                if wtok.eq_ignore_ascii_case("from") {
                    break;
                }
                if let Some((head, _)) = wtok.split_once('(') {
                    let sym = head.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '"' | '\'' | '*'));
                    if is_valid_ident(sym) && !sym.contains('.') {
                        let kind = if sym.chars().next().is_some_and(char::is_uppercase) {
                            NodeKind::Class
                        } else {
                            NodeKind::Function
                        };
                        record(wpos, sym, kind);
                    }
                } else if j == idx + 1 {
                    let sym = wtok.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | '.' | '(' | ')' | '"' | '\''));
                    if is_valid_ident(sym)
                        && sym.chars().next().is_some_and(char::is_uppercase)
                        && sym.chars().skip(1).any(|c| c.is_ascii_lowercase())
                    {
                        record(pos, sym, NodeKind::Class);
                    }
                }
            }
            for ch in wtok.chars() {
                if ch == '(' {
                    paren_depth += 1;
                } else if ch == ')' {
                    paren_depth = paren_depth.saturating_sub(1);
                }
            }
            if ends_sentence {
                break;
            }
            j += 1;
        }
    }

    results
}

struct ImportMention {
    importer_path: Option<String>,
    target_path: String,
    symbol: String,
}

/// Extract cross-unit import mentions like:
/// - `` `tinydb/storage.py` will import `BTree` directly from `tinydb.btree` ``
/// - `` import `BTree` from `tinydb.btree` ``
/// - `` `parse` from `tinydb.parser`, `StorageEngine` from `tinydb.storage` ``
fn extract_import_mentions(
    text: &str,
    module_to_file: &BTreeMap<String, String>,
    expected_file_owner: &BTreeMap<String, (String, String)>,
) -> Vec<ImportMention> {
    let mut out = Vec::new();
    let clean: String = text.chars().map(|c| if c == '`' { ' ' } else { c }).collect();
    // Split cleanly on newlines and `;` (and `. ` followed by whitespace) so dotted module names stay intact.
    let mut clauses: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let bytes = clean.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        let is_break = b == b'\n'
            || b == b';'
            || (b == b'.'
                && idx + 1 < bytes.len()
                && bytes[idx + 1].is_ascii_whitespace());
        if is_break {
            clauses.push(&clean[start..idx]);
            start = idx + 1;
        }
        idx += 1;
    }
    if start < clean.len() {
        clauses.push(&clean[start..]);
    }

    for clause in clauses {
        let tokens: Vec<&str> = clause.split_whitespace().collect();
        // Check if this clause explicitly names an importer file before "will import" / "imports".
        let explicit_importer = tokens
            .iter()
            .position(|t| t.eq_ignore_ascii_case("import") || t.eq_ignore_ascii_case("imports"))
            .and_then(|imp_pos| {
                tokens[..imp_pos].iter().rev().find_map(|t| {
                    let cand = normalize_rel_path(t.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ':' | '"' | '\'')));
                    expected_file_owner.contains_key(&cand).then_some(cand)
                })
            });

        for (i, tok) in tokens.iter().enumerate() {
            if !tok.eq_ignore_ascii_case("from") {
                continue;
            }
            let Some(next) = tokens.get(i + 1) else { continue };
            let mod_tok = next.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ':' | '"' | '\'' | '.'));
            let target_path = module_to_file
                .get(mod_tok)
                .cloned()
                .or_else(|| {
                    let norm = normalize_rel_path(mod_tok);
                    expected_file_owner.contains_key(&norm).then_some(norm)
                });
            let Some(target_path) = target_path else { continue };

            // Case A: `from <mod> import <Sym1>, <Sym2>`
            if tokens.get(i + 2).is_some_and(|t| t.eq_ignore_ascii_case("import")) {
                for sym_raw in tokens.iter().skip(i + 3).take(8) {
                    let sym = sym_raw.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ':' | '"' | '\'' | '.'));
                    if sym.eq_ignore_ascii_case("and") || sym.eq_ignore_ascii_case("as") {
                        continue;
                    }
                    if !is_valid_ident(sym) || is_noise_symbol(sym) {
                        break;
                    }
                    out.push(ImportMention {
                        importer_path: explicit_importer.clone(),
                        target_path: target_path.clone(),
                        symbol: sym.to_string(),
                    });
                }
                continue;
            }

            // Case B: `<Sym> [directly] from <mod>` or `import <Sym1>/<Sym2> from <mod>`
            if i > 0 {
                let mut back = i - 1;
                if tokens[back].eq_ignore_ascii_case("directly") && back > 0 {
                    back -= 1;
                }
                // Walk backwards across slash/comma-separated symbol list (up to 6 tokens).
                let mut syms = Vec::new();
                let mut steps = 0usize;
                loop {
                    let raw = tokens[back];
                    if raw.eq_ignore_ascii_case("import")
                        || raw.eq_ignore_ascii_case("imports")
                        || raw.eq_ignore_ascii_case("use")
                        || raw.eq_ignore_ascii_case("uses")
                        || raw.eq_ignore_ascii_case("dependencies:")
                        || raw.eq_ignore_ascii_case("dependency:")
                        || raw.eq_ignore_ascii_case("export")
                    {
                        break;
                    }
                    if raw.eq_ignore_ascii_case("and") {
                        if back == 0 || steps >= 6 {
                            break;
                        }
                        back -= 1;
                        steps += 1;
                        continue;
                    }
                    let mut found_any = false;
                    for piece in raw.split('/') {
                        let sym = piece
                            .split('(')
                            .next()
                            .unwrap_or("")
                            .trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ':' | '"' | '\'' | '.'));
                        if is_valid_ident(sym) && !is_noise_symbol(sym) {
                            syms.push(sym.to_string());
                            found_any = true;
                        }
                    }
                    if !found_any || back == 0 || steps >= 6 {
                        break;
                    }
                    // Continue backwards only if separated by comma or `and` or `/`.
                    let prev_ends_comma = back > 0 && tokens[back - 1].ends_with(',');
                    let prev_is_and = back > 0 && tokens[back - 1].eq_ignore_ascii_case("and");
                    if !prev_ends_comma && !prev_is_and {
                        break;
                    }
                    back -= 1;
                    steps += 1;
                }
                for sym in syms {
                    out.push(ImportMention {
                        importer_path: explicit_importer.clone(),
                        target_path: target_path.clone(),
                        symbol: sym,
                    });
                }
            }
        }
    }
    out
}

/// True when a contract node represents an interface/Protocol/ABC/trait that a unit implements.
fn is_contract_interface(node: &Node) -> bool {
    if !matches!(node.kind, NodeKind::Class | NodeKind::Type) {
        return false;
    }
    node.name.ends_with("Protocol")
        || node.name.ends_with("Interface")
        || node.sig.contains("(Protocol")
        || node.sig.contains("(ABC")
        || node.sig.starts_with("trait ")
        || node.sig.starts_with("pub trait ")
        || node.sig.starts_with("interface ")
        || node.sig.starts_with("export interface ")
}

/// True when a unit's brief/context declares that this unit implements/conforms to `contract_name`
/// (excluding consumers like `"operating against StorageProtocol"` or `"fake implementing StorageProtocol"`).
fn unit_implements_contract(text: &str, contract_name: &str, unit_id: &str) -> bool {
    let clean: String = text.chars().map(|c| if c == '`' { ' ' } else { c }).collect();
    let lower = clean.to_ascii_lowercase();
    let cname_low = contract_name.to_ascii_lowercase();
    let mut offset = 0usize;
    while let Some(pos) = lower[offset..].find(&cname_low) {
        let start = offset + pos;
        let end = start + cname_low.len();
        let before = &lower[start.saturating_sub(48)..start];
        let after = &lower[end..(end + 32).min(lower.len())];
        let is_fake_or_consumer = before.contains("fake")
            || before.contains("mock")
            || before.contains("stub")
            || before.contains("against");
        if !is_fake_or_consumer
            && (before.contains("conforming to")
                || before.contains("conforms to")
                || before.contains("implementing")
                || before.contains("implements")
                || before.contains("implement ")
                || after.contains("methods"))
        {
            return true;
        }
        offset = end;
    }
    // Fallback: Protocol stem matches unit id (e.g. `BTreeProtocol` <-> `btree`, `StorageProtocol` <-> `storage`).
    let stem = cname_low
        .strip_suffix("protocol")
        .or_else(|| cname_low.strip_suffix("interface"))
        .unwrap_or(&cname_low);
    !stem.is_empty() && unit_id.to_ascii_lowercase().contains(stem)
}

/// Pick the best planned class in a unit that implements `contract_name` (preferring stem match).
fn pick_implementing_class<'a>(classes: &'a [Node], contract_name: &str) -> Option<&'a Node> {
    let cname_low = contract_name.to_ascii_lowercase();
    let stem = cname_low
        .strip_suffix("protocol")
        .or_else(|| cname_low.strip_suffix("interface"))
        .unwrap_or(&cname_low);
    if !stem.is_empty()
        && let Some(found) = classes.iter().find(|c| c.name.to_ascii_lowercase().contains(stem))
    {
        return Some(found);
    }
    classes.first()
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut offset = 0usize;
    while let Some(pos) = haystack[offset..].find(needle) {
        let start = offset + pos;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_word(bytes[start - 1]);
        let right_ok = end >= bytes.len() || !is_word(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        offset = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;
    use tm_core::{ContractFile, PagePlan, ProjectKind};

    fn git_repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tm-flow-upd-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let st = Command::new("git").arg("-C").arg(&dir).args(args).status().unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        dir
    }

    fn git_commit(dir: &Path, msg: &str) {
        assert!(Command::new("git").arg("-C").arg(dir).args(["add", "-A"]).status().unwrap().success());
        assert!(Command::new("git").arg("-C").arg(dir).args(["commit", "-q", "-m", msg]).status().unwrap().success());
    }

    #[test]
    fn incremental_edit_rename_delete_rewrites_only_touched_shard() {
        let dir = git_repo("inc");
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/a.py"), "def alpha():\n    return 1\n").unwrap();
        std::fs::write(dir.join("pkg/b.py"), "def beta():\n    return 2\n").unwrap();
        git_commit(&dir, "init");

        let r1 = build(&dir).unwrap();
        assert_eq!(r1.parsed.len(), 2);
        assert_eq!(r1.total_files, 2);

        // Second build with no edits re-parses nothing.
        let r2 = build(&dir).unwrap();
        assert!(r2.parsed.is_empty(), "expected 0 parsed on clean rebuild, got {:?}", r2.parsed);
        assert_eq!(r2.unchanged, 2);

        // Record shard mtime of untouched `pkg/b.py`.
        let b_shard = crate::store::shard_path(&dir, "pkg/b.py");
        let b_mtime_before = std::fs::metadata(&b_shard).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));

        // Edit only `pkg/a.py` -> update_files rewrites ONLY `pkg/a.py` shard.
        std::fs::write(dir.join("pkg/a.py"), "def alpha():\n    return 99\n").unwrap();
        let u1 = update_files(&dir, &["pkg/a.py".into()]).unwrap();
        assert_eq!(u1.updated, vec!["pkg/a.py"]);
        let b_mtime_after = std::fs::metadata(&b_shard).unwrap().modified().unwrap();
        assert_eq!(b_mtime_before, b_mtime_after, "untouched shard must not be rewritten");

        // Rename `pkg/a.py` -> `pkg/a2.py` keeping body: records move + removes old shard.
        std::fs::rename(dir.join("pkg/a.py"), dir.join("pkg/a2.py")).unwrap();
        let u2 = update_files(&dir, &["pkg/a.py".into(), "pkg/a2.py".into()]).unwrap();
        assert_eq!(u2.removed, vec!["pkg/a.py"]);
        assert_eq!(u2.updated, vec!["pkg/a2.py"]);
        assert_eq!(
            u2.moves.get("py:pkg/a.py#alpha").map(String::as_str),
            Some("py:pkg/a2.py#alpha")
        );
        assert!(!crate::store::shard_path(&dir, "pkg/a.py").exists());

        // Delete `pkg/a2.py` -> shard removed, `pkg/b.py` still untouched.
        std::fs::remove_file(dir.join("pkg/a2.py")).unwrap();
        let u3 = update_files(&dir, &["pkg/a2.py".into()]).unwrap();
        assert_eq!(u3.removed, vec!["pkg/a2.py"]);
        assert!(!crate::store::shard_path(&dir, "pkg/a2.py").exists());
        assert_eq!(
            std::fs::metadata(&b_shard).unwrap().modified().unwrap(),
            b_mtime_before
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_from_plan_creates_planned_nodes_and_flips_to_built_with_same_id() {
        let dir = git_repo("seed");
        std::fs::create_dir_all(dir.join("tinydb")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        let contract_src = "from typing import Protocol\nclass StorageProtocol(Protocol):\n    def save(self) -> None: ...\n";
        let test_src = "from tinydb.executor import Executor\ndef test_exec():\n    e = Executor()\n";
        std::fs::write(dir.join("tinydb/contract.py"), contract_src).unwrap();
        std::fs::write(dir.join("tests/test_acceptance.py"), test_src).unwrap();
        git_commit(&dir, "lock contracts");

        let plan = Plan {
            kind: ProjectKind::Cli,
            contracts: vec![
                ContractFile { path: "tinydb/contract.py".into(), content: contract_src.into(), phase: 0 },
                ContractFile { path: "tests/test_acceptance.py".into(), content: test_src.into(), phase: 0 },
            ],
            pages: vec![PagePlan {
                id: "executor".into(),
                owned_glob: "tinydb/executor.py".into(),
                brief: "Build executor".into(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let seeded = seed_from_plan(&dir, &plan).unwrap();
        assert_eq!(seeded.contracts, 2);
        assert_eq!(seeded.units, 1);
        assert_eq!(seeded.planned_nodes, 1);

        let db = FlowDb::open_or_rebuild(&dir).unwrap();
        let contract_node = db.get_node("py:tinydb/contract.py#StorageProtocol").unwrap().unwrap();
        assert_eq!(contract_node.status, NodeStatus::Contract);

        let planned = db.get_node("py:tinydb/executor.py#Executor").unwrap().unwrap();
        assert_eq!(planned.status, NodeStatus::Planned);
        assert_eq!(planned.owner.as_deref(), Some("executor"));
        let incoming = db.incoming_edges(&planned.id).unwrap();
        assert!(incoming.iter().any(|e| e.kind == EdgeKind::Tests));
        drop(db);

        // Now land the implementation file and run update_files -> flips to Built with SAME ID!
        std::fs::write(
            dir.join("tinydb/executor.py"),
            "from tinydb.contract import StorageProtocol\nclass Executor(StorageProtocol):\n    def save(self) -> None:\n        pass\n",
        )
        .unwrap();
        let upd = update_files(&dir, &["tinydb/executor.py".into()]).unwrap();
        assert_eq!(upd.flipped_from_planned, vec!["py:tinydb/executor.py#Executor"]);

        let db2 = FlowDb::open_or_rebuild(&dir).unwrap();
        let built = db2.get_node("py:tinydb/executor.py#Executor").unwrap().unwrap();
        assert_eq!(built.id, planned.id);
        assert_eq!(built.status, NodeStatus::Built);
        assert_eq!(built.owner.as_deref(), Some("executor"));
        // ContractOf edge created from StorageProtocol -> Executor
        let contract_out = db2.outgoing_edges("py:tinydb/contract.py#StorageProtocol").unwrap();
        assert!(contract_out.iter().any(|e| e.kind == EdgeKind::ContractOf && e.to == built.id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_all_detects_staleness_and_ensure_fresh_repairs_it() {
        let dir = git_repo("stale");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> i32 { 1 }\n").unwrap();
        git_commit(&dir, "init");

        build(&dir).unwrap();
        assert!(verify_all(&dir).unwrap().is_empty());

        // Edit file on disk without updating .flow.
        std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> i32 { 2 }\npub fn two() {}\n").unwrap();
        assert_eq!(verify_all(&dir).unwrap(), vec!["src/lib.rs"]);

        // ensure_fresh auto-repairs it.
        let repaired = ensure_fresh(&dir, &[]).unwrap();
        assert_eq!(repaired, vec!["src/lib.rs"]);
        assert!(verify_all(&dir).unwrap().is_empty());

        let db = FlowDb::open_or_rebuild(&dir).unwrap();
        assert!(db.get_node("rs:src/lib.rs#two").unwrap().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_from_plan_seeds_n6_subprocess_contract_with_wave1_unit_maps_and_flips_to_built() {
        let dir = git_repo("n6-wave1");
        std::fs::create_dir_all(dir.join("tinydb")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();

        let contract_src = "\
from typing import Any, Protocol

class StorageError(Exception):
    pass

class Token:
    pass

class BTreeProtocol(Protocol):
    def insert(self, key: Any, value: Any) -> None: ...
    def search(self, key: Any) -> Any | None: ...

class StorageProtocol(Protocol):
    def create_table(self, name: str) -> None: ...
    def get_by_pk(self, table: str, key: Any) -> Any: ...
";
        // Subprocess-only acceptance test: imports NOTHING from `tinydb`!
        let accept_src = "\
import subprocess
import sys

def test_cli_runs():
    res = subprocess.run([sys.executable, '-m', 'tinydb', '--help'], capture_output=True, text=True)
    assert res.returncode == 0
";
        std::fs::write(dir.join("tinydb/contract.py"), contract_src).unwrap();
        std::fs::write(dir.join("tests/test_acceptance_contract.py"), accept_src).unwrap();
        git_commit(&dir, "lock contracts");

        let plan = Plan {
            kind: ProjectKind::Cli,
            contracts: vec![
                ContractFile { path: "tinydb/contract.py".into(), content: contract_src.into(), phase: 0 },
                ContractFile { path: "tests/test_acceptance_contract.py".into(), content: accept_src.into(), phase: 0 },
            ],
            pages: vec![
                PagePlan {
                    id: "btree".into(),
                    owned_glob: "{tinydb/btree.py,tests/test_btree.py}".into(),
                    brief: "Implement a balanced B-tree in `tinydb/btree.py` conforming to `BTreeProtocol` in `tinydb/contract.py`.".into(),
                    context: "Locked interface: `tinydb/contract.py` defines `BTreeProtocol` and `StorageError`. Export `class BTree` from `tinydb/btree.py`. Sibling boundary: `tinydb/storage.py` will import `BTree` directly from `tinydb.btree`; do not import `tinydb.storage` or `tinydb.parser`.".into(),
                    ..Default::default()
                },
                PagePlan {
                    id: "frontend".into(),
                    owned_glob: "{tinydb/tokenizer.py,tinydb/parser.py,tests/test_parser.py}".into(),
                    brief: "Implement lexer in `tinydb/tokenizer.py` and parser in `tinydb/parser.py` using `Token` from `tinydb/contract.py`.".into(),
                    context: "In `tinydb/tokenizer.py`, export `tokenize(sql: str) -> list[Token]`. In `tinydb/parser.py`, export `parse(sql: str) -> list[Any]`. Sibling boundary: `tinydb/cli.py` will import `parse` from `tinydb.parser`; never import `tinydb.storage` or `tinydb.btree`.".into(),
                    ..Default::default()
                },
                PagePlan {
                    id: "storage".into(),
                    owned_glob: "{tinydb/storage.py,tests/test_storage.py}".into(),
                    brief: "Implement persistent storage in `tinydb/storage.py` conforming to `StorageProtocol`.".into(),
                    context: "Concrete dependency: import `BTree` from `tinydb.btree`. Export `class StorageEngine` implementing `StorageProtocol`.".into(),
                    depends_on: vec!["btree".into()],
                    ..Default::default()
                },
                PagePlan {
                    id: "cli".into(),
                    owned_glob: "{tinydb/__main__.py,tinydb/cli.py,README.md}".into(),
                    brief: "Wire CLI in `tinydb/cli.py` and `tinydb/__main__.py`.".into(),
                    context: "Concrete dependencies: `parse` from `tinydb.parser`, `StorageEngine` from `tinydb.storage`. In `tinydb/cli.py`, implement `main(argv=None) -> int`.".into(),
                    depends_on: vec!["frontend".into(), "storage".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let seeded = seed_from_plan(&dir, &plan).unwrap();
        assert!(seeded.planned_files >= 7, "expected planned file nodes from owned globs, got {seeded:?}");
        assert!(seeded.planned_nodes >= 5, "expected planned symbol nodes from briefs/contracts, got {seeded:?}");

        let db = FlowDb::open_or_rebuild(&dir).unwrap();
        // Planned file node exists with exact stable ID and owner.
        let btree_file = db.get_node("py:tinydb/btree.py").unwrap().expect("planned btree file");
        assert_eq!(btree_file.kind, NodeKind::File);
        assert_eq!(btree_file.status, NodeStatus::Planned);
        assert_eq!(btree_file.owner.as_deref(), Some("btree"));

        // Planned class + Protocol member methods exist with exact stable IDs!
        let btree_cls = db.get_node("py:tinydb/btree.py#BTree").unwrap().expect("planned BTree class");
        assert_eq!(btree_cls.status, NodeStatus::Planned);
        assert_eq!(btree_cls.owner.as_deref(), Some("btree"));
        let btree_insert = db.get_node("py:tinydb/btree.py#BTree.insert").unwrap().expect("planned BTree.insert method");
        assert_eq!(btree_insert.status, NodeStatus::Planned);

        // `contract_of` edges connect BTreeProtocol and its member to btree!
        let proto_edges = db.outgoing_edges("py:tinydb/contract.py#BTreeProtocol").unwrap();
        assert!(
            proto_edges.iter().any(|e| e.kind == EdgeKind::ContractOf && e.to == "py:tinydb/btree.py#BTree"),
            "expected contract_of edge to BTree: {proto_edges:?}"
        );
        let member_edges = db.outgoing_edges("py:tinydb/contract.py#BTreeProtocol.insert").unwrap();
        assert!(
            member_edges.iter().any(|e| e.kind == EdgeKind::ContractOf && e.to == "py:tinydb/btree.py#BTree.insert"),
            "expected contract_of edge to BTree.insert: {member_edges:?}"
        );
        drop(db);

        // Every wave-1 unit (btree, frontend) gets a non-empty unit map BEFORE any code is written!
        let contract_paths = vec!["tinydb/contract.py".into(), "tests/test_acceptance_contract.py".into()];
        let btree_map = crate::render::unit_map(&dir, "btree", &[], &contract_paths, crate::DEFAULT_UNIT_MAP_BUDGET).unwrap();
        assert!(btree_map.contains("## Flow unit map (btree)"), "{btree_map}");
        assert!(btree_map.contains("file tinydb/btree.py [planned]"), "{btree_map}");
        assert!(btree_map.contains("BTree [planned]"), "{btree_map}");
        assert!(btree_map.contains("contract BTreeProtocol"), "{btree_map}");
        assert!(btree_map.contains("contract StorageError"), "{btree_map}");
        assert!(btree_map.contains("storage:storage.py ▸ imports btree:BTree [planned]"), "{btree_map}");

        let frontend_map = crate::render::unit_map(&dir, "frontend", &[], &contract_paths, crate::DEFAULT_UNIT_MAP_BUDGET).unwrap();
        assert!(frontend_map.contains("## Flow unit map (frontend)"), "{frontend_map}");
        assert!(frontend_map.contains("file tinydb/tokenizer.py [planned]"), "{frontend_map}");
        assert!(frontend_map.contains("file tinydb/parser.py [planned]"), "{frontend_map}");
        assert!(frontend_map.contains("tokenize [planned]"), "{frontend_map}");
        assert!(frontend_map.contains("parse [planned]"), "{frontend_map}");
        assert!(frontend_map.contains("contract Token"), "{frontend_map}");
        assert!(frontend_map.contains("cli:cli.py ▸ imports frontend:parse [planned]"), "{frontend_map}");
        // Negated clause "never import tinydb.storage or tinydb.btree" must NOT create false outgoing edges!
        assert!(!frontend_map.contains("frontend ▸ imports btree"), "negated import leaked:\n{frontend_map}");
        assert!(!frontend_map.contains("frontend ▸ imports storage"), "negated import leaked:\n{frontend_map}");

        // Wave-2 / Wave-3 units also get non-empty maps right away.
        let cli_map = crate::render::unit_map(&dir, "cli", &[], &contract_paths, crate::DEFAULT_UNIT_MAP_BUDGET).unwrap();
        assert!(cli_map.contains("test_acceptance_contract.py"), "subprocess test link expected:\n{cli_map}");

        // Now land `tinydb/btree.py` -> planned file, class, and methods flip to Built with the EXACT SAME IDs!
        std::fs::write(
            dir.join("tinydb/btree.py"),
            "from tinydb.contract import BTreeProtocol\nclass BTree(BTreeProtocol):\n    def insert(self, key, value):\n        pass\n    def search(self, key):\n        return None\n",
        )
        .unwrap();
        let upd = update_files(&dir, &["tinydb/btree.py".into()]).unwrap();
        assert!(upd.flipped_files.contains(&"py:tinydb/btree.py".to_string()), "{:?}", upd.flipped_files);
        assert!(upd.flipped_from_planned.contains(&"py:tinydb/btree.py#BTree".to_string()), "{:?}", upd.flipped_from_planned);
        assert!(upd.flipped_from_planned.contains(&"py:tinydb/btree.py#BTree.insert".to_string()), "{:?}", upd.flipped_from_planned);

        let db2 = FlowDb::open_or_rebuild(&dir).unwrap();
        assert_eq!(db2.get_node("py:tinydb/btree.py").unwrap().unwrap().status, NodeStatus::Built);
        assert_eq!(db2.get_node("py:tinydb/btree.py#BTree").unwrap().unwrap().status, NodeStatus::Built);
        assert_eq!(db2.get_node("py:tinydb/btree.py#BTree.insert").unwrap().unwrap().status, NodeStatus::Built);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
