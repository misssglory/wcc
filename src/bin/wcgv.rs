use anyhow::Result;
use chrono::Local;
use quote::ToTokens;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};
use syn::{
    parse_file,
    spanned::Spanned,
    visit::Visit,
    FnArg, GenericParam, Generics, ImplItem, ItemFn, ItemImpl, ReturnType, Signature, WhereClause,
};

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    signature: String,
    calls: HashSet<String>,
    generics: Vec<String>,
    visibility: String,
    path: PathBuf,
    in_impl: Option<String>,
    range: SourceRange,
    source_snippet: String,
}

#[derive(Debug, Clone)]
struct StructInfo {
    name: String,
    visibility: String,
    path: PathBuf,
    range: SourceRange,
    source_snippet: String,
}

#[derive(Debug, Clone)]
struct TraitInfo {
    name: String,
    visibility: String,
    path: PathBuf,
    range: SourceRange,
    source_snippet: String,
}

#[derive(Debug, Clone)]
struct ImplInfo {
    target_type: String,
    methods: Vec<FunctionInfo>,
    path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct CodeGraph {
    structs: HashMap<String, StructInfo>,
    functions: HashMap<String, FunctionInfo>,
    impls: HashMap<String, ImplInfo>,
    traits: HashMap<String, TraitInfo>,
}

#[derive(Debug, Clone)]
struct GraphNode {
    id: String,
    label: String,
    node_type: String,
    path: String,
    visibility: String,
    size: f64,
    color: String,
    level: i32,
    calls: Vec<String>,
    signature: Option<String>,
    range: SourceRange,
    source_snippet: String,
}

#[derive(Debug, Clone)]
struct GraphEdge {
    source: String,
    target: String,
    edge_type: String,
}

#[derive(Debug, Clone)]
struct Position {
    line: usize,
    column: usize,
}

#[derive(Debug, Clone)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct SourceRange {
    start: Position,
    end: Position,
    bytes: Option<ByteRange>,
}

impl Default for SourceRange {
    fn default() -> Self {
        Self {
            start: Position { line: 0, column: 0 },
            end: Position { line: 0, column: 0 },
            bytes: None,
        }
    }
}

fn line_col_to_byte_index(text: &str, line: usize, column: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    let mut current_line = 1usize;
    let mut current_col = 0usize;
    for (idx, ch) in text.char_indices() {
        if current_line == line && current_col == column {
            return Some(idx);
        }
        if ch == '\n' {
            current_line += 1;
            current_col = 0;
            if current_line == line && column == 0 {
                return Some(idx + 1);
            }
        } else {
            current_col += 1;
        }
    }
    if current_line == line && current_col == column {
        Some(text.len())
    } else {
        None
    }
}

fn range_from_span<T: Spanned>(node: &T, content: &str) -> SourceRange {
    let span = node.span();
    let start = span.start();
    let end = span.end();
    let byte_start = line_col_to_byte_index(content, start.line, start.column);
    let byte_end = line_col_to_byte_index(content, end.line, end.column);

    SourceRange {
        start: Position {
            line: start.line,
            column: start.column,
        },
        end: Position {
            line: end.line,
            column: end.column,
        },
        bytes: match (byte_start, byte_end) {
            (Some(start), Some(end)) if end >= start => Some(ByteRange { start, end }),
            _ => None,
        },
    }
}

fn snippet_from_range(content: &str, range: &SourceRange) -> String {
    if let Some(bytes) = &range.bytes {
        if bytes.start <= bytes.end && bytes.end <= content.len() {
            return content[bytes.start..bytes.end].to_string();
        }
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = range.start.line.saturating_sub(1);
    let end = range.end.line.min(lines.len());
    if start < end {
        lines[start..end].join("\n")
    } else {
        String::new()
    }
}

impl CodeGraph {
    fn to_visualization_graph(&self, root: &Path) -> (Vec<GraphNode>, Vec<GraphEdge>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut node_ids = HashSet::new();

        for (name, func) in &self.functions {
            let rel_path = func.path.strip_prefix(root).unwrap_or(&func.path);
            let node_id = if let Some(target) = &func.in_impl {
                format!("func::{}::{}", target, name)
            } else {
                format!("func::{}", name)
            };

            if node_ids.insert(node_id.clone()) {
                nodes.push(GraphNode {
                    id: node_id.clone(),
                    label: if let Some(target) = &func.in_impl {
                        format!("{}::{}", target, name)
                    } else {
                        name.clone()
                    },
                    node_type: if func.in_impl.is_some() { "method".into() } else { "function".into() },
                    path: rel_path.display().to_string(),
                    visibility: func.visibility.clone(),
                    size: if func.in_impl.is_some() { 0.9 } else { 1.0 },
                    color: if func.in_impl.is_some() {
                        "#9C27B0".to_string()
                    } else if func.visibility == "pub" {
                        "#4CAF50".to_string()
                    } else {
                        "#9E9E9E".to_string()
                    },
                    level: if func.in_impl.is_some() { 1 } else { 0 },
                    calls: func.calls.iter().cloned().collect(),
                    signature: Some(func.signature.clone()),
                    range: func.range.clone(),
                    source_snippet: func.source_snippet.clone(),
                });
            }

            for call in &func.calls {
                let target_id = if self.functions.contains_key(call) {
                    format!("func::{}", call)
                } else {
                    format!("unresolved::{}", call)
                };
                edges.push(GraphEdge {
                    source: node_id.clone(),
                    target: target_id,
                    edge_type: "calls".to_string(),
                });
            }
        }

        for (name, struct_info) in &self.structs {
            let rel_path = struct_info.path.strip_prefix(root).unwrap_or(&struct_info.path);
            let node_id = format!("struct::{}", name);
            if node_ids.insert(node_id.clone()) {
                nodes.push(GraphNode {
                    id: node_id,
                    label: name.clone(),
                    node_type: "struct".to_string(),
                    path: rel_path.display().to_string(),
                    visibility: struct_info.visibility.clone(),
                    size: 1.2,
                    color: "#2196F3".to_string(),
                    level: 0,
                    calls: Vec::new(),
                    signature: None,
                    range: struct_info.range.clone(),
                    source_snippet: struct_info.source_snippet.clone(),
                });
            }
        }

        for (name, trait_info) in &self.traits {
            let rel_path = trait_info.path.strip_prefix(root).unwrap_or(&trait_info.path);
            let node_id = format!("trait::{}", name);
            if node_ids.insert(node_id.clone()) {
                nodes.push(GraphNode {
                    id: node_id,
                    label: name.clone(),
                    node_type: "trait".to_string(),
                    path: rel_path.display().to_string(),
                    visibility: trait_info.visibility.clone(),
                    size: 1.1,
                    color: "#FF9800".to_string(),
                    level: 0,
                    calls: Vec::new(),
                    signature: None,
                    range: trait_info.range.clone(),
                    source_snippet: trait_info.source_snippet.clone(),
                });
            }
        }

        for (target, impl_info) in &self.impls {
            let impl_id = format!("impl::{}", target);
            let struct_id = format!("struct::{}", target);
            edges.push(GraphEdge {
                source: impl_id.clone(),
                target: struct_id,
                edge_type: "implements".to_string(),
            });
            for method in &impl_info.methods {
                let method_id = format!("func::{}::{}", target, method.name);
                edges.push(GraphEdge {
                    source: impl_id.clone(),
                    target: method_id,
                    edge_type: "contains".to_string(),
                });
            }
        }

        (nodes, edges)
    }

    fn export_to_json(&self, root: &Path) -> serde_json::Value {
        let (nodes, edges) = self.to_visualization_graph(root);

        let nodes_json: Vec<_> = nodes
            .iter()
            .map(|node| {
                json!({
                    "key": node.id,
                    "attributes": {
                        "label": node.label,
                        "type": node.node_type,
                        "path": node.path,
                        "visibility": node.visibility,
                        "size": node.size,
                        "color": node.color,
                        "level": node.level,
                        "calls": node.calls,
                        "signature": node.signature,
                        "sourceSnippet": node.source_snippet,
                        "range": {
                            "start": {
                                "line": node.range.start.line,
                                "column": node.range.start.column,
                            },
                            "end": {
                                "line": node.range.end.line,
                                "column": node.range.end.column,
                            },
                            "bytes": node.range.bytes.as_ref().map(|b| json!({
                                "start": b.start,
                                "end": b.end,
                            }))
                        }
                    }
                })
            })
            .collect();

        let edges_json: Vec<_> = edges
            .iter()
            .map(|edge| {
                json!({
                    "source": edge.source,
                    "target": edge.target,
                    "attributes": {
                        "type": edge.edge_type
                    }
                })
            })
            .collect();

        json!({
            "graph": {
                "nodes": nodes_json,
                "edges": edges_json
            },
            "metadata": {
                "timestamp": Local::now().to_rfc3339(),
                "root": root.display().to_string(),
                "stats": {
                    "functions": self.functions.len(),
                    "structs": self.structs.len(),
                    "traits": self.traits.len(),
                    "impls": self.impls.len()
                }
            }
        })
    }

    fn save_for_viewer(&self, root: &Path, output_path: &Path) -> Result<()> {
        let json_data = self.export_to_json(root);
        let json_string = serde_json::to_string_pretty(&json_data)?;
        fs::write(output_path, json_string)?;
        Ok(())
    }
}

struct CodeGraphVisitor {
    graph: CodeGraph,
    current_file: PathBuf,
    current_file_content: String,
    current_impl_target: Option<String>,
}

impl CodeGraphVisitor {
    fn new(file_path: PathBuf, content: String) -> Self {
        Self {
            graph: CodeGraph::default(),
            current_file: file_path,
            current_file_content: content,
            current_impl_target: None,
        }
    }

    fn get_visibility(&self, vis: &syn::Visibility) -> String {
        match vis {
            syn::Visibility::Public(_) => "pub".to_string(),
            _ => "private".to_string(),
        }
    }

    fn parse_generics(&self, generics: &Generics) -> Vec<String> {
        generics
            .params
            .iter()
            .map(|param| match param {
                GenericParam::Type(type_param) => type_param.ident.to_string(),
                GenericParam::Lifetime(lifetime_def) => lifetime_def.lifetime.ident.to_string(),
                GenericParam::Const(const_param) => const_param.ident.to_string(),
            })
            .collect()
    }

    fn parse_where_clause(&self, where_clause: &Option<WhereClause>) -> Option<String> {
        where_clause.as_ref().map(|wc| wc.to_token_stream().to_string())
    }

    fn format_signature(&self, sig: &Signature) -> String {
        let mut parts = Vec::new();
        if sig.constness.is_some() {
            parts.push("const".to_string());
        }
        if sig.asyncness.is_some() {
            parts.push("async".to_string());
        }
        if sig.unsafety.is_some() {
            parts.push("unsafe".to_string());
        }
        parts.push("fn".to_string());
        parts.push(sig.ident.to_string());
        if !sig.generics.params.is_empty() {
            parts.push(sig.generics.to_token_stream().to_string());
        }
        let params: Vec<String> = sig
            .inputs
            .iter()
            .map(|input| match input {
                FnArg::Receiver(recv) => {
                    if recv.reference.is_some() {
                        if recv.mutability.is_some() {
                            "&mut self".to_string()
                        } else {
                            "&self".to_string()
                        }
                    } else {
                        "self".to_string()
                    }
                }
                FnArg::Typed(pat_type) => pat_type.to_token_stream().to_string(),
            })
            .collect();
        parts.push(format!("({})", params.join(", ")));
        if let ReturnType::Type(_, ty) = &sig.output {
            parts.push("->".to_string());
            parts.push(ty.to_token_stream().to_string());
        }
        parts.join(" ")
    }

    fn extract_function_calls(&self, block: &syn::Block) -> HashSet<String> {
        fn is_ignored_call(name: &str) -> bool {
            let simple = name.rsplit("::").next().unwrap_or(name);
            matches!(
                simple,
                "Ok"
                    | "Err"
                    | "Some"
                    | "None"
                    | "Self"
                    | "default"
                    | "new"
                    | "into"
                    | "from"
                    | "clone"
                    | "iter"
                    | "collect"
                    | "len"
                    | "is_empty"
                    | "unwrap"
                    | "expect"
                    | "map"
                    | "and_then"
            )
        }

        let mut calls = HashSet::new();
        let mut visitor = FunctionCallVisitor::new(&mut calls);
        visitor.visit_block(block);
        calls.into_iter().filter(|call| !is_ignored_call(call)).collect()
    }

    fn item_range<T: Spanned>(&self, node: &T) -> SourceRange {
        range_from_span(node, &self.current_file_content)
    }

    fn item_snippet<T: Spanned>(&self, node: &T) -> String {
        let range = self.item_range(node);
        snippet_from_range(&self.current_file_content, &range)
    }
}

struct FunctionCallVisitor<'a> {
    calls: &'a mut HashSet<String>,
}

impl<'a> FunctionCallVisitor<'a> {
    fn new(calls: &'a mut HashSet<String>) -> Self {
        Self { calls }
    }
}

impl<'a, 'ast> Visit<'ast> for FunctionCallVisitor<'a> {
    fn visit_expr_call(&mut self, expr_call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(expr_path) = &*expr_call.func {
            let segments: Vec<String> = expr_path
                .path
                .segments
                .iter()
                .map(|seg| seg.ident.to_string())
                .collect();
            self.calls.insert(segments.join("::"));
        }
        syn::visit::visit_expr_call(self, expr_call);
    }

    fn visit_expr_method_call(&mut self, method_call: &'ast syn::ExprMethodCall) {
        self.calls.insert(method_call.method.to_string());
        syn::visit::visit_expr_method_call(self, method_call);
    }
}

impl<'ast> Visit<'ast> for CodeGraphVisitor {
    fn visit_item_fn(&mut self, item_fn: &'ast ItemFn) {
        if self.current_impl_target.is_some() {
            return;
        }
        let func_name = item_fn.sig.ident.to_string();
        let calls = self.extract_function_calls(&item_fn.block);
        let func_info = FunctionInfo {
            name: func_name.clone(),
            signature: self.format_signature(&item_fn.sig),
            calls,
            generics: self.parse_generics(&item_fn.sig.generics),
            visibility: self.get_visibility(&item_fn.vis),
            path: self.current_file.clone(),
            in_impl: None,
            range: self.item_range(item_fn),
            source_snippet: self.item_snippet(item_fn),
        };
        self.graph.functions.insert(func_name, func_info);
        syn::visit::visit_item_fn(self, item_fn);
    }

    fn visit_item_impl(&mut self, item_impl: &'ast ItemImpl) {
        let target_type = item_impl.self_ty.as_ref().to_token_stream().to_string();
        let old_target = self.current_impl_target.take();
        self.current_impl_target = Some(target_type.clone());

        let mut methods = Vec::new();
        for item in &item_impl.items {
            if let ImplItem::Fn(method) = item {
                let func_name = method.sig.ident.to_string();
                let calls = self.extract_function_calls(&method.block);
                let func_info = FunctionInfo {
                    name: func_name.clone(),
                    signature: self.format_signature(&method.sig),
                    calls,
                    generics: self.parse_generics(&method.sig.generics),
                    visibility: self.get_visibility(&method.vis),
                    path: self.current_file.clone(),
                    in_impl: Some(target_type.clone()),
                    range: self.item_range(method),
                    source_snippet: self.item_snippet(method),
                };
                methods.push(func_info.clone());
                self.graph
                    .functions
                    .insert(format!("{}::{}", target_type, func_name), func_info);
            }
        }

        let impl_info = ImplInfo {
            target_type: target_type.clone(),
            methods,
            path: self.current_file.clone(),
        };
        self.graph.impls.insert(target_type, impl_info);
        self.current_impl_target = old_target;
        syn::visit::visit_item_impl(self, item_impl);
    }

    fn visit_item_struct(&mut self, item_struct: &'ast syn::ItemStruct) {
        let name = item_struct.ident.to_string();
        let info = StructInfo {
            name: name.clone(),
            visibility: self.get_visibility(&item_struct.vis),
            path: self.current_file.clone(),
            range: self.item_range(item_struct),
            source_snippet: self.item_snippet(item_struct),
        };
        self.graph.structs.insert(name, info);
        syn::visit::visit_item_struct(self, item_struct);
    }

    fn visit_item_trait(&mut self, item_trait: &'ast syn::ItemTrait) {
        let name = item_trait.ident.to_string();
        let info = TraitInfo {
            name: name.clone(),
            visibility: "pub".to_string(),
            path: self.current_file.clone(),
            range: self.item_range(item_trait),
            source_snippet: self.item_snippet(item_trait),
        };
        self.graph.traits.insert(name, info);
        syn::visit::visit_item_trait(self, item_trait);
    }
}

struct CodeGraphScanner {
    skip_dirs: Vec<String>,
}

impl CodeGraphScanner {
    fn new() -> Self {
        Self {
            skip_dirs: vec![
                "target".to_string(),
                "node_modules".to_string(),
                ".git".to_string(),
                ".cargo".to_string(),
            ],
        }
    }

    fn scan_directory(&self, dir: &Path) -> Result<CodeGraph> {
        let mut graph = CodeGraph::default();
        self.walk_directory(dir, &mut graph)?;
        Ok(graph)
    }

    fn walk_directory(&self, dir: &Path, graph: &mut CodeGraph) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if self.skip_dirs.iter().any(|d| d == name_str.as_ref()) {
                        continue;
                    }
                }
                self.walk_directory(&path, graph)?;
            } else if self.is_rust_file(&path) {
                if let Ok(file_graph) = self.analyze_file(&path) {
                    self.merge_graphs(graph, file_graph);
                }
            }
        }
        Ok(())
    }

    fn is_rust_file(&self, path: &Path) -> bool {
        path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs")
    }

    fn analyze_file(&self, file_path: &Path) -> Result<CodeGraph> {
        let content = fs::read_to_string(file_path)?;
        let mut visitor = CodeGraphVisitor::new(file_path.to_path_buf(), content.clone());
        match parse_file(&content) {
            Ok(syntax_tree) => {
                visitor.visit_file(&syntax_tree);
                Ok(visitor.graph)
            }
            Err(e) => {
                eprintln!("Warning: Failed to parse {}: {}", file_path.display(), e);
                Ok(CodeGraph::default())
            }
        }
    }

    fn merge_graphs(&self, target: &mut CodeGraph, source: CodeGraph) {
        target.structs.extend(source.structs);
        target.functions.extend(source.functions);
        target.impls.extend(source.impls);
        target.traits.extend(source.traits);
    }
}

struct GraphApp {
    scanner: CodeGraphScanner,
    output_path: Option<PathBuf>,
}

impl GraphApp {
    fn new() -> Self {
        let args: Vec<String> = env::args().collect();
        let mut output_path = None;
        let mut i = 1;

        while i < args.len() {
            match args[i].as_str() {
                "--output" | "-o" => {
                    if i + 1 < args.len() {
                        output_path = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        Self {
            scanner: CodeGraphScanner::new(),
            output_path,
        }
    }

    fn run(&self) -> Result<()> {
        let args: Vec<String> = env::args().collect();
        let target_dir = if args.len() > 1 {
            let last_arg = args.last().unwrap();
            if last_arg.starts_with('-') {
                env::current_dir()?
            } else {
                PathBuf::from(last_arg)
            }
        } else {
            env::current_dir()?
        };

        eprintln!("🔍 Scanning directory: {}", target_dir.display());
        let start = std::time::Instant::now();
        let graph = self.scanner.scan_directory(&target_dir)?;
        let duration = start.elapsed();

        eprintln!(
            "📊 Found {} functions, {} structs, {} impls, {} traits",
            graph.functions.len(),
            graph.structs.len(),
            graph.impls.len(),
            graph.traits.len()
        );

        let output_file = self
            .output_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("code_graph.json"));

        graph.save_for_viewer(&target_dir, &output_file)?;

        eprintln!("\x1b[1;32m✓ Graph saved to: {}\x1b[0m", output_file.display());
        eprintln!("\x1b[36m✓ Analysis took {:.2?}\x1b[0m", duration);
        eprintln!("\x1b[33m✓ Ready for the Bun Sigma GUI\x1b[0m");
        println!("{}", output_file.display());
        Ok(())
    }
}

fn main() -> Result<()> {
    let app = GraphApp::new();
    app.run()
}
