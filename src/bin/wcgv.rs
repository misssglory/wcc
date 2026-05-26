// src/bin/wcgv.rs - Standalone version that exports graphs for the Sigma.js viewer
use anyhow::{Result};
use chrono::Local;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};
use syn::{
    parse_file, visit::Visit, Attribute, Fields, GenericParam, Generics, ImplItem, ItemImpl,
    ReturnType, Signature, WhereClause,
};

#[derive(Debug, Clone)]
struct StructInfo {
    name: String,
    fields: Vec<FieldInfo>,
    generics: Vec<String>,
    where_clause: Option<String>,
    visibility: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct FieldInfo {
    name: String,
    type_str: String,
    visibility: String,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    signature: String,
    calls: HashSet<String>,
    generics: Vec<String>,
    visibility: String,
    path: PathBuf,
    in_impl: Option<String>,
}

#[derive(Debug, Clone)]
struct ImplInfo {
    target_type: String,
    generics: Vec<String>,
    where_clause: Option<String>,
    methods: Vec<FunctionInfo>,
    traits: Vec<String>,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct TraitInfo {
    name: String,
    methods: Vec<FunctionInfo>,
    generics: Vec<String>,
    super_traits: Vec<String>,
    path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct ImportInfo {
    full_path: String,
    alias: Option<String>,
    last_segment: String,
    is_glob: bool,
}

#[derive(Debug, Clone, Default)]
struct FileImports {
    imports: Vec<ImportInfo>,
}

#[derive(Debug, Clone, Default)]
struct CodeGraph {
    structs: HashMap<String, StructInfo>,
    functions: HashMap<String, FunctionInfo>,
    impls: HashMap<String, ImplInfo>,
    traits: HashMap<String, TraitInfo>,
    unresolved_calls: HashMap<String, HashSet<String>>,
    file_imports: HashMap<PathBuf, FileImports>,
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
}

#[derive(Debug, Clone)]
struct GraphEdge {
    source: String,
    target: String,
    edge_type: String,
}

impl CodeGraph {
    fn to_visualization_graph(&self, root: &Path) -> (Vec<GraphNode>, Vec<GraphEdge>) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut node_ids = HashSet::new();

        // Add all functions as nodes
        for (name, func) in &self.functions {
            let rel_path = func.path.strip_prefix(root).unwrap_or(&func.path);
            let node_id = format!("func::{}", name);
            
            if !node_ids.contains(&node_id) {
                node_ids.insert(node_id.clone());
                nodes.push(GraphNode {
                    id: node_id.clone(),
                    label: name.clone(),
                    node_type: "function".to_string(),
                    path: rel_path.display().to_string(),
                    visibility: func.visibility.clone(),
                    size: 1.0,
                    color: if func.visibility == "pub" { "#4CAF50".to_string() } else { "#9E9E9E".to_string() },
                    level: 0,
                    calls: func.calls.iter().cloned().collect(),
                });
            }
            
            // Add edges for function calls
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

        // Add structs as nodes
        for (name, struct_info) in &self.structs {
            let rel_path = struct_info.path.strip_prefix(root).unwrap_or(&struct_info.path);
            let node_id = format!("struct::{}", name);
            
            if !node_ids.contains(&node_id) {
                node_ids.insert(node_id.clone());
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
                });
            }
        }

        // Add traits as nodes
        for (name, trait_info) in &self.traits {
            let rel_path = trait_info.path.strip_prefix(root).unwrap_or(&trait_info.path);
            let node_id = format!("trait::{}", name);
            
            if !node_ids.contains(&node_id) {
                node_ids.insert(node_id.clone());
                nodes.push(GraphNode {
                    id: node_id,
                    label: name.clone(),
                    node_type: "trait".to_string(),
                    path: rel_path.display().to_string(),
                    visibility: "pub".to_string(),
                    size: 1.1,
                    color: "#FF9800".to_string(),
                    level: 0,
                    calls: Vec::new(),
                });
            }
        }

        // Add impl relationships
        for (target, impl_info) in &self.impls {
            let source_id = format!("impl::{}", target);
            let target_id = format!("struct::{}", target);
            
            edges.push(GraphEdge {
                source: source_id.clone(),
                target: target_id,
                edge_type: "implements".to_string(),
            });
            
            for method in &impl_info.methods {
                let method_id = format!("func::{}::{}", target, method.name);
                nodes.push(GraphNode {
                    id: method_id.clone(),
                    label: format!("{}::{}", target, method.name),
                    node_type: "method".to_string(),
                    path: impl_info.path.display().to_string(),
                    visibility: method.visibility.clone(),
                    size: 0.8,
                    color: "#9C27B0".to_string(),
                    level: 1,
                    calls: method.calls.iter().cloned().collect(),
                });
                
                edges.push(GraphEdge {
                    source: source_id.clone(),
                    target: method_id,
                    edge_type: "contains".to_string(),
                });
            }
        }

        (nodes, edges)
    }

    fn export_to_json(&self, root: &Path) -> serde_json::Value {
        let (nodes, edges) = self.to_visualization_graph(root);
        
        // Build node map for the viewer format
        let mut nodes_json = Vec::new();
        for node in &nodes {
            nodes_json.push(json!({
                "key": node.id,
                "attributes": {
                    "label": node.label,
                    "type": node.node_type,
                    "path": node.path,
                    "visibility": node.visibility,
                    "size": node.size,
                    "color": node.color,
                    "level": node.level,
                    "calls": node.calls
                }
            }));
        }
        
        // Build edges map
        let mut edges_json = Vec::new();
        for edge in &edges {
            edges_json.push(json!({
                "source": edge.source,
                "target": edge.target,
                "attributes": {
                    "type": edge.edge_type
                }
            }));
        }
        
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
    current_impl_target: Option<String>,
}

impl CodeGraphVisitor {
    fn new(file_path: PathBuf) -> Self {
        Self {
            graph: CodeGraph::default(),
            current_file: file_path,
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
            .filter_map(|param| match param {
                GenericParam::Type(type_param) => Some(type_param.ident.to_string()),
                GenericParam::Lifetime(lifetime_def) => Some(lifetime_def.lifetime.ident.to_string()),
                GenericParam::Const(const_param) => Some(const_param.ident.to_string()),
            })
            .collect()
    }

    fn parse_where_clause(&self, where_clause: &Option<WhereClause>) -> Option<String> {
        where_clause
            .as_ref()
            .map(|wc| quote::quote!(# wc).to_string())
    }

    fn format_signature(&self, sig: &Signature) -> String {
        let mut parts: Vec<String> = Vec::new();
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
            parts.push(quote::quote!(# sig.generics).to_string());
        }
        let params: Vec<String> = sig
            .inputs
            .iter()
            .map(|input| match input {
                syn::FnArg::Receiver(recv) => {
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
                syn::FnArg::Typed(pat_type) => quote::quote!(# pat_type).to_string(),
            })
            .collect();
        parts.push(format!("({})", params.join(", ")));
        match &sig.output {
            ReturnType::Default => {}
            ReturnType::Type(_, ty) => {
                parts.push("->".to_string());
                parts.push(quote::quote!(# ty).to_string());
            }
        }
        parts.join(" ")
    }

    fn extract_function_calls(&self, block: &syn::Block) -> HashSet<String> {
        fn is_ignored_call(name: &str) -> bool {
            let simple = name.rsplit("::").next().unwrap_or(name);
            matches!(
                simple,
                "Ok" | "Err" | "Some" | "None" | "Self" | "default" | "new"
                    | "into" | "from" | "clone" | "iter" | "collect" | "len"
                    | "is_empty" | "unwrap" | "expect" | "map" | "and_then"
            )
        }
        
        let mut calls = HashSet::new();
        let mut visitor = FunctionCallVisitor::new(&mut calls);
        visitor.visit_block(block);
        calls
            .into_iter()
            .filter(|call| !is_ignored_call(call))
            .collect()
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

impl<'a> Visit<'a> for FunctionCallVisitor<'a> {
    fn visit_expr_call(&mut self, expr_call: &'a syn::ExprCall) {
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

    fn visit_expr_method_call(&mut self, method_call: &'a syn::ExprMethodCall) {
        self.calls.insert(method_call.method.to_string());
        syn::visit::visit_expr_method_call(self, method_call);
    }
}

impl<'ast> Visit<'ast> for CodeGraphVisitor {
    fn visit_item_fn(&mut self, item_fn: &'ast syn::ItemFn) {
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
        };
        self.graph.functions.insert(func_name, func_info);
        syn::visit::visit_item_fn(self, item_fn);
    }

    fn visit_item_impl(&mut self, item_impl: &'ast ItemImpl) {
        use quote::ToTokens;
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
                };
                methods.push(func_info.clone());
                self.graph.functions.insert(func_name, func_info);
            }
        }
        
        let traits: Vec<String> = item_impl
            .trait_
            .as_ref()
            .map(|(_, path, _)| vec![path.to_token_stream().to_string()])
            .unwrap_or_default();
            
        let impl_info = ImplInfo {
            target_type: target_type.clone(),
            generics: self.parse_generics(&item_impl.generics),
            where_clause: self.parse_where_clause(&item_impl.generics.where_clause),
            methods,
            traits,
            path: self.current_file.clone(),
        };
        self.graph.impls.insert(target_type, impl_info);
        
        self.current_impl_target = old_target;
        syn::visit::visit_item_impl(self, item_impl);
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
        let mut visitor = CodeGraphVisitor::new(file_path.to_path_buf());
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
        for (name, struct_info) in source.structs {
            target.structs.insert(name, struct_info);
        }
        for (name, func_info) in source.functions {
            target.functions.insert(name, func_info);
        }
        for (name, impl_info) in source.impls {
            target.impls.insert(name, impl_info);
        }
        for (name, trait_info) in source.traits {
            target.traits.insert(name, trait_info);
        }
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
        
        eprintln!("📊 Found {} functions, {} structs, {} impls, {} traits",
            graph.functions.len(),
            graph.structs.len(),
            graph.impls.len(),
            graph.traits.len()
        );
        
        let output_file = match &self.output_path {
            Some(path) => path.clone(),
            None => PathBuf::from("code_graph.json"),
        };
        
        graph.save_for_viewer(&target_dir, &output_file)?;
        
        eprintln!("\x1b[1;32m✓ Graph saved to: {}\x1b[0m", output_file.display());
        eprintln!("\x1b[36m✓ Analysis took {:.2?}\x1b[0m", duration);
        eprintln!("\x1b[33m✓ You can now open this in the Chrome Deps Viewer\x1b[0m");
        
        // Also print the file path for scripting
        println!("{}", output_file.display());
        
        Ok(())
    }
}

fn main() -> Result<()> {
    let app = GraphApp::new();
    app.run()
}