//! Terse prompt rendering from `.flow`: per-unit maps within strict character budgets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{Edge, EdgeKind, Node, NodeKind, NodeStatus, normalize_rel_path};
use crate::store::{FlowDb, read_plan_seed};
use crate::update::ensure_fresh;

/// Default character budget for a normal worker's unit map (~1.5k tokens).
pub const DEFAULT_UNIT_MAP_BUDGET: usize = 6_000;
/// Larger character budget for repairs and integrator units (~3k tokens).
pub const REPAIR_UNIT_MAP_BUDGET: usize = 12_000;

/// True when `unit_id` imports or calls symbols from 3 or more distinct other units.
pub fn is_integrator_unit(tree: &Path, unit_id: &str) -> bool {
    let Ok(db) = FlowDb::open_or_rebuild(tree) else {
        return false;
    };
    let Ok(all_nodes) = db.all_nodes() else {
        return false;
    };
    let owner_by_id: BTreeMap<&str, Option<&str>> = all_nodes
        .iter()
        .map(|n| (n.id.as_str(), n.owner.as_deref()))
        .collect();
    let mut other_units: BTreeSet<&str> = BTreeSet::new();
    for node in all_nodes.iter().filter(|n| n.owner.as_deref() == Some(unit_id)) {
        if let Ok(edges) = db.outgoing_edges(&node.id) {
            for e in edges {
                if matches!(e.kind, EdgeKind::Imports | EdgeKind::Calls | EdgeKind::Inherits | EdgeKind::Implements)
                    && let Some(Some(other)) = owner_by_id.get(e.to.as_str())
                    && *other != unit_id
                {
                    other_units.insert(*other);
                }
            }
        }
    }
    if other_units.len() >= 3 {
        return true;
    }
    if let Ok(Some(seed)) = read_plan_seed(tree)
        && let Some(u) = seed.units.iter().find(|u| u.id == unit_id)
        && u.depends_on.len() >= 3
    {
        return true;
    }
    false
}

/// Render the terse, budget-capped `.flow` unit map for `unit_id`, placed after `--- Your task ---`.
///
/// Priority order:
/// 1. The unit's own nodes (built or planned).
/// 2. Referenced contract symbols — line refs only (never duplicating contract text).
/// 3. 1-hop cross-unit neighbours (built or planned, with owner and status).
/// 4. Tests touching this unit.
pub fn unit_map(
    tree: &Path,
    unit_id: &str,
    owned_files: &[String],
    contract_paths: &[String],
    budget_chars: usize,
) -> anyhow::Result<String> {
    if budget_chars < 64 {
        return Ok(String::new());
    }
    if !owned_files.is_empty() {
        let _ = ensure_fresh(tree, owned_files);
    }
    let db = FlowDb::open_or_rebuild(tree)?;
    let all_nodes = db.all_nodes()?;
    let all_edges = db.all_edges()?;
    let plan_seed = read_plan_seed(tree)?;

    let owned_file_set: BTreeSet<String> = owned_files.iter().map(|p| normalize_rel_path(p)).collect();
    let contract_set: BTreeSet<String> = contract_paths.iter().map(|p| normalize_rel_path(p)).collect();
    let node_by_id: BTreeMap<&str, &Node> = all_nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    // Identify every node belonging to this unit (by owner or owned_files list).
    let mut unit_node_ids: BTreeSet<&str> = BTreeSet::new();
    let mut unit_nodes: Vec<&Node> = Vec::new();
    for n in &all_nodes {
        let belongs = n.owner.as_deref() == Some(unit_id)
            || (!n.path.is_empty() && owned_file_set.contains(&n.path));
        if belongs && n.kind != NodeKind::Unit {
            unit_node_ids.insert(n.id.as_str());
            if n.kind != NodeKind::File {
                unit_nodes.push(n);
            }
        }
    }

    let is_contract_node = |n: &Node| -> bool {
        n.status == NodeStatus::Contract
            || (!n.path.is_empty() && contract_set.contains(&n.path) && !tm_core::is_test_path(&n.path))
    };

    // 2. Referenced contract symbols (line refs only — never duplicate full contract text).
    let mut contract_refs: BTreeMap<&str, &Node> = BTreeMap::new();
    // 3. 1-hop cross-unit neighbours (high-confidence or planned).
    let mut cross_lines: Vec<String> = Vec::new();
    let mut seen_cross: BTreeSet<String> = BTreeSet::new();
    // 4. Tests touching this unit.
    let mut test_nodes: BTreeMap<&str, &Node> = BTreeMap::new();

    for edge in &all_edges {
        let from_mine = unit_node_ids.contains(edge.from.as_str());
        let to_mine = unit_node_ids.contains(edge.to.as_str());
        if from_mine == to_mine {
            continue;
        }
        if from_mine {
            if let Some(target) = node_by_id.get(edge.to.as_str()).copied() {
                if is_contract_node(target) {
                    if target.kind != NodeKind::File {
                        contract_refs.insert(target.id.as_str(), target);
                    }
                    continue;
                }
                if target.kind == NodeKind::Unit {
                    continue;
                }
                if tm_core::is_test_path(&target.path) {
                    test_nodes.insert(target.id.as_str(), target);
                    continue;
                }
                if (edge.conf.is_high() || target.status == NodeStatus::Planned)
                    && (target.kind != NodeKind::File || target.status == NodeStatus::Planned)
                    && seen_cross.insert(format!("out:{}:{}", edge.kind.as_str(), target.id))
                {
                    cross_lines.push(format_cross_outgoing(unit_id, edge, target));
                }
            }
        } else if let Some(src) = node_by_id.get(edge.from.as_str()).copied() {
            if is_contract_node(src) {
                if src.kind != NodeKind::File {
                    contract_refs.insert(src.id.as_str(), src);
                }
                continue;
            }
            if src.kind == NodeKind::Unit {
                continue;
            }
            if edge.kind == EdgeKind::Tests || tm_core::is_test_path(&src.path) {
                test_nodes.insert(src.id.as_str(), src);
                continue;
            }
            if (edge.conf.is_high() || src.status == NodeStatus::Planned)
                && src.kind != NodeKind::File
                && seen_cross.insert(format!("in:{}:{}", edge.kind.as_str(), src.id))
            {
                let my_target = node_by_id
                    .get(edge.to.as_str())
                    .map(|n| qualified_label(n))
                    .unwrap_or_else(|| edge.to.clone());
                cross_lines.push(format_cross_incoming(unit_id, &my_target, edge, src));
            }
        }
    }

    // For units with declared `depends_on` (e.g. greenfield integrator units with no files yet),
    // also include exported built/planned symbols from upstream dependencies as cross-unit context.
    if let Some(seed) = &plan_seed
        && let Some(u) = seed.units.iter().find(|u| u.id == unit_id)
    {
        for dep_unit in &u.depends_on {
            for n in all_nodes.iter().filter(|n| {
                n.owner.as_deref() == Some(dep_unit.as_str())
                    && n.kind != NodeKind::File
                    && n.kind != NodeKind::Unit
                    && n.exported
            }) {
                if seen_cross.insert(format!("dep:{}", n.id)) {
                    cross_lines.push(format_dep_symbol(dep_unit, n));
                }
            }
        }
        // Also surface contract tests that import/test this unit's planned symbols (already in test_nodes
        // if edges exist), plus if unit has no files yet and a contract test references a contract
        // interface mentioned alongside it.
    }

    if unit_nodes.is_empty()
        && contract_refs.is_empty()
        && cross_lines.is_empty()
        && test_nodes.is_empty()
    {
        return Ok(String::new());
    }

    let mut out = format!("## Flow unit map ({unit_id})\n");
    let mut truncated = false;

    if !unit_nodes.is_empty() {
        append_section_header(&mut out, "Your symbols:\n", budget_chars, &mut truncated);
        for n in &unit_nodes {
            let line = format_own_node(n);
            if !push_line(&mut out, &line, budget_chars) {
                truncated = true;
                break;
            }
        }
    }

    if !truncated && !contract_refs.is_empty() {
        append_section_header(&mut out, "Referenced contracts (line refs only):\n", budget_chars, &mut truncated);
        for n in contract_refs.values() {
            let line = format!(
                "- contract {} ({}) {}:{}-{}\n",
                qualified_label(n),
                n.kind.as_str(),
                n.path,
                n.span[0],
                n.span[1]
            );
            if !push_line(&mut out, &line, budget_chars) {
                truncated = true;
                break;
            }
        }
    }

    if !truncated && !cross_lines.is_empty() {
        append_section_header(&mut out, "Cross-unit neighbours:\n", budget_chars, &mut truncated);
        for line in &cross_lines {
            if !push_line(&mut out, line, budget_chars) {
                truncated = true;
                break;
            }
        }
    }

    if !truncated && !test_nodes.is_empty() {
        append_section_header(&mut out, "Tests touching this unit:\n", budget_chars, &mut truncated);
        for n in test_nodes.values() {
            let line = format!("- test {} {}:{}\n", qualified_label(n), n.path, n.span[0]);
            if !push_line(&mut out, &line, budget_chars) {
                truncated = true;
                break;
            }
        }
    }

    if truncated {
        let marker = "... (unit map truncated to budget)\n";
        while out.len() + marker.len() > budget_chars {
            if let Some(pos) = out[..out.len().saturating_sub(1)].rfind('\n') {
                out.truncate(pos + 1);
            } else {
                out.clear();
                break;
            }
        }
        if out.len() + marker.len() <= budget_chars {
            out.push_str(marker);
        }
    }

    Ok(out)
}

fn append_section_header(out: &mut String, header: &str, budget: usize, truncated: &mut bool) {
    if !push_line(out, header, budget) {
        *truncated = true;
    }
}

fn push_line(out: &mut String, line: &str, budget: usize) -> bool {
    if out.len() + line.len() > budget {
        return false;
    }
    out.push_str(line);
    true
}

fn qualified_label(node: &Node) -> String {
    if let Some((_, qual)) = node.id.split_once('#') {
        qual.to_string()
    } else {
        node.name.clone()
    }
}

fn format_own_node(n: &Node) -> String {
    let mut line = format!(
        "- {} {} [{}] {}:{}-{}",
        n.kind.as_str(),
        qualified_label(n),
        n.status.as_str(),
        n.path,
        n.span[0],
        n.span[1]
    );
    if !n.sig.is_empty() {
        line.push_str(" :: ");
        line.push_str(&clip_inline(&n.sig, 120));
    }
    if let Some(doc) = &n.doc {
        line.push_str(" — ");
        line.push_str(&clip_inline(doc, 80));
    }
    line.push('\n');
    line
}

fn format_cross_outgoing(unit_id: &str, edge: &Edge, target: &Node) -> String {
    let other = target.owner.as_deref().unwrap_or("shared");
    let sig = if target.sig.is_empty() {
        String::new()
    } else {
        format!(" :: {}", clip_inline(&target.sig, 100))
    };
    format!(
        "- {unit_id} ▸ {} {other}:{}{} [{}] {}:{}\n",
        edge.kind.as_str(),
        qualified_label(target),
        sig,
        target.status.as_str(),
        target.path,
        target.span[0]
    )
}

fn format_cross_incoming(unit_id: &str, my_target: &str, edge: &Edge, src: &Node) -> String {
    let other = src.owner.as_deref().unwrap_or("shared");
    format!(
        "- {other}:{} ▸ {} {unit_id}:{my_target} [{}] {}:{}\n",
        qualified_label(src),
        edge.kind.as_str(),
        src.status.as_str(),
        src.path,
        src.span[0]
    )
}

fn format_dep_symbol(dep_unit: &str, n: &Node) -> String {
    let sig = if n.sig.is_empty() {
        String::new()
    } else {
        format!(" :: {}", clip_inline(&n.sig, 100))
    };
    format!(
        "- dep {dep_unit}:{}{} [{}] {}:{}\n",
        qualified_label(n),
        sig,
        n.status.as_str(),
        n.path,
        n.span[0]
    )
}

fn clip_inline(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{build, seed_from_plan};
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;
    use tm_core::{ContractFile, PagePlan, Plan, ProjectKind};

    fn temp_repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tm-flow-render-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tinydb")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        dir
    }

    #[test]
    fn unit_map_shows_cross_unit_neighbours_line_only_contracts_tests_and_respects_budget() {
        let dir = temp_repo("map");
        let contract = "from typing import Protocol\nclass StorageProtocol(Protocol):\n    def save(self) -> None: ...\n";
        let test_contract = "from tinydb.executor import Executor\ndef test_exec_runs():\n    e = Executor()\n    e.execute()\n";
        std::fs::write(dir.join("tinydb/contract.py"), contract).unwrap();
        std::fs::write(dir.join("tests/test_acceptance.py"), test_contract).unwrap();
        std::fs::write(
            dir.join("tinydb/btree.py"),
            "class BTree:\n    def insert(self, key: int) -> None:\n        pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("tinydb/executor.py"),
            "from tinydb.contract import StorageProtocol\nfrom tinydb.btree import BTree\n\nclass Executor(StorageProtocol):\n    def __init__(self, bt: BTree) -> None:\n        self.bt = bt\n    def execute(self) -> None:\n        self.bt.insert(1)\n",
        )
        .unwrap();

        let plan = Plan {
            kind: ProjectKind::Cli,
            contracts: vec![
                ContractFile { path: "tinydb/contract.py".into(), content: contract.into(), phase: 0 },
                ContractFile { path: "tests/test_acceptance.py".into(), content: test_contract.into(), phase: 0 },
            ],
            pages: vec![
                PagePlan { id: "btree".into(), owned_glob: "tinydb/btree.py".into(), brief: "btree".into(), ..Default::default() },
                PagePlan { id: "executor".into(), owned_glob: "tinydb/executor.py".into(), brief: "exec".into(), depends_on: vec!["btree".into()], ..Default::default() },
            ],
            ..Default::default()
        };

        build(&dir).unwrap();
        seed_from_plan(&dir, &plan).unwrap();

        let map = unit_map(
            &dir,
            "executor",
            &["tinydb/executor.py".into()],
            &["tinydb/contract.py".into(), "tests/test_acceptance.py".into()],
            DEFAULT_UNIT_MAP_BUDGET,
        )
        .unwrap();

        assert!(map.contains("## Flow unit map (executor)"), "{map}");
        assert!(map.contains("Executor.execute"), "{map}");
        // Contract referenced by line ref only (no full signature body duplicated)
        assert!(map.contains("contract StorageProtocol"), "{map}");
        assert!(map.contains("tinydb/contract.py:2-3"), "{map}");
        assert!(!map.contains("def save(self) -> None"), "must not duplicate contract text: {map}");
        // Cross-unit neighbour from btree unit
        assert!(map.contains("btree:BTree.insert"), "{map}");
        // Test touching this unit
        assert!(map.contains("test_exec_runs"), "{map}");

        // Tight budget is strictly respected
        let small = unit_map(
            &dir,
            "executor",
            &["tinydb/executor.py".into()],
            &["tinydb/contract.py".into(), "tests/test_acceptance.py".into()],
            240,
        )
        .unwrap();
        assert!(small.len() <= 240, "map length {} exceeded budget 240:\n{small}", small.len());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
