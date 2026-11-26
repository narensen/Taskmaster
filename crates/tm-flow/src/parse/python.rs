//! Tree-sitter Python parser for `.flow`.

use std::collections::HashMap;

use tree_sitter::{Node as TsNode, Parser};

use crate::model::{EdgeKind, Node, NodeKind, NodeStatus, body_hash, file_node_id, sig_hash};
use crate::parse::{
    NameCounter, ParsedFile, RawCall, RawImport, RawRelation, clean_signature, make_file_node,
    strip_python_string,
};

pub fn parse(path: &str, source: &str, blob: &str) -> ParsedFile {
    let mut parser = Parser::new();
    let _ = parser.set_language(&tree_sitter_python::LANGUAGE.into());
    let file_id = file_node_id("python", path);
    let Some(tree) = parser.parse(source, None) else {
        return crate::parse::fallback::parse(path, "python", source, blob);
    };

    let root = tree.root_node();
    let module_doc = block_docstring(root, source);
    let mut nodes = vec![make_file_node("python", path, source, module_doc)];
    let mut imports = Vec::new();
    let mut relations = Vec::new();
    let mut calls = Vec::new();
    let mut counter = NameCounter::default();

    let mut ctx = PyCtx {
        path,
        source,
        file_id: &file_id,
        nodes: &mut nodes,
        imports: &mut imports,
        relations: &mut relations,
        calls: &mut calls,
        counter: &mut counter,
    };

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        ctx.visit_top_level(child);
    }

    ParsedFile {
        path: path.to_string(),
        lang: "python".to_string(),
        blob: blob.to_string(),
        file_id,
        nodes,
        imports,
        relations,
        calls,
    }
}

struct PyCtx<'a> {
    path: &'a str,
    source: &'a str,
    file_id: &'a str,
    nodes: &'a mut Vec<Node>,
    imports: &'a mut Vec<RawImport>,
    relations: &'a mut Vec<RawRelation>,
    calls: &'a mut Vec<RawCall>,
    counter: &'a mut NameCounter,
}

impl<'a> PyCtx<'a> {
    fn text(&self, node: TsNode) -> &'a str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    fn visit_top_level(&mut self, node: TsNode) {
        match node.kind() {
            "import_statement" => self.extract_import(node),
            "import_from_statement" => self.extract_import_from(node),
            "decorated_definition" => {
                if let Some(def) = node.child_by_field_name("definition") {
                    match def.kind() {
                        "class_definition" => self.extract_class(def, Some(node), None, None),
                        "function_definition" => {
                            self.extract_function(def, Some(node), None, None, &HashMap::new())
                        }
                        _ => {}
                    }
                }
            }
            "class_definition" => self.extract_class(node, None, None, None),
            "function_definition" => {
                self.extract_function(node, None, None, None, &HashMap::new())
            }
            "type_alias_statement" => self.extract_type_alias(node),
            "expression_statement" => {
                if let Some(child) = node.named_child(0) {
                    if child.kind() == "assignment" {
                        self.extract_top_const(child);
                    }
                    let mut locals = HashMap::new();
                    self.collect_calls(child, self.file_id, None, &mut locals);
                }
            }
            "if_statement" | "try_statement" | "with_statement" => {
                // Capture module-level conditional imports (e.g. `if TYPE_CHECKING:`).
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "block" {
                        let mut bcur = child.walk();
                        for stmt in child.children(&mut bcur) {
                            match stmt.kind() {
                                "import_statement" => self.extract_import(stmt),
                                "import_from_statement" => self.extract_import_from(stmt),
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn extract_type_alias(&mut self, node: TsNode) {
        let Some(name_node) = node.child_by_field_name("left").or_else(|| node.named_child(0)) else {
            return;
        };
        let name = self.text(name_node).trim();
        let ident = name.split('[').next().unwrap_or(name).trim();
        if ident.is_empty() {
            return;
        }
        let body = self.text(node);
        let sig = clean_signature(body);
        let id = self.counter.next_id("python", self.path, ident);
        self.nodes.push(Node {
            id,
            kind: NodeKind::Type,
            name: ident.to_string(),
            path: self.path.to_string(),
            parent: None,
            span: span_of(node),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body),
            sig,
            exported: !ident.starts_with('_'),
            doc: None,
            owner: None,
            status: NodeStatus::Built,
        });
    }

    fn extract_top_const(&mut self, assign: TsNode) {
        let Some(left) = assign.child_by_field_name("left") else { return };
        if left.kind() != "identifier" {
            return;
        }
        let name = self.text(left).trim();
        let is_const_style = !name.is_empty()
            && !name.starts_with('_')
            && (name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                || assign.child_by_field_name("type").is_some());
        if !is_const_style {
            return;
        }
        let body = self.text(assign);
        let sig = clean_signature(body.lines().next().unwrap_or(body));
        let id = self.counter.next_id("python", self.path, name);
        self.nodes.push(Node {
            id,
            kind: NodeKind::Const,
            name: name.to_string(),
            path: self.path.to_string(),
            parent: None,
            span: span_of(assign),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body),
            sig,
            exported: !name.starts_with('_'),
            doc: None,
            owner: None,
            status: NodeStatus::Built,
        });
    }

    fn extract_class(
        &mut self,
        node: TsNode,
        decorated: Option<TsNode>,
        parent_id: Option<&str>,
        parent_qual: Option<&str>,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let qual = match parent_qual {
            Some(p) => format!("{p}.{name}"),
            None => name.clone(),
        };
        let outer = decorated.unwrap_or(node);
        let id = self.counter.next_id("python", self.path, &qual);
        let body_node = node.child_by_field_name("body");
        let sig = extract_py_header(self.source, node, body_node);
        let doc = body_node.and_then(|b| block_docstring(b, self.source));
        let body_text = self.text(outer);

        // Extract base classes for inheritance.
        if let Some(bases) = node.child_by_field_name("superclasses") {
            let mut bcur = bases.walk();
            for child in bases.named_children(&mut bcur) {
                let target = base_type_name(self.text(child));
                if !target.is_empty() {
                    let kind = if target != "Protocol" && target.ends_with("Protocol") {
                        EdgeKind::Implements
                    } else {
                        EdgeKind::Inherits
                    };
                    self.relations.push(RawRelation {
                        from_id: id.clone(),
                        target,
                        kind,
                    });
                }
            }
        }

        // Collect self-attribute type hints across `__init__` or class annotations.
        let mut class_attrs: HashMap<String, String> = HashMap::new();
        if let Some(body) = body_node {
            collect_class_attr_types(body, self.source, &mut class_attrs);
        }

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Class,
            name,
            path: self.path.to_string(),
            parent: parent_id.map(str::to_string),
            span: span_of(outer),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported: !qual.starts_with('_'),
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        if let Some(body) = body_node {
            let mut cur = body.walk();
            for child in body.children(&mut cur) {
                match child.kind() {
                    "decorated_definition" => {
                        if let Some(def) = child.child_by_field_name("definition") {
                            match def.kind() {
                                "function_definition" => {
                                    self.extract_function(def, Some(child), Some(&id), Some(&qual), &class_attrs);
                                }
                                "class_definition" => {
                                    self.extract_class(def, Some(child), Some(&id), Some(&qual));
                                }
                                _ => {}
                            }
                        }
                    }
                    "function_definition" => {
                        self.extract_function(child, None, Some(&id), Some(&qual), &class_attrs);
                    }
                    "class_definition" => {
                        self.extract_class(child, None, Some(&id), Some(&qual));
                    }
                    _ => {}
                }
            }
        }
    }

    fn extract_function(
        &mut self,
        node: TsNode,
        decorated: Option<TsNode>,
        parent_class_id: Option<&str>,
        parent_class_qual: Option<&str>,
        class_attrs: &HashMap<String, String>,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let (kind, qual, exported) = match parent_class_qual {
            Some(cls) => (
                NodeKind::Method,
                format!("{cls}.{name}"),
                !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__")),
            ),
            None => (NodeKind::Function, name.clone(), !name.starts_with('_')),
        };
        let outer = decorated.unwrap_or(node);
        let id = self.counter.next_id("python", self.path, &qual);
        let body_node = node.child_by_field_name("body");
        let sig = extract_py_header(self.source, node, body_node);
        let doc = body_node.and_then(|b| block_docstring(b, self.source));
        let body_text = self.text(outer);

        self.nodes.push(Node {
            id: id.clone(),
            kind,
            name,
            path: self.path.to_string(),
            parent: parent_class_id.map(str::to_string),
            span: span_of(outer),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        let mut locals = class_attrs.clone();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_param_types(params, self.source, &mut locals);
        }
        if let Some(body) = body_node {
            self.collect_calls(body, &id, parent_class_qual, &mut locals);
        }
    }

    fn extract_import(&mut self, node: TsNode) {
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            match child.kind() {
                "dotted_name" => {
                    let module = self.text(child).trim().to_string();
                    let alias = module.split('.').next().unwrap_or(&module).to_string();
                    self.imports.push(RawImport {
                        module,
                        relative_dots: 0,
                        names: vec![],
                        module_alias: Some(alias),
                        wildcard: false,
                    });
                }
                "aliased_import" => {
                    let module = child
                        .child_by_field_name("name")
                        .map(|n| self.text(n).trim().to_string())
                        .unwrap_or_default();
                    let alias = child
                        .child_by_field_name("alias")
                        .map(|n| self.text(n).trim().to_string());
                    if !module.is_empty() {
                        self.imports.push(RawImport {
                            module,
                            relative_dots: 0,
                            names: vec![],
                            module_alias: alias,
                            wildcard: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn extract_import_from(&mut self, node: TsNode) {
        let mut module = String::new();
        let mut relative_dots = 0usize;
        if let Some(mod_node) = node.child_by_field_name("module_name") {
            if mod_node.kind() == "relative_import" {
                let mut cur = mod_node.walk();
                for ch in mod_node.children(&mut cur) {
                    if ch.kind() == "import_prefix" {
                        relative_dots = self.text(ch).chars().filter(|&c| c == '.').count();
                    } else if ch.kind() == "dotted_name" {
                        module = self.text(ch).trim().to_string();
                    }
                }
            } else {
                module = self.text(mod_node).trim().to_string();
            }
        }

        let mut names = Vec::new();
        let mut wildcard = false;
        let mod_id = node.child_by_field_name("module_name").map(|n| n.id());
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if Some(child.id()) == mod_id {
                continue;
            }
            match child.kind() {
                "wildcard_import" => wildcard = true,
                "dotted_name" | "identifier" => {
                    let sym = self.text(child).trim().to_string();
                    if !sym.is_empty() {
                        names.push((sym.clone(), sym));
                    }
                }
                "aliased_import" => {
                    let remote = child
                        .child_by_field_name("name")
                        .map(|n| self.text(n).trim().to_string())
                        .unwrap_or_default();
                    let local = child
                        .child_by_field_name("alias")
                        .map(|n| self.text(n).trim().to_string())
                        .unwrap_or_else(|| remote.clone());
                    if !remote.is_empty() {
                        names.push((remote, local));
                    }
                }
                _ => {}
            }
        }

        self.imports.push(RawImport {
            module,
            relative_dots,
            names,
            module_alias: None,
            wildcard,
        });
    }

    fn collect_calls(
        &mut self,
        node: TsNode,
        from_id: &str,
        enclosing_type: Option<&str>,
        locals: &mut HashMap<String, String>,
    ) {
        // Track local type annotations and constructor assignments (`x = Foo(...)`, `x: Foo = ...`).
        if node.kind() == "assignment"
            && let Some(left) = node.child_by_field_name("left")
        {
            let lhs = self.text(left).trim();
            if let Some(ty) = node.child_by_field_name("type") {
                let tname = base_type_name(self.text(ty));
                if !tname.is_empty() {
                    locals.insert(lhs.to_string(), tname);
                }
            } else if let Some(right) = node.child_by_field_name("right")
                && right.kind() == "call"
                && let Some(func) = right.child_by_field_name("function")
            {
                let fname = self.text(func).trim();
                let short = fname.rsplit('.').next().unwrap_or(fname);
                if short.chars().next().is_some_and(char::is_uppercase) {
                    locals.insert(lhs.to_string(), short.to_string());
                }
            }
        }

        if node.kind() == "call"
            && let Some(func) = node.child_by_field_name("function")
        {
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
                "attribute" => {
                    let obj = func.child_by_field_name("object").map(|n| self.text(n).trim().to_string());
                    let attr = func.child_by_field_name("attribute").map(|n| self.text(n).trim().to_string());
                    if let Some(callee) = attr.filter(|s| !s.is_empty()) {
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

        // Do not descend into nested class definitions (they have their own nodes);
        // nested function defs stay attributed to their enclosing function for MVP simplicity.
        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            if child.kind() == "class_definition" {
                continue;
            }
            self.collect_calls(child, from_id, enclosing_type, locals);
        }
    }
}

fn span_of(node: TsNode) -> [u32; 2] {
    [node.start_position().row as u32 + 1, node.end_position().row as u32 + 1]
}

fn extract_py_header(source: &str, node: TsNode, body: Option<TsNode>) -> String {
    let start = node.start_byte();
    let end = body.map(|b| b.start_byte()).unwrap_or_else(|| node.end_byte());
    let slice = source.get(start..end).unwrap_or("");
    let trimmed = slice.trim().trim_end_matches(':').trim();
    clean_signature(trimmed)
}

fn block_docstring(block: TsNode, source: &str) -> Option<String> {
    let mut cur = block.walk();
    for child in block.named_children(&mut cur) {
        if child.kind() == "comment" {
            continue;
        }
        if child.kind() == "expression_statement"
            && let Some(expr) = child.named_child(0)
            && expr.kind() == "string"
        {
            let raw = expr.utf8_text(source.as_bytes()).ok()?;
            return strip_python_string(raw);
        }
        break;
    }
    None
}

fn base_type_name(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('=') {
        return String::new();
    }
    let before_sub = trimmed.split(['[', '|']).next().unwrap_or(trimmed).trim();
    let last = before_sub.rsplit('.').next().unwrap_or(before_sub).trim();
    last.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn collect_param_types(params: TsNode, source: &str, locals: &mut HashMap<String, String>) {
    let mut cur = params.walk();
    for child in params.named_children(&mut cur) {
        if matches!(child.kind(), "typed_parameter" | "typed_default_parameter") {
            let mut pcur = child.walk();
            let mut ident = None;
            for part in child.named_children(&mut pcur) {
                if part.kind() == "identifier" && ident.is_none() {
                    ident = part.utf8_text(source.as_bytes()).ok().map(str::to_string);
                }
            }
            if let (Some(name), Some(ty)) = (ident, child.child_by_field_name("type")) {
                let tname = base_type_name(ty.utf8_text(source.as_bytes()).unwrap_or(""));
                if !tname.is_empty() {
                    locals.insert(name, tname);
                }
            }
        }
    }
}

fn collect_class_attr_types(body: TsNode, source: &str, attrs: &mut HashMap<String, String>) {
    let mut cur = body.walk();
    for child in body.named_children(&mut cur) {
        if child.kind() == "expression_statement"
            && let Some(assign) = child.named_child(0)
            && assign.kind() == "assignment"
            && let (Some(left), Some(ty)) = (assign.child_by_field_name("left"), assign.child_by_field_name("type"))
        {
            let name = left.utf8_text(source.as_bytes()).unwrap_or("").trim();
            let tname = base_type_name(ty.utf8_text(source.as_bytes()).unwrap_or(""));
            if !name.is_empty() && !tname.is_empty() {
                attrs.insert(format!("self.{name}"), tname);
            }
        } else if child.kind() == "function_definition"
            && child.child_by_field_name("name").and_then(|n| n.utf8_text(source.as_bytes()).ok()) == Some("__init__")
        {
            let mut init_params = HashMap::new();
            if let Some(params) = child.child_by_field_name("parameters") {
                collect_param_types(params, source, &mut init_params);
            }
            if let Some(fn_body) = child.child_by_field_name("body") {
                collect_self_assigns(fn_body, source, &init_params, attrs);
            }
        }
    }
}

fn collect_self_assigns(
    node: TsNode,
    source: &str,
    params: &HashMap<String, String>,
    attrs: &mut HashMap<String, String>,
) {
    if node.kind() == "assignment"
        && let Some(left) = node.child_by_field_name("left")
    {
        let lhs = left.utf8_text(source.as_bytes()).unwrap_or("").trim();
        if lhs.starts_with("self.") {
            if let Some(ty) = node.child_by_field_name("type") {
                let tname = base_type_name(ty.utf8_text(source.as_bytes()).unwrap_or(""));
                if !tname.is_empty() {
                    attrs.insert(lhs.to_string(), tname);
                }
            } else if let Some(right) = node.child_by_field_name("right") {
                let rhs = right.utf8_text(source.as_bytes()).unwrap_or("").trim();
                if let Some(tname) = params.get(rhs) {
                    attrs.insert(lhs.to_string(), tname.clone());
                } else if right.kind() == "call"
                    && let Some(func) = right.child_by_field_name("function")
                {
                    let fname = func.utf8_text(source.as_bytes()).unwrap_or("").trim();
                    let short = fname.rsplit('.').next().unwrap_or(fname);
                    if short.chars().next().is_some_and(char::is_uppercase) {
                        attrs.insert(lhs.to_string(), short.to_string());
                    }
                }
            }
        }
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        collect_self_assigns(child, source, params, attrs);
    }
}
