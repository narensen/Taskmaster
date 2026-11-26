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
    pub moves: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub contracts: usize,
    pub units: usize,
    pub planned_nodes: usize,
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
    if let Some(seed) = plan_seed.as_mut()
        && reconcile_plan_seed(seed, &shards, &matchers)
    {
        write_plan_seed(tree, seed)?;
    }

    let meta = build_meta(tree, &shards, moves);
    write_meta(tree, &meta)?;
    let db = FlowDb::rebuild(tree)?;

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
    if let Some(seed) = plan_seed.as_mut() {
        let before_planned: BTreeSet<String> = seed
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Planned)
            .map(|n| n.id.clone())
            .collect();
        let changed = reconcile_plan_seed(seed, &shards, &matchers);
        let after_planned: BTreeSet<String> = seed
            .nodes
            .iter()
            .filter(|n| n.status == NodeStatus::Planned)
            .map(|n| n.id.clone())
            .collect();
        for id in before_planned.difference(&after_planned) {
            flipped.push(id.clone());
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

    // 3. Create planned nodes from contract test imports + `tests` edges.
    let mut planned_seen: BTreeSet<String> = BTreeSet::new();
    let mut planned_count = 0usize;
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
    }

    // 4. Add `owns` and `contract_of` edges from contracts/existing shards.
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
}
