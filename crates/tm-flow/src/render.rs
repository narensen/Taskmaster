//! Terse prompt rendering from `.flow`: per-unit maps within strict character budgets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::model::{Edge, EdgeKind, Node, NodeKind, NodeStatus, normalize_rel_path};
use crate::query::{EdgeView, FlowQuery};
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
    // Separate production symbols from owned test-file symbols so unit tests never drown out
    // the unit's own API, referenced contracts, or cross-unit neighbours.
    let my_unit_node = crate::model::unit_node_id(unit_id);
    let mut unit_node_ids: BTreeSet<&str> = BTreeSet::new();
    let mut prod_nodes: Vec<&Node> = Vec::new();
    let mut own_test_nodes: Vec<&Node> = Vec::new();
    let mut planned_prod_files: Vec<&Node> = Vec::new();
    let mut planned_test_files: Vec<&Node> = Vec::new();
    for n in &all_nodes {
        let belongs = n.owner.as_deref() == Some(unit_id)
            || (!n.path.is_empty() && owned_file_set.contains(&n.path));
        if belongs && n.kind != NodeKind::Unit {
            unit_node_ids.insert(n.id.as_str());
            if n.kind == NodeKind::File {
                if n.status == NodeStatus::Planned {
                    if tm_core::is_test_path(&n.path) {
                        planned_test_files.push(n);
                    } else {
                        planned_prod_files.push(n);
                    }
                }
            } else if tm_core::is_test_path(&n.path) {
                own_test_nodes.push(n);
            } else {
                prod_nodes.push(n);
            }
        }
    }
    let mut unit_nodes: Vec<&Node> = if prod_nodes.is_empty() && planned_prod_files.is_empty() {
        let mut combined = planned_test_files;
        combined.extend(std::mem::take(&mut own_test_nodes));
        combined
    } else {
        let mut combined = planned_prod_files;
        combined.extend(prod_nodes);
        combined
    };
    // Planned files first, then exported symbols before private helpers, preserving path/line order.
    unit_nodes.sort_by(|a, b| {
        let a_file = a.kind == NodeKind::File;
        let b_file = b.kind == NodeKind::File;
        b_file
            .cmp(&a_file)
            .then_with(|| (!a.exported).cmp(&(!b.exported)))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.span[0].cmp(&b.span[0]))
            .then_with(|| a.id.cmp(&b.id))
    });

    let is_contract_node = |n: &Node| -> bool {
        n.status == NodeStatus::Contract
            || (!n.path.is_empty() && contract_set.contains(&n.path) && !tm_core::is_test_path(&n.path))
    };

    // 2. Referenced contract symbols (line refs only — never duplicate full contract text).
    let mut contract_refs: BTreeMap<&str, &Node> = BTreeMap::new();
    // 3. 1-hop cross-unit neighbours (high-confidence or planned).
    let mut cross_lines: Vec<String> = Vec::new();
    let mut seen_cross: BTreeSet<String> = BTreeSet::new();
    // 4. Tests touching this unit (both cross-file tests and the unit's own top-level test functions).
    let mut test_nodes: BTreeMap<&str, &Node> = BTreeMap::new();
    for n in own_test_nodes {
        if n.parent.is_none()
            && !n.name.starts_with('_')
            && (n.name.starts_with("test_") || n.name.starts_with("Test"))
        {
            test_nodes.insert(n.id.as_str(), n);
        }
    }

    for edge in &all_edges {
        if edge.to == my_unit_node
            && edge.kind == EdgeKind::ContractOf
            && let Some(src) = node_by_id.get(edge.from.as_str()).copied()
            && is_contract_node(src)
            && src.kind != NodeKind::File
        {
            contract_refs.insert(src.id.as_str(), src);
            continue;
        }
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
                && (src.kind != NodeKind::File || src.status == NodeStatus::Planned)
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
    // also include exported built/planned non-test symbols (or planned files) from upstream dependencies.
    let mut seen_cross_nodes: BTreeSet<String> = all_edges
        .iter()
        .filter(|e| unit_node_ids.contains(e.from.as_str()) && !unit_node_ids.contains(e.to.as_str()))
        .map(|e| e.to.clone())
        .collect();
    if let Some(seed) = &plan_seed
        && let Some(u) = seed.units.iter().find(|u| u.id == unit_id)
    {
        for dep_unit in &u.depends_on {
            let mut added_sym = false;
            for n in all_nodes.iter().filter(|n| {
                n.owner.as_deref() == Some(dep_unit.as_str())
                    && n.kind != NodeKind::File
                    && n.kind != NodeKind::Unit
                    && n.exported
                    && !tm_core::is_test_path(&n.path)
            }) {
                if seen_cross_nodes.insert(n.id.clone()) && seen_cross.insert(format!("dep:{}", n.id)) {
                    cross_lines.push(format_dep_symbol(dep_unit, n));
                    added_sym = true;
                }
            }
            if !added_sym {
                for n in all_nodes.iter().filter(|n| {
                    n.owner.as_deref() == Some(dep_unit.as_str())
                        && n.kind == NodeKind::File
                        && n.status == NodeStatus::Planned
                        && !tm_core::is_test_path(&n.path)
                }) {
                    if seen_cross_nodes.insert(n.id.clone()) && seen_cross.insert(format!("dep:{}", n.id)) {
                        cross_lines.push(format_dep_symbol(dep_unit, n));
                    }
                }
            }
        }
    }

    // Deduplicate contract method sub-refs when their parent contract class/Protocol is already listed.
    let parent_contract_ids: BTreeSet<&str> = contract_refs
        .values()
        .filter(|n| n.parent.is_none())
        .map(|n| n.id.as_str())
        .collect();
    let contract_list: Vec<&Node> = contract_refs
        .values()
        .copied()
        .filter(|n| n.parent.as_deref().is_none_or(|p| !parent_contract_ids.contains(p)))
        .collect();

    if unit_nodes.is_empty()
        && contract_list.is_empty()
        && cross_lines.is_empty()
        && test_nodes.is_empty()
    {
        return Ok(String::new());
    }

    let header = format!("## Flow unit map ({unit_id})\n");
    let own_lines: Vec<String> = unit_nodes.iter().map(|n| format_own_node(n)).collect();
    let contract_lines: Vec<String> = contract_list
        .into_iter()
        .map(|n| {
            format!(
                "- contract {} ({}) {}:{}-{}\n",
                qualified_label(n),
                n.kind.as_str(),
                n.path,
                n.span[0],
                n.span[1]
            )
        })
        .collect();
    let test_lines: Vec<String> = test_nodes
        .values()
        .map(|n| format!("- test {} {}:{}\n", qualified_label(n), n.path, n.span[0]))
        .collect();

    let sections: [(&str, &[String], usize); 4] = [
        ("Your symbols:\n", &own_lines, 40),
        ("Referenced contracts (line refs only):\n", &contract_lines, 20),
        ("Cross-unit neighbours:\n", &cross_lines, 25),
        ("Tests touching this unit:\n", &test_lines, 15),
    ];

    // Water-fill budget allocation across non-empty sections so a large symbol list never
    // starves referenced contracts, cross-unit neighbours, or tests.
    let avail = budget_chars.saturating_sub(header.len());
    let needs: [usize; 4] = std::array::from_fn(|i| {
        let (title, lines, _) = sections[i];
        if lines.is_empty() {
            0
        } else {
            title.len() + lines.iter().map(String::len).sum::<usize>()
        }
    });
    let total_need: usize = needs.iter().sum();

    let caps: [usize; 4] = if total_need <= avail {
        needs
    } else {
        allocate_section_budgets(&needs, &[40, 20, 25, 15], avail)
    };

    let mut out = header;
    for (idx, (title, lines, _)) in sections.iter().enumerate() {
        if lines.is_empty() || caps[idx] == 0 {
            continue;
        }
        let sec_budget = caps[idx].min(budget_chars.saturating_sub(out.len()));
        if sec_budget < title.len() {
            continue;
        }
        let mut sec = String::with_capacity(sec_budget);
        sec.push_str(title);
        let mut written = 0usize;
        for line in *lines {
            let remaining_after = lines.len() - written - 1;
            let reserve = if remaining_after > 0 { 24 } else { 0 };
            if sec.len() + line.len() + reserve > sec_budget {
                break;
            }
            sec.push_str(line);
            written += 1;
        }
        if written < lines.len() {
            let omitted = lines.len() - written;
            let note = format!("... (+{omitted} more)\n");
            if sec.len() + note.len() <= sec_budget {
                sec.push_str(&note);
            }
        }
        if written > 0 {
            out.push_str(&sec);
        }
    }

    if out.len() > budget_chars {
        while out.len() > budget_chars {
            if let Some(pos) = out[..out.len().saturating_sub(1)].rfind('\n') {
                out.truncate(pos + 1);
            } else {
                out.truncate(budget_chars);
                break;
            }
        }
    }

    Ok(out)
}

const MAX_PREINJECT_TARGETS: usize = 16;

/// Pre-inject concise `flow_query` results (`node` + `1-hop neighbors` + `callers`) when a unit
/// edits existing code (brownfield runs, repairs, or later-wave units referencing already-built
/// symbols). Returns an empty string when no existing built symbols apply (e.g. wave-1 greenfield).
/// Output is strictly capped at `budget_chars`.
pub fn preinjected_context(
    tree: &Path,
    _unit_id: &str,
    owned_files: &[String],
    task_text: &str,
    budget_chars: usize,
) -> anyhow::Result<String> {
    if budget_chars < 96 {
        return Ok(String::new());
    }
    if !owned_files.is_empty() {
        let _ = ensure_fresh(tree, owned_files);
    }
    let db = FlowDb::open_or_rebuild(tree)?;
    let all_nodes = db.all_nodes()?;
    let node_by_id: BTreeMap<&str, &Node> = all_nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let owned_file_set: BTreeSet<String> = owned_files.iter().map(|p| normalize_rel_path(p)).collect();

    // 1. Existing built symbols in this unit's owned files.
    let mut owned_syms: Vec<&Node> = all_nodes
        .iter()
        .filter(|n| {
            n.status == NodeStatus::Built
                && n.kind != NodeKind::File
                && n.kind != NodeKind::Unit
                && !n.path.is_empty()
                && owned_file_set.contains(&n.path)
        })
        .collect();
    owned_syms.sort_by(|a, b| {
        let a_test = tm_core::is_test_path(&a.path);
        let b_test = tm_core::is_test_path(&b.path);
        a_test
            .cmp(&b_test)
            .then_with(|| (!a.exported).cmp(&(!b.exported)))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.span[0].cmp(&b.span[0]))
            .then_with(|| a.id.cmp(&b.id))
    });

    // 2. Existing built non-test symbols explicitly mentioned in the unit's task/error text.
    let mut bare_name_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &all_nodes {
        if n.status == NodeStatus::Built
            && n.kind != NodeKind::File
            && n.kind != NodeKind::Unit
            && !tm_core::is_test_path(&n.path)
        {
            *bare_name_counts.entry(n.name.as_str()).or_insert(0) += 1;
        }
    }
    let mut mentioned_syms: Vec<&Node> = if task_text.trim().is_empty() {
        Vec::new()
    } else {
        all_nodes
            .iter()
            .filter(|n| {
                n.status == NodeStatus::Built
                    && n.kind != NodeKind::File
                    && n.kind != NodeKind::Unit
                    && !tm_core::is_test_path(&n.path)
                    && !owned_file_set.contains(&n.path)
                    && symbol_mentioned_in_text(n, task_text, bare_name_counts.get(n.name.as_str()).copied().unwrap_or(0))
            })
            .collect()
    };
    mentioned_syms.sort_by(|a, b| {
        a.parent
            .is_some()
            .cmp(&b.parent.is_some())
            .then_with(|| (!a.exported).cmp(&(!b.exported)))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.span[0].cmp(&b.span[0]))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut targets: Vec<&Node> = Vec::new();
    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
    for n in owned_syms.into_iter().chain(mentioned_syms) {
        if targets.len() >= MAX_PREINJECT_TARGETS {
            break;
        }
        if seen_ids.insert(n.id.as_str()) {
            targets.push(n);
        }
    }
    if targets.is_empty() {
        return Ok(String::new());
    }

    let q = FlowQuery::open(tree)?;
    let header = "## Pre-injected flow context (existing code)\n";
    let mut out = String::with_capacity(budget_chars.min(4096));
    out.push_str(header);
    let mut written = 0usize;

    for target in &targets {
        let Some(detail) = q.node(&target.id)? else {
            continue;
        };
        let block = format_preinjected_node(&detail.node, &detail.incoming, &detail.outgoing, &node_by_id);
        let remaining_after = targets.len().saturating_sub(written + 1);
        let reserve = if remaining_after > 0 { 28 } else { 0 };
        if out.len() + block.len() + reserve > budget_chars {
            // Try fitting just the primary node line if nothing has been written yet.
            if written == 0
                && let Some(first_line) = block.lines().next()
            {
                let mut trimmed = format!("{first_line}\n");
                if out.len() + trimmed.len() > budget_chars {
                    let max_line = budget_chars.saturating_sub(out.len() + 1);
                    trimmed = format!("{}\n", clip_inline(first_line, max_line));
                }
                if out.len() + trimmed.len() <= budget_chars {
                    out.push_str(&trimmed);
                    written += 1;
                }
            }
            break;
        }
        out.push_str(&block);
        written += 1;
    }

    if written == 0 {
        return Ok(String::new());
    }
    if written < targets.len() {
        let omitted = targets.len() - written;
        let note = format!("... (+{omitted} more symbols)\n");
        if out.len() + note.len() <= budget_chars {
            out.push_str(&note);
        }
    }
    if out.len() > budget_chars {
        while out.len() > budget_chars {
            if let Some(pos) = out[..out.len().saturating_sub(1)].rfind('\n') {
                out.truncate(pos + 1);
            } else {
                let mut end = budget_chars;
                while end > 0 && !out.is_char_boundary(end) {
                    end -= 1;
                }
                out.truncate(end);
                break;
            }
        }
    }
    Ok(out)
}

fn format_preinjected_node(
    node: &Node,
    incoming: &[EdgeView],
    outgoing: &[EdgeView],
    node_by_id: &BTreeMap<&str, &Node>,
) -> String {
    let owner = node.owner.as_deref().unwrap_or("-");
    let loc = if node.span == [0, 0] {
        node.path.clone()
    } else {
        format!("{}:{}-{}", node.path, node.span[0], node.span[1])
    };
    let mut block = format!(
        "- node {} ({}, {}, owner: {}) {}",
        node.id,
        node.kind.as_str(),
        node.status.as_str(),
        owner,
        loc,
    );
    if !node.sig.is_empty() {
        block.push_str(" :: ");
        block.push_str(&clip_inline(&node.sig, 110));
    }
    block.push('\n');

    let mut callers: Vec<String> = Vec::new();
    let mut seen_callers: BTreeSet<&str> = BTreeSet::new();
    let mut neighbors: Vec<String> = Vec::new();
    let mut seen_neighbors: BTreeSet<String> = BTreeSet::new();

    for e in incoming {
        if e.kind == EdgeKind::Owns {
            continue;
        }
        let peer_desc = format_peer_brief(e, node_by_id);
        if e.kind == EdgeKind::Calls && seen_callers.insert(e.peer_id.as_str()) && callers.len() < 8 {
            callers.push(peer_desc.clone());
        }
        if seen_neighbors.insert(format!("in:{}:{}", e.kind.as_str(), e.peer_id)) && neighbors.len() < 10 {
            neighbors.push(format!("in:{} {peer_desc}", e.kind.as_str()));
        }
    }
    for e in outgoing {
        if e.kind == EdgeKind::Owns {
            continue;
        }
        let peer_desc = format_peer_brief(e, node_by_id);
        if seen_neighbors.insert(format!("out:{}:{}", e.kind.as_str(), e.peer_id)) && neighbors.len() < 10 {
            neighbors.push(format!("out:{} {peer_desc}", e.kind.as_str()));
        }
    }

    if !callers.is_empty() {
        block.push_str("  callers: ");
        block.push_str(&callers.join(", "));
        block.push('\n');
    }
    if !neighbors.is_empty() {
        block.push_str("  neighbors(1-hop): ");
        block.push_str(&neighbors.join(", "));
        block.push('\n');
    }
    block
}

fn format_peer_brief(e: &EdgeView, node_by_id: &BTreeMap<&str, &Node>) -> String {
    let owner = e.peer_owner.as_deref().unwrap_or("-");
    let label = node_by_id
        .get(e.peer_id.as_str())
        .map(|n| qualified_label(n))
        .filter(|s| !s.is_empty())
        .or_else(|| (!e.peer_name.is_empty()).then(|| e.peer_name.clone()))
        .unwrap_or_else(|| e.peer_id.clone());
    let loc = node_by_id
        .get(e.peer_id.as_str())
        .map(|n| format_loc(&n.path, n.span[0]))
        .filter(|s| !s.is_empty())
        .or_else(|| (!e.peer_path.is_empty()).then(|| e.peer_path.clone()));
    match loc {
        Some(loc) => format!("{owner}:{label} ({loc})"),
        None => format!("{owner}:{label}"),
    }
}

fn symbol_mentioned_in_text(node: &Node, text: &str, bare_count: usize) -> bool {
    if text.contains(&node.id) {
        return true;
    }
    let qual = qualified_label(node);
    if qual.contains('.') && contains_ident(text, &qual) {
        return true;
    }
    let name = node.name.as_str();
    if is_generic_symbol_name(name) {
        return false;
    }
    let min_len = if node.parent.is_none() { 3 } else { 4 };
    let specific_len = name.len() >= min_len
        || (node.parent.is_none() && name.len() >= 2 && name.chars().next().is_some_and(char::is_uppercase));
    if !specific_len {
        return false;
    }
    if node.parent.is_some() && bare_count > 3 {
        return false;
    }
    contains_ident(text, name)
}

fn is_generic_symbol_name(name: &str) -> bool {
    matches!(
        name,
        "init"
            | "__init__"
            | "__repr__"
            | "__str__"
            | "__eq__"
            | "__len__"
            | "__iter__"
            | "new"
            | "default"
            | "get"
            | "set"
            | "run"
            | "main"
            | "add"
            | "put"
            | "pop"
            | "len"
            | "str"
            | "int"
            | "key"
            | "val"
            | "value"
            | "data"
            | "item"
            | "items"
            | "list"
            | "dict"
            | "self"
            | "cls"
            | "test"
            | "open"
            | "close"
            | "read"
            | "write"
            | "load"
            | "save"
            | "from"
            | "into"
            | "type"
            | "name"
            | "path"
            | "file"
            | "node"
            | "error"
            | "result"
            | "config"
            | "state"
    )
}

fn contains_ident(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut offset = 0;
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

fn allocate_section_budgets(needs: &[usize; 4], weights: &[usize; 4], total: usize) -> [usize; 4] {
    let mut caps = [0usize; 4];
    let mut active = [false; 4];
    for i in 0..4 {
        active[i] = needs[i] > 0;
    }
    let mut left = total;
    loop {
        let weight_sum: usize = (0..4).filter(|&i| active[i]).map(|i| weights[i]).sum();
        if weight_sum == 0 || left == 0 {
            break;
        }
        let mut settled_any = false;
        let mut distributed = 0usize;
        for i in 0..4 {
            if !active[i] {
                continue;
            }
            let share = left.saturating_mul(weights[i]) / weight_sum;
            let remaining_need = needs[i].saturating_sub(caps[i]);
            if share >= remaining_need {
                caps[i] += remaining_need;
                distributed += remaining_need;
                active[i] = false;
                settled_any = true;
            }
        }
        if settled_any {
            left = left.saturating_sub(distributed);
            continue;
        }
        // Distribute remaining budget proportionally among active sections.
        for i in 0..4 {
            if !active[i] {
                continue;
            }
            let share = (left.saturating_mul(weights[i]) / weight_sum).min(needs[i] - caps[i]);
            caps[i] += share;
            distributed += share;
        }
        left = left.saturating_sub(distributed);
        // Hand out any rounding remainder to the first still-unsatisfied active section.
        for i in 0..4 {
            if left == 0 {
                break;
            }
            if active[i] && caps[i] < needs[i] {
                let extra = left.min(needs[i] - caps[i]);
                caps[i] += extra;
                left -= extra;
            }
        }
        break;
    }
    caps
}

fn qualified_label(node: &Node) -> String {
    if let Some((_, qual)) = node.id.split_once('#') {
        qual.to_string()
    } else {
        node.name.clone()
    }
}

fn format_own_node(n: &Node) -> String {
    if n.kind == NodeKind::File {
        return format!("- file {} [{}]\n", n.path, n.status.as_str());
    }
    let loc = if n.span == [0, 0] {
        n.path.clone()
    } else {
        format!("{}:{}-{}", n.path, n.span[0], n.span[1])
    };
    let mut line = format!(
        "- {} {} [{}] {}",
        n.kind.as_str(),
        qualified_label(n),
        n.status.as_str(),
        loc,
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

fn format_loc(path: &str, line: u32) -> String {
    if line == 0 {
        path.to_string()
    } else {
        format!("{path}:{line}")
    }
}

fn format_cross_outgoing(unit_id: &str, edge: &Edge, target: &Node) -> String {
    let other = target.owner.as_deref().unwrap_or("shared");
    let sig = if target.sig.is_empty() || target.kind == NodeKind::File {
        String::new()
    } else {
        format!(" :: {}", clip_inline(&target.sig, 100))
    };
    format!(
        "- {unit_id} ▸ {} {other}:{}{} [{}] {}\n",
        edge.kind.as_str(),
        qualified_label(target),
        sig,
        target.status.as_str(),
        format_loc(&target.path, target.span[0]),
    )
}

fn format_cross_incoming(unit_id: &str, my_target: &str, edge: &Edge, src: &Node) -> String {
    let other = src.owner.as_deref().unwrap_or("shared");
    format!(
        "- {other}:{} ▸ {} {unit_id}:{my_target} [{}] {}\n",
        qualified_label(src),
        edge.kind.as_str(),
        src.status.as_str(),
        format_loc(&src.path, src.span[0]),
    )
}

fn format_dep_symbol(dep_unit: &str, n: &Node) -> String {
    let sig = if n.sig.is_empty() || n.kind == NodeKind::File {
        String::new()
    } else {
        format!(" :: {}", clip_inline(&n.sig, 100))
    };
    format!(
        "- dep {dep_unit}:{}{} [{}] {}\n",
        qualified_label(n),
        sig,
        n.status.as_str(),
        format_loc(&n.path, n.span[0]),
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

    #[test]
    fn preinjected_context_includes_node_neighbors_and_callers_within_budget() {
        let dir = temp_repo("preinject");
        std::fs::write(
            dir.join("tinydb/codec.py"),
            "def encode_row(row: dict) -> bytes:\n    return str(row).encode()\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("tinydb/store.py"),
            "from tinydb.codec import encode_row\n\nclass StoreEngine:\n    def persist_row(self, row: dict) -> bytes:\n        return encode_row(row)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("tinydb/api.py"),
            "from tinydb.store import StoreEngine\n\ndef handle_insert(engine: StoreEngine, row: dict) -> bytes:\n    return engine.persist_row(row)\n",
        )
        .unwrap();

        let plan = Plan {
            kind: ProjectKind::Cli,
            pages: vec![
                PagePlan { id: "codec".into(), owned_glob: "tinydb/codec.py".into(), brief: "codec".into(), ..Default::default() },
                PagePlan { id: "store".into(), owned_glob: "tinydb/store.py".into(), brief: "store".into(), ..Default::default() },
                PagePlan { id: "api".into(), owned_glob: "tinydb/api.py".into(), brief: "api".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        build(&dir).unwrap();
        seed_from_plan(&dir, &plan).unwrap();

        // Brownfield / repair case: owned file exists.
        let pre = preinjected_context(
            &dir,
            "store",
            &["tinydb/store.py".into()],
            "Fix StoreEngine.persist_row handling",
            2000,
        )
        .unwrap();
        assert!(pre.contains("## Pre-injected flow context (existing code)"), "{pre}");
        assert!(pre.contains("node py:tinydb/store.py#StoreEngine.persist_row"), "{pre}");
        assert!(pre.contains("callers:"), "expected callers line:\n{pre}");
        assert!(pre.contains("handle_insert"), "expected caller handle_insert:\n{pre}");
        assert!(pre.contains("neighbors(1-hop):"), "expected 1-hop neighbors:\n{pre}");
        assert!(pre.contains("encode_row"), "expected outgoing neighbor encode_row:\n{pre}");
        assert!(pre.len() <= 2000, "len {} exceeded budget 2000", pre.len());

        // Later-wave case: owned file does not exist yet, task mentions existing built symbol.
        let wave2 = preinjected_context(
            &dir,
            "cli",
            &[],
            "Wire CLI commands to StoreEngine.persist_row and encode_row",
            2000,
        )
        .unwrap();
        assert!(wave2.contains("node py:tinydb/store.py#StoreEngine.persist_row"), "{wave2}");
        assert!(wave2.contains("node py:tinydb/codec.py#encode_row"), "{wave2}");
        assert!(wave2.contains("callers:"), "{wave2}");

        // Tight budget is strictly respected.
        let tight = preinjected_context(
            &dir,
            "store",
            &["tinydb/store.py".into()],
            "Fix StoreEngine",
            260,
        )
        .unwrap();
        assert!(!tight.is_empty());
        assert!(tight.len() <= 260, "tight len {} exceeded 260:\n{tight}", tight.len());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
