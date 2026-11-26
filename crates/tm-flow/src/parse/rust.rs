//! Tree-sitter Rust parser for `.flow`.

use std::collections::HashMap;

use tree_sitter::{Node as TsNode, Parser};

use crate::model::{EdgeKind, Node, NodeKind, NodeStatus, body_hash, file_node_id, sig_hash};
use crate::parse::{
    NameCounter, ParsedFile, RawCall, RawImport, RawRelation, clean_doc_line, clean_signature,
    make_file_node,
};

pub fn parse(path: &str, source: &str, blob: &str) -> ParsedFile {
    let mut parser = Parser::new();
    let _ = parser.set_language(&tree_sitter_rust::LANGUAGE.into());
    let file_id = file_node_id("rust", path);
    let Some(tree) = parser.parse(source, None) else {
        return crate::parse::fallback::parse(path, "rust", source, blob);
    };

    let root = tree.root_node();
    let file_doc = leading_inner_doc(root, source);
    let mut nodes = vec![make_file_node("rust", path, source, file_doc)];
    let mut imports = Vec::new();
    let mut relations = Vec::new();
    let mut calls = Vec::new();
    let mut counter = NameCounter::default();
    let mut local_types: HashMap<String, String> = HashMap::new();

    let mut ctx = RustCtx {
        path,
        source,
        file_id: &file_id,
        nodes: &mut nodes,
        imports: &mut imports,
        relations: &mut relations,
        calls: &mut calls,
        counter: &mut counter,
        local_types: &mut local_types,
    };

    // Pass 1: types, traits, functions, consts, imports, mods so impl blocks find local type parents.
    let mut impl_nodes = Vec::new();
    let mut cur = root.walk();
    for child in root.children(&mut cur) {
        if child.kind() == "impl_item" {
            impl_nodes.push(child);
        } else {
            ctx.visit_item(child);
        }
    }
    // Pass 2: impl blocks (inherent and trait impls).
    for imp in impl_nodes {
        ctx.extract_impl(imp);
    }

    ParsedFile {
        path: path.to_string(),
        lang: "rust".to_string(),
        blob: blob.to_string(),
        file_id,
        nodes,
        imports,
        relations,
        calls,
    }
}

struct RustCtx<'a> {
    path: &'a str,
    source: &'a str,
    file_id: &'a str,
    nodes: &'a mut Vec<Node>,
    imports: &'a mut Vec<RawImport>,
    relations: &'a mut Vec<RawRelation>,
    calls: &'a mut Vec<RawCall>,
    counter: &'a mut NameCounter,
    local_types: &'a mut HashMap<String, String>,
}

impl<'a> RustCtx<'a> {
    fn text(&self, node: TsNode) -> &'a str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    fn visit_item(&mut self, node: TsNode) {
        match node.kind() {
            "use_declaration" => self.extract_use(node),
            "mod_item" => self.extract_mod(node),
            "struct_item" | "enum_item" | "union_item" | "type_item" => {
                self.extract_type_item(node);
            }
            "trait_item" => self.extract_trait(node),
            "const_item" | "static_item" => self.extract_const(node),
            "function_item" => self.extract_function(node, None, None),
            _ => {}
        }
    }

    fn extract_mod(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        if node.child_by_field_name("body").is_none() {
            // External module declaration: `mod foo;`
            self.imports.push(RawImport {
                module: format!("self::{name}"),
                relative_dots: 0,
                names: vec![],
                module_alias: Some(name),
                wildcard: false,
            });
        } else if let Some(body) = node.child_by_field_name("body") {
            let mut cur = body.walk();
            let mut impls = Vec::new();
            for child in body.children(&mut cur) {
                if child.kind() == "impl_item" {
                    impls.push(child);
                } else {
                    self.visit_item(child);
                }
            }
            for imp in impls {
                self.extract_impl(imp);
            }
        }
    }

    fn extract_use(&mut self, node: TsNode) {
        let Some(arg) = node.child_by_field_name("argument") else { return };
        let mut entries = Vec::new();
        flatten_use_tree(arg, self.source, "", &mut entries);
        for (path_str, alias, wildcard) in entries {
            let parts: Vec<&str> = path_str.split("::").filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                continue;
            }
            if wildcard {
                self.imports.push(RawImport {
                    module: parts.join("::"),
                    relative_dots: 0,
                    names: vec![],
                    module_alias: None,
                    wildcard: true,
                });
            } else if parts.len() >= 2 {
                let remote = parts[parts.len() - 1].to_string();
                let local = alias.unwrap_or_else(|| remote.clone());
                let module = parts[..parts.len() - 1].join("::");
                self.imports.push(RawImport {
                    module,
                    relative_dots: 0,
                    names: vec![(remote, local)],
                    module_alias: None,
                    wildcard: false,
                });
            } else {
                let module = parts[0].to_string();
                self.imports.push(RawImport {
                    module: module.clone(),
                    relative_dots: 0,
                    names: vec![],
                    module_alias: alias.or(Some(module)),
                    wildcard: false,
                });
            }
        }
    }

    fn extract_type_item(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id("rust", self.path, &name);
        let body = node.child_by_field_name("body");
        let sig = if body.is_some() {
            header_before(self.source, node, body)
        } else {
            clean_signature(self.text(node).trim_end_matches(';'))
        };
        let doc = prev_rust_doc(node, self.source);
        let body_text = self.text(node);
        let exported = has_pub(node);
        self.local_types.entry(name.clone()).or_insert_with(|| id.clone());

        self.nodes.push(Node {
            id,
            kind: NodeKind::Type,
            name,
            path: self.path.to_string(),
            parent: None,
            span: span_of(node),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });
    }

    fn extract_trait(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id("rust", self.path, &name);
        let body = node.child_by_field_name("body");
        let sig = header_before(self.source, node, body);
        let doc = prev_rust_doc(node, self.source);
        let body_text = self.text(node);
        let exported = has_pub(node);
        self.local_types.entry(name.clone()).or_insert_with(|| id.clone());

        // Supertraits in `trait_bounds`
        if let Some(bounds) = node.child_by_field_name("bounds") {
            let mut bcur = bounds.walk();
            for b in bounds.named_children(&mut bcur) {
                let target = rust_type_ident(self.text(b));
                if !target.is_empty() && !matches!(target.as_str(), "Send" | "Sync" | "Sized" | "Clone" | "Debug" | "Copy") {
                    self.relations.push(RawRelation {
                        from_id: id.clone(),
                        target,
                        kind: EdgeKind::Inherits,
                    });
                }
            }
        }

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Type,
            name: name.clone(),
            path: self.path.to_string(),
            parent: None,
            span: span_of(node),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        if let Some(body_node) = body {
            let mut cur = body_node.walk();
            for member in body_node.named_children(&mut cur) {
                match member.kind() {
                    "function_item" => self.extract_function(member, Some(&id), Some(&name)),
                    "function_signature_item" => {
                        if let Some(mname) = member.child_by_field_name("name") {
                            let mname = self.text(mname).trim().to_string();
                            if !mname.is_empty() {
                                let qual = format!("{name}.{mname}");
                                let mid = self.counter.next_id("rust", self.path, &qual);
                                let msig = clean_signature(self.text(member).trim_end_matches(';'));
                                let mdoc = prev_rust_doc(member, self.source);
                                self.nodes.push(Node {
                                    id: mid,
                                    kind: NodeKind::Method,
                                    name: mname,
                                    path: self.path.to_string(),
                                    parent: Some(id.clone()),
                                    span: span_of(member),
                                    sig_hash: sig_hash(&msig),
                                    body_hash: body_hash(self.text(member)),
                                    sig: msig,
                                    exported,
                                    doc: mdoc,
                                    owner: None,
                                    status: NodeStatus::Built,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn extract_const(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id("rust", self.path, &name);
        let body_text = self.text(node);
        let sig = clean_signature(body_text.lines().next().unwrap_or(body_text).trim_end_matches(';'));
        let doc = prev_rust_doc(node, self.source);
        let exported = has_pub(node);

        self.nodes.push(Node {
            id,
            kind: NodeKind::Const,
            name,
            path: self.path.to_string(),
            parent: None,
            span: span_of(node),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });
    }

    fn extract_impl(&mut self, node: TsNode) {
        let Some(ty_node) = node.child_by_field_name("type") else { return };
        let type_name = rust_type_ident(self.text(ty_node));
        if type_name.is_empty() {
            return;
        }
        let parent_id = self.local_types.get(&type_name).cloned();

        if let Some(trait_node) = node.child_by_field_name("trait") {
            let trait_name = rust_type_ident(self.text(trait_node));
            if !trait_name.is_empty() {
                let from_id = parent_id.clone().unwrap_or_else(|| self.file_id.to_string());
                self.relations.push(RawRelation {
                    from_id,
                    target: trait_name,
                    kind: EdgeKind::Implements,
                });
            }
        }

        if let Some(body) = node.child_by_field_name("body") {
            let mut cur = body.walk();
            for member in body.named_children(&mut cur) {
                if member.kind() == "function_item" {
                    self.extract_function(member, parent_id.as_deref(), Some(&type_name));
                }
            }
        }
    }

    fn extract_function(
        &mut self,
        node: TsNode,
        parent_id: Option<&str>,
        enclosing_type: Option<&str>,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let (kind, qual) = match enclosing_type {
            Some(ty) => (NodeKind::Method, format!("{ty}.{name}")),
            None => (NodeKind::Function, name.clone()),
        };
        let id = self.counter.next_id("rust", self.path, &qual);
        let body = node.child_by_field_name("body");
        let sig = header_before(self.source, node, body);
        let doc = prev_rust_doc(node, self.source);
        let body_text = self.text(node);
        let exported = has_pub(node);

        self.nodes.push(Node {
            id: id.clone(),
            kind,
            name,
            path: self.path.to_string(),
            parent: parent_id.map(str::to_string),
            span: span_of(node),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        let mut locals = HashMap::new();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_rust_param_types(params, self.source, &mut locals);
        }
        if let Some(body_node) = body {
            self.collect_calls(body_node, &id, enclosing_type, &mut locals);
        }
    }

    fn collect_calls(
        &mut self,
        node: TsNode,
        from_id: &str,
        enclosing_type: Option<&str>,
        locals: &mut HashMap<String, String>,
    ) {
        if node.kind() == "let_declaration"
            && let Some(pat) = node.child_by_field_name("pattern")
            && pat.kind() == "identifier"
        {
            let vname = self.text(pat).trim();
            if let Some(ty) = node.child_by_field_name("type") {
                let tname = rust_type_ident(self.text(ty));
                if !tname.is_empty() {
                    locals.insert(vname.to_string(), tname);
                }
            } else if let Some(val) = node.child_by_field_name("value")
                && val.kind() == "call_expression"
                && let Some(func) = val.child_by_field_name("function")
                && func.kind() == "scoped_identifier"
                && let Some(scope) = func.child_by_field_name("path")
            {
                let tname = rust_type_ident(self.text(scope));
                if tname.chars().next().is_some_and(char::is_uppercase) {
                    locals.insert(vname.to_string(), tname);
                }
            }
        }

        match node.kind() {
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            let callee = self.text(func).trim().to_string();
                            if !callee.is_empty() {
                                self.calls.push(RawCall {
                                    from_id: from_id.to_string(),
                                    receiver: None,
                                    callee,
                                    enclosing_type: enclosing_type.map(str::to_string),
                                    receiver_type: None,
                                });
                            }
                        }
                        "scoped_identifier" => {
                            let receiver = func.child_by_field_name("path").map(|n| self.text(n).trim().to_string());
                            let callee = func.child_by_field_name("name").map(|n| self.text(n).trim().to_string());
                            if let Some(callee) = callee.filter(|s| !s.is_empty()) {
                                let receiver_type = receiver
                                    .as_deref()
                                    .map(rust_type_ident)
                                    .filter(|t| t.chars().next().is_some_and(char::is_uppercase));
                                self.calls.push(RawCall {
                                    from_id: from_id.to_string(),
                                    receiver,
                                    callee,
                                    enclosing_type: enclosing_type.map(str::to_string),
                                    receiver_type,
                                });
                            }
                        }
                        "field_expression" => {
                            let obj = func.child_by_field_name("value").map(|n| self.text(n).trim().to_string());
                            let field = func.child_by_field_name("field").map(|n| self.text(n).trim().to_string());
                            if let Some(callee) = field.filter(|s| !s.is_empty()) {
                                let receiver_type = obj.as_deref().and_then(|o| locals.get(o).cloned());
                                self.calls.push(RawCall {
                                    from_id: from_id.to_string(),
                                    receiver: obj,
                                    callee,
                                    enclosing_type: enclosing_type.map(str::to_string),
                                    receiver_type,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "method_call_expression" => {
                let obj = node.child_by_field_name("receiver").map(|n| self.text(n).trim().to_string());
                let name = node.child_by_field_name("name").map(|n| self.text(n).trim().to_string());
                if let Some(callee) = name.filter(|s| !s.is_empty()) {
                    let receiver_type = obj.as_deref().and_then(|o| locals.get(o).cloned());
                    self.calls.push(RawCall {
                        from_id: from_id.to_string(),
                        receiver: obj,
                        callee,
                        enclosing_type: enclosing_type.map(str::to_string),
                        receiver_type,
                    });
                }
            }
            _ => {}
        }

        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            if matches!(child.kind(), "impl_item" | "mod_item") {
                continue;
            }
            self.collect_calls(child, from_id, enclosing_type, locals);
        }
    }
}

fn span_of(node: TsNode) -> [u32; 2] {
    [node.start_position().row as u32 + 1, node.end_position().row as u32 + 1]
}

fn has_pub(node: TsNode) -> bool {
    let mut cur = node.walk();
    node.children(&mut cur).any(|c| c.kind() == "visibility_modifier")
}

fn header_before(source: &str, node: TsNode, body: Option<TsNode>) -> String {
    let start = node.start_byte();
    let end = body.map(|b| b.start_byte()).unwrap_or_else(|| node.end_byte());
    let slice = source.get(start..end).unwrap_or("");
    let trimmed = slice.trim().trim_end_matches('{').trim();
    clean_signature(trimmed)
}

fn rust_type_ident(raw: &str) -> String {
    let t = raw.trim().trim_start_matches('&').trim_start_matches("mut ").trim();
    let before_gen = t.split('<').next().unwrap_or(t).trim();
    let last = before_gen.rsplit("::").next().unwrap_or(before_gen).trim();
    last.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn prev_rust_doc(node: TsNode, source: &str) -> Option<String> {
    let mut prev = node.prev_sibling();
    let mut lines = Vec::new();
    while let Some(sib) = prev {
        match sib.kind() {
            "line_comment" => {
                let text = sib.utf8_text(source.as_bytes()).unwrap_or("").trim();
                if text.starts_with("///") {
                    lines.push(text);
                } else {
                    break;
                }
            }
            "attribute_item" => {}
            _ => break,
        }
        prev = sib.prev_sibling();
    }
    lines.reverse();
    lines.into_iter().find_map(clean_doc_line)
}

fn leading_inner_doc(root: TsNode, source: &str) -> Option<String> {
    let mut cur = root.walk();
    for child in root.children(&mut cur) {
        if child.kind() == "line_comment" {
            let text = child.utf8_text(source.as_bytes()).unwrap_or("").trim();
            if text.starts_with("//!") {
                return clean_doc_line(text);
            }
        } else {
            break;
        }
    }
    None
}

fn flatten_use_tree(
    node: TsNode,
    source: &str,
    prefix: &str,
    out: &mut Vec<(String, Option<String>, bool)>,
) {
    let text = node.utf8_text(source.as_bytes()).unwrap_or("").trim();
    let join = |p: &str, part: &str| {
        if p.is_empty() {
            part.to_string()
        } else if part == "self" {
            p.to_string()
        } else {
            format!("{p}::{part}")
        }
    };
    match node.kind() {
        "identifier" | "crate" | "super" | "self" => {
            out.push((join(prefix, text), None, false));
        }
        "scoped_identifier" => {
            out.push((join(prefix, text), None, false));
        }
        "use_as_clause" => {
            let path = node.child_by_field_name("path").map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").trim()).unwrap_or("");
            let alias = node.child_by_field_name("alias").map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").trim().to_string());
            if !path.is_empty() {
                out.push((join(prefix, path), alias, false));
            }
        }
        "use_wildcard" => {
            let base = text.trim_end_matches("::*").trim_end_matches('*').trim();
            let full = if base.is_empty() { prefix.to_string() } else { join(prefix, base) };
            out.push((full, None, true));
        }
        "scoped_use_list" => {
            let sub = node.child_by_field_name("path").map(|n| n.utf8_text(source.as_bytes()).unwrap_or("").trim()).unwrap_or("");
            let next_prefix = join(prefix, sub);
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use_tree(list, source, &next_prefix, out);
            }
        }
        "use_list" => {
            let mut cur = node.walk();
            for child in node.named_children(&mut cur) {
                flatten_use_tree(child, source, prefix, out);
            }
        }
        _ => {}
    }
}

fn collect_rust_param_types(params: TsNode, source: &str, locals: &mut HashMap<String, String>) {
    let mut cur = params.walk();
    for child in params.named_children(&mut cur) {
        if child.kind() == "parameter"
            && let (Some(pat), Some(ty)) = (child.child_by_field_name("pattern"), child.child_by_field_name("type"))
        {
            let pname = pat.utf8_text(source.as_bytes()).unwrap_or("").trim();
            let tname = rust_type_ident(ty.utf8_text(source.as_bytes()).unwrap_or(""));
            if !pname.is_empty() && !tname.is_empty() {
                locals.insert(pname.to_string(), tname);
            }
        }
    }
}
