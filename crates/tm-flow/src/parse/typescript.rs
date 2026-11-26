//! Tree-sitter TypeScript, TSX, JavaScript, and JSX parser for `.flow`.

use std::collections::HashMap;

use tree_sitter::{Node as TsNode, Parser};

use crate::model::{EdgeKind, Node, NodeKind, NodeStatus, body_hash, file_node_id, sig_hash};
use crate::parse::{
    NameCounter, ParsedFile, RawCall, RawImport, RawRelation, clean_doc_line, clean_signature,
    make_file_node,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Ts,
    Tsx,
    Js,
}

pub fn parse(path: &str, lang: &str, dialect: Dialect, source: &str, blob: &str) -> ParsedFile {
    let mut parser = Parser::new();
    let language = match dialect {
        Dialect::Ts => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Dialect::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Dialect::Js => tree_sitter_javascript::LANGUAGE.into(),
    };
    let _ = parser.set_language(&language);
    let file_id = file_node_id(lang, path);
    let Some(tree) = parser.parse(source, None) else {
        return crate::parse::fallback::parse(path, lang, source, blob);
    };

    let root = tree.root_node();
    let file_doc = leading_file_doc(root, source);
    let mut nodes = vec![make_file_node(lang, path, source, file_doc)];
    let mut imports = Vec::new();
    let mut relations = Vec::new();
    let mut calls = Vec::new();
    let mut counter = NameCounter::default();

    let mut ctx = TsCtx {
        path,
        lang,
        source,
        file_id: &file_id,
        nodes: &mut nodes,
        imports: &mut imports,
        relations: &mut relations,
        calls: &mut calls,
        counter: &mut counter,
    };

    let mut cur = root.walk();
    for child in root.children(&mut cur) {
        ctx.visit_stmt(child, false, None);
    }

    ParsedFile {
        path: path.to_string(),
        lang: lang.to_string(),
        blob: blob.to_string(),
        file_id,
        nodes,
        imports,
        relations,
        calls,
    }
}

struct TsCtx<'a> {
    path: &'a str,
    lang: &'a str,
    source: &'a str,
    file_id: &'a str,
    nodes: &'a mut Vec<Node>,
    imports: &'a mut Vec<RawImport>,
    relations: &'a mut Vec<RawRelation>,
    calls: &'a mut Vec<RawCall>,
    counter: &'a mut NameCounter,
}

impl<'a> TsCtx<'a> {
    fn text(&self, node: TsNode) -> &'a str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    fn visit_stmt(&mut self, node: TsNode, exported: bool, doc_anchor: Option<TsNode>) {
        let anchor = doc_anchor.unwrap_or(node);
        match node.kind() {
            "import_statement" => self.extract_import(node),
            "export_statement" => self.extract_export(node),
            "class_declaration" | "abstract_class_declaration" => {
                self.extract_class(node, exported, anchor);
            }
            "interface_declaration" => self.extract_interface(node, exported, anchor),
            "type_alias_declaration" | "enum_declaration" => {
                self.extract_type_or_enum(node, exported, anchor);
            }
            "function_declaration" | "generator_function_declaration" => {
                self.extract_function(node, exported, anchor);
            }
            "lexical_declaration" | "variable_declaration" => {
                self.extract_var_decl(node, exported, anchor);
            }
            "expression_statement" => {
                let mut locals = HashMap::new();
                self.collect_calls(node, self.file_id, None, &mut locals);
            }
            _ => {}
        }
    }

    fn extract_export(&mut self, node: TsNode) {
        if let Some(source_node) = node.child_by_field_name("source") {
            let module = unquote(self.text(source_node));
            let mut names = Vec::new();
            let mut wildcard = false;
            let mut cur = node.walk();
            for child in node.children(&mut cur) {
                if child.kind() == "*" {
                    wildcard = true;
                } else if child.kind() == "export_clause" {
                    let mut ecur = child.walk();
                    for spec in child.named_children(&mut ecur) {
                        if spec.kind() == "export_specifier" {
                            let remote = spec
                                .child_by_field_name("name")
                                .map(|n| self.text(n).trim().to_string())
                                .unwrap_or_default();
                            let local = spec
                                .child_by_field_name("alias")
                                .map(|n| self.text(n).trim().to_string())
                                .unwrap_or_else(|| remote.clone());
                            if !remote.is_empty() {
                                names.push((remote, local));
                            }
                        }
                    }
                }
            }
            if !module.is_empty() {
                self.imports.push(RawImport {
                    module,
                    relative_dots: 0,
                    names,
                    module_alias: None,
                    wildcard,
                });
            }
            return;
        }
        if let Some(decl) = node.child_by_field_name("declaration") {
            self.visit_stmt(decl, true, Some(node));
            return;
        }
        // `export { Foo, Bar }` marks previously defined top-level symbols exported.
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if child.kind() == "export_clause" {
                let mut ecur = child.walk();
                for spec in child.named_children(&mut ecur) {
                    if let Some(name) = spec.child_by_field_name("name") {
                        let target = self.text(name).trim();
                        for n in self.nodes.iter_mut() {
                            if n.parent.is_none() && n.name == target {
                                n.exported = true;
                            }
                        }
                    }
                }
            }
        }
    }

    fn extract_import(&mut self, node: TsNode) {
        let Some(source_node) = node.child_by_field_name("source") else { return };
        let module = unquote(self.text(source_node));
        if module.is_empty() {
            return;
        }
        let mut names = Vec::new();
        let mut module_alias = None;

        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if child.kind() == "import_clause" {
                let mut icur = child.walk();
                for part in child.named_children(&mut icur) {
                    match part.kind() {
                        "identifier" => {
                            let local = self.text(part).trim().to_string();
                            if !local.is_empty() {
                                names.push(("default".to_string(), local.clone()));
                                names.push((local.clone(), local));
                            }
                        }
                        "namespace_import" => {
                            let mut ncur = part.walk();
                            for id in part.named_children(&mut ncur) {
                                if id.kind() == "identifier" {
                                    module_alias = Some(self.text(id).trim().to_string());
                                }
                            }
                        }
                        "named_imports" => {
                            let mut ncur = part.walk();
                            for spec in part.named_children(&mut ncur) {
                                if spec.kind() == "import_specifier" {
                                    let remote = spec
                                        .child_by_field_name("name")
                                        .map(|n| self.text(n).trim().to_string())
                                        .unwrap_or_default();
                                    let local = spec
                                        .child_by_field_name("alias")
                                        .map(|n| self.text(n).trim().to_string())
                                        .unwrap_or_else(|| remote.clone());
                                    if !remote.is_empty() {
                                        names.push((remote, local));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        self.imports.push(RawImport {
            module,
            relative_dots: 0,
            names,
            module_alias,
            wildcard: false,
        });
    }

    fn extract_class(&mut self, node: TsNode, exported: bool, anchor: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id(self.lang, self.path, &name);
        let body = node.child_by_field_name("body");
        let sig = header_before(self.source, anchor, body);
        let doc = prev_doc_comment(anchor, self.source);
        let body_text = self.text(anchor);

        // Extract `extends` and `implements` from `class_heritage`.
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if child.kind() == "class_heritage" {
                let mut hcur = child.walk();
                for clause in child.named_children(&mut hcur) {
                    match clause.kind() {
                        "extends_clause" => {
                            let mut ecur = clause.walk();
                            for base in clause.named_children(&mut ecur) {
                                let target = type_ident(self.text(base));
                                if !target.is_empty() {
                                    self.relations.push(RawRelation {
                                        from_id: id.clone(),
                                        target,
                                        kind: EdgeKind::Inherits,
                                    });
                                }
                            }
                        }
                        "implements_clause" => {
                            let mut icur = clause.walk();
                            for iface in clause.named_children(&mut icur) {
                                let target = type_ident(self.text(iface));
                                if !target.is_empty() {
                                    self.relations.push(RawRelation {
                                        from_id: id.clone(),
                                        target,
                                        kind: EdgeKind::Implements,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Class,
            name: name.clone(),
            path: self.path.to_string(),
            parent: None,
            span: span_of(anchor),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        let mut class_fields: HashMap<String, String> = HashMap::new();
        if let Some(body_node) = body {
            collect_class_field_types(body_node, self.source, &mut class_fields);
            let mut mcur = body_node.walk();
            for member in body_node.named_children(&mut mcur) {
                if matches!(
                    member.kind(),
                    "method_definition" | "method_signature" | "abstract_method_signature"
                ) {
                    self.extract_method(member, &id, &name, exported, &class_fields);
                }
            }
        }
    }

    fn extract_interface(&mut self, node: TsNode, exported: bool, anchor: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id(self.lang, self.path, &name);
        let body = node.child_by_field_name("body");
        let sig = header_before(self.source, anchor, body);
        let doc = prev_doc_comment(anchor, self.source);
        let body_text = self.text(anchor);

        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if child.kind() == "extends_type_clause" {
                let mut ecur = child.walk();
                for base in child.named_children(&mut ecur) {
                    let target = type_ident(self.text(base));
                    if !target.is_empty() {
                        self.relations.push(RawRelation {
                            from_id: id.clone(),
                            target,
                            kind: EdgeKind::Inherits,
                        });
                    }
                }
            }
        }

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Type,
            name: name.clone(),
            path: self.path.to_string(),
            parent: None,
            span: span_of(anchor),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        if let Some(body_node) = body {
            let mut mcur = body_node.walk();
            for member in body_node.named_children(&mut mcur) {
                if member.kind() == "method_signature" {
                    self.extract_method(member, &id, &name, exported, &HashMap::new());
                }
            }
        }
    }

    fn extract_type_or_enum(&mut self, node: TsNode, exported: bool, anchor: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id(self.lang, self.path, &name);
        let body = node.child_by_field_name("body");
        let sig = if body.is_some() {
            header_before(self.source, anchor, body)
        } else {
            clean_signature(self.text(anchor))
        };
        let doc = prev_doc_comment(anchor, self.source);
        let body_text = self.text(anchor);
        self.nodes.push(Node {
            id,
            kind: NodeKind::Type,
            name,
            path: self.path.to_string(),
            parent: None,
            span: span_of(anchor),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });
    }

    fn extract_function(&mut self, node: TsNode, exported: bool, anchor: TsNode) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let id = self.counter.next_id(self.lang, self.path, &name);
        let body = node.child_by_field_name("body");
        let sig = header_before(self.source, anchor, body);
        let doc = prev_doc_comment(anchor, self.source);
        let body_text = self.text(anchor);

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Function,
            name,
            path: self.path.to_string(),
            parent: None,
            span: span_of(anchor),
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
            collect_ts_param_types(params, self.source, &mut locals);
        }
        if let Some(body_node) = body {
            self.collect_calls(body_node, &id, None, &mut locals);
        }
    }

    fn extract_method(
        &mut self,
        node: TsNode,
        parent_id: &str,
        parent_name: &str,
        parent_exported: bool,
        class_fields: &HashMap<String, String>,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else { return };
        let name = self.text(name_node).trim().to_string();
        if name.is_empty() {
            return;
        }
        let qual = format!("{parent_name}.{name}");
        let id = self.counter.next_id(self.lang, self.path, &qual);
        let body = node.child_by_field_name("body");
        let sig = if body.is_some() {
            header_before(self.source, node, body)
        } else {
            clean_signature(self.text(node).trim_end_matches(';'))
        };
        let doc = prev_doc_comment(node, self.source);
        let body_text = self.text(node);
        let is_private = sig.starts_with("private ") || name.starts_with('#');

        self.nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Method,
            name,
            path: self.path.to_string(),
            parent: Some(parent_id.to_string()),
            span: span_of(node),
            sig_hash: sig_hash(&sig),
            body_hash: body_hash(body_text),
            sig,
            exported: parent_exported && !is_private,
            doc,
            owner: None,
            status: NodeStatus::Built,
        });

        let mut locals = class_fields.clone();
        if let Some(params) = node.child_by_field_name("parameters") {
            collect_ts_param_types(params, self.source, &mut locals);
        }
        if let Some(body_node) = body {
            self.collect_calls(body_node, &id, Some(parent_name), &mut locals);
        }
    }

    fn extract_var_decl(&mut self, node: TsNode, exported: bool, anchor: TsNode) {
        let is_const = self.text(node).trim_start().starts_with("const ");
        let mut cur = node.walk();
        for child in node.named_children(&mut cur) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = child.child_by_field_name("name") else { continue };
            let value = child.child_by_field_name("value");

            // Handle CommonJS `const x = require('./mod')` or `const { a } = require('./mod')`.
            if let Some(val) = value
                && let Some(req_mod) = extract_require_module(val, self.source)
            {
                let mut names = Vec::new();
                let mut module_alias = None;
                if name_node.kind() == "identifier" {
                    module_alias = Some(self.text(name_node).trim().to_string());
                } else if name_node.kind() == "object_pattern" {
                    let mut pcur = name_node.walk();
                    for item in name_node.named_children(&mut pcur) {
                        if item.kind() == "shorthand_property_identifier_pattern" {
                            let s = self.text(item).trim().to_string();
                            names.push((s.clone(), s));
                        } else if item.kind() == "pair_pattern" {
                            let k = item.child_by_field_name("key").map(|n| self.text(n).trim().to_string()).unwrap_or_default();
                            let v = item.child_by_field_name("value").map(|n| self.text(n).trim().to_string()).unwrap_or_else(|| k.clone());
                            if !k.is_empty() {
                                names.push((k, v));
                            }
                        }
                    }
                }
                self.imports.push(RawImport {
                    module: req_mod,
                    relative_dots: 0,
                    names,
                    module_alias,
                    wildcard: false,
                });
                continue;
            }

            if name_node.kind() != "identifier" {
                continue;
            }
            let name = self.text(name_node).trim().to_string();
            if name.is_empty() {
                continue;
            }
            let is_fn = value.is_some_and(|v| {
                matches!(v.kind(), "arrow_function" | "function_expression" | "function")
            });
            if !is_fn && !is_const && !exported {
                continue;
            }
            let kind = if is_fn { NodeKind::Function } else { NodeKind::Const };
            let id = self.counter.next_id(self.lang, self.path, &name);
            let fn_body = value.and_then(|v| v.child_by_field_name("body"));
            let sig = if is_fn && fn_body.is_some() {
                header_before(self.source, anchor, fn_body)
            } else {
                clean_signature(self.text(anchor).lines().next().unwrap_or(""))
            };
            let doc = prev_doc_comment(anchor, self.source);
            let body_text = self.text(anchor);

            self.nodes.push(Node {
                id: id.clone(),
                kind,
                name,
                path: self.path.to_string(),
                parent: None,
                span: span_of(anchor),
                sig_hash: sig_hash(&sig),
                body_hash: body_hash(body_text),
                sig,
                exported,
                doc,
                owner: None,
                status: NodeStatus::Built,
            });

            if let Some(val) = value {
                let mut locals = HashMap::new();
                if let Some(params) = val.child_by_field_name("parameters") {
                    collect_ts_param_types(params, self.source, &mut locals);
                }
                if let Some(body_node) = fn_body {
                    self.collect_calls(body_node, &id, None, &mut locals);
                } else {
                    self.collect_calls(val, &id, None, &mut locals);
                }
            }
        }
    }

    fn collect_calls(
        &mut self,
        node: TsNode,
        from_id: &str,
        enclosing_type: Option<&str>,
        locals: &mut HashMap<String, String>,
    ) {
        if node.kind() == "variable_declarator"
            && let Some(lhs) = node.child_by_field_name("name")
            && lhs.kind() == "identifier"
        {
            let vname = self.text(lhs).trim();
            if let Some(ty) = node.child_by_field_name("type") {
                let tname = type_ident(self.text(ty).trim_start_matches(':'));
                if !tname.is_empty() {
                    locals.insert(vname.to_string(), tname);
                }
            } else if let Some(val) = node.child_by_field_name("value")
                && val.kind() == "new_expression"
                && let Some(ctor) = val.child_by_field_name("constructor")
            {
                let tname = type_ident(self.text(ctor));
                if !tname.is_empty() {
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
                        "member_expression" => {
                            let obj = func.child_by_field_name("object").map(|n| self.text(n).trim().to_string());
                            let prop = func.child_by_field_name("property").map(|n| self.text(n).trim().to_string());
                            if let Some(callee) = prop.filter(|s| !s.is_empty()) {
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
            "new_expression" => {
                if let Some(ctor) = node.child_by_field_name("constructor") {
                    let raw = self.text(ctor).trim();
                    let (receiver, callee) = match raw.rsplit_once('.') {
                        Some((r, c)) => (Some(r.to_string()), c.to_string()),
                        None => (None, raw.to_string()),
                    };
                    if !callee.is_empty() {
                        self.calls.push(RawCall {
                            from_id: from_id.to_string(),
                            receiver,
                            callee,
                            enclosing_type: enclosing_type.map(str::to_string),
                            receiver_type: None,
                        });
                    }
                }
            }
            _ => {}
        }

        let mut cur = node.walk();
        for child in node.children(&mut cur) {
            if matches!(child.kind(), "class_declaration" | "abstract_class_declaration") {
                continue;
            }
            self.collect_calls(child, from_id, enclosing_type, locals);
        }
    }
}

fn span_of(node: TsNode) -> [u32; 2] {
    [node.start_position().row as u32 + 1, node.end_position().row as u32 + 1]
}

fn header_before(source: &str, anchor: TsNode, body: Option<TsNode>) -> String {
    let start = anchor.start_byte();
    let end = body.map(|b| b.start_byte()).unwrap_or_else(|| anchor.end_byte());
    let slice = source.get(start..end).unwrap_or("");
    let trimmed = slice.trim().trim_end_matches('{').trim();
    clean_signature(trimmed)
}

fn unquote(raw: &str) -> String {
    let t = raw.trim();
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"'))
            || (t.starts_with('\'') && t.ends_with('\''))
            || (t.starts_with('`') && t.ends_with('`')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

fn type_ident(raw: &str) -> String {
    let trimmed = raw.trim();
    let before = trimmed.split(['<', '|', '&', '[']).next().unwrap_or(trimmed).trim();
    let last = before.rsplit('.').next().unwrap_or(before).trim();
    last.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
        .collect()
}

fn extract_require_module(val: TsNode, source: &str) -> Option<String> {
    if val.kind() != "call_expression" {
        return None;
    }
    let func = val.child_by_field_name("function")?;
    if func.utf8_text(source.as_bytes()).ok()?.trim() != "require" {
        return None;
    }
    let args = val.child_by_field_name("arguments")?;
    let first = args.named_child(0)?;
    if first.kind() == "string" {
        let m = unquote(first.utf8_text(source.as_bytes()).ok()?);
        return (!m.is_empty()).then_some(m);
    }
    None
}

fn prev_doc_comment(node: TsNode, source: &str) -> Option<String> {
    let mut prev = node.prev_sibling();
    let mut last_comment = None;
    let target_start = node.start_position().row;
    while let Some(sib) = prev {
        if sib.kind() == "comment" {
            let end_row = sib.end_position().row;
            if target_start <= end_row + 2 {
                last_comment = sib.utf8_text(source.as_bytes()).ok();
                if last_comment.is_some_and(|c| c.starts_with("/**")) {
                    break;
                }
            } else {
                break;
            }
            prev = sib.prev_sibling();
        } else {
            break;
        }
    }
    last_comment.and_then(clean_doc_line)
}

fn leading_file_doc(root: TsNode, source: &str) -> Option<String> {
    let first = root.child(0)?;
    if first.kind() == "comment" && first.start_position().row == 0 {
        let raw = first.utf8_text(source.as_bytes()).ok()?;
        return clean_doc_line(raw);
    }
    None
}

fn collect_ts_param_types(params: TsNode, source: &str, locals: &mut HashMap<String, String>) {
    let mut cur = params.walk();
    for child in params.named_children(&mut cur) {
        if matches!(child.kind(), "required_parameter" | "optional_parameter") {
            let pat = child.child_by_field_name("pattern").or_else(|| child.named_child(0));
            let ty = child.child_by_field_name("type");
            if let (Some(p), Some(t)) = (pat, ty) {
                let pname = p.utf8_text(source.as_bytes()).unwrap_or("").trim();
                let tname = type_ident(t.utf8_text(source.as_bytes()).unwrap_or("").trim_start_matches(':'));
                if !pname.is_empty() && !tname.is_empty() {
                    locals.insert(pname.to_string(), tname);
                }
            }
        }
    }
}

fn collect_class_field_types(body: TsNode, source: &str, fields: &mut HashMap<String, String>) {
    let mut cur = body.walk();
    for member in body.named_children(&mut cur) {
        if matches!(member.kind(), "public_field_definition" | "field_definition")
            && let (Some(name), Some(ty)) = (member.child_by_field_name("name"), member.child_by_field_name("type"))
        {
            let n = name.utf8_text(source.as_bytes()).unwrap_or("").trim();
            let t = type_ident(ty.utf8_text(source.as_bytes()).unwrap_or("").trim_start_matches(':'));
            if !n.is_empty() && !t.is_empty() {
                fields.insert(format!("this.{n}"), t);
            }
        }
    }
}
