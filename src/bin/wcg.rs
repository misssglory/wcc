// src/bin/wcg.rs
use anyhow::{bail, Context, Result};
use chrono::Local;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};
use syn::{
    parse_file, visit::Visit, Attribute, Field, Fields, GenericParam, Generics, ImplItem,
    ItemImpl, ItemStruct, ItemTrait, ReturnType, Signature, TraitItem, TypeParamBound,
    WhereClause,
};
use wcc::common::*;
use wcc::config::{load_unified_config, WcgConfig};

#[derive(Debug, Clone)]
struct StructInfo {
    name: String,
    fields: Vec<FieldInfo>,
    generics: Vec<String>,
    where_clause: Option<String>,
    attributes: Vec<String>,
    visibility: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct FieldInfo {
    name: String,
    type_str: String,
    visibility: String,
    attributes: Vec<String>,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    signature: String,
    return_type: Option<String>,
    parameters: Vec<ParamInfo>,
    calls: HashSet<String>,
    generics: Vec<String>,
    where_clause: Option<String>,
    attributes: Vec<String>,
    visibility: String,
    asyncness: bool,
    unsafeness: bool,
    constness: bool,
    path: PathBuf,
    in_impl: Option<String>,
}

#[derive(Debug, Clone)]
struct ParamInfo {
    name: String,
    type_str: String,
    is_self: bool,
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
    where_clause: Option<String>,
    super_traits: Vec<String>,
    path: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct CodeGraph {
    structs: HashMap<String, StructInfo>,
    functions: HashMap<String, FunctionInfo>,
    impls: HashMap<String, ImplInfo>,
    traits: HashMap<String, TraitInfo>,
    unresolved_calls: HashMap<String, HashSet<String>>,
}

impl CodeGraph {
    fn add_struct(&mut self, struct_info: StructInfo) {
        self.structs.insert(struct_info.name.clone(), struct_info);
    }

    fn add_function(&mut self, func_info: FunctionInfo) {
        self.functions.insert(func_info.name.clone(), func_info);
    }

    fn add_impl(&mut self, impl_info: ImplInfo) {
        self.impls
            .insert(impl_info.target_type.clone(), impl_info);
    }

    fn add_trait(&mut self, trait_info: TraitInfo) {
        self.traits.insert(trait_info.name.clone(), trait_info);
    }

    fn resolve_calls(&mut self) {
        let mut resolved = HashMap::new();
        
        for (func_name, func_info) in &self.functions {
            let mut resolved_calls = HashSet::new();
            
            for call in &func_info.calls {
                if self.functions.contains_key(call) {
                    resolved_calls.insert(call.clone());
                } else if self.traits.contains_key(call) {
                    resolved_calls.insert(format!("trait::{}", call));
                } else {
                    self.unresolved_calls
                        .entry(func_name.clone())
                        .or_insert_with(HashSet::new)
                        .insert(call.clone());
                }
            }
            
            resolved.insert(func_name.clone(), resolved_calls);
        }
        
        for (func_name, calls) in resolved {
            if let Some(func) = self.functions.get_mut(&func_name) {
                func.calls = calls;
            }
        }
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
            syn::Visibility::Restricted(restricted) => {
                format!("pub({})", quote::quote!(#restricted))
            }
            _ => "private".to_string(),
        }
    }

    fn get_attributes(&self, attrs: &[Attribute]) -> Vec<String> {
        attrs
            .iter()
            .filter_map(|attr| {
                let path = attr.path();
                let is_derive = path.segments.len() == 1 && path.segments[0].ident == "derive";
                if is_derive {
                    None
                } else {
                    Some(quote::quote!(#path).to_string())
                }
            })
            .collect()
    }

    fn parse_generics(&self, generics: &Generics) -> Vec<String> {
        generics
            .params
            .iter()
            .filter_map(|param| match param {
                GenericParam::Type(type_param) => {
                    Some(type_param.ident.to_string())
                }
                GenericParam::Lifetime(lifetime_def) => {
                    Some(lifetime_def.lifetime.ident.to_string())
                }
                GenericParam::Const(const_param) => {
                    Some(const_param.ident.to_string())
                }
            })
            .collect()
    }

    fn parse_where_clause(&self, where_clause: &Option<WhereClause>) -> Option<String> {
        where_clause.as_ref().map(|wc| quote::quote!(#wc).to_string())
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
            parts.push(quote::quote!(#sig.generics).to_string());
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
                syn::FnArg::Typed(pat_type) => {
                    quote::quote!(#pat_type).to_string()
                }
            })
            .collect();
        
        parts.push(format!("({})", params.join(", ")));
        
        match &sig.output {
            ReturnType::Default => {}
            ReturnType::Type(_, ty) => {
                parts.push("->".to_string());
                parts.push(quote::quote!(#ty).to_string());
            }
        }
        
        parts.join(" ")
    }

    fn extract_function_calls(&self, block: &syn::Block) -> HashSet<String> {
        let mut calls = HashSet::new();
        let mut visitor = FunctionCallVisitor::new(&mut calls);
        visitor.visit_block(block);
        calls
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
            if let Some(segment) = expr_path.path.segments.last() {
                self.calls.insert(segment.ident.to_string());
            }
        }
        syn::visit::visit_expr_call(self, expr_call);
    }
}

impl<'ast> Visit<'ast> for CodeGraphVisitor {
    fn visit_item_struct(&mut self, item_struct: &'ast ItemStruct) {
        let struct_name = item_struct.ident.to_string();
        
        let fields = match &item_struct.fields {
            Fields::Named(fields_named) => fields_named
                .named
                .iter()
                .map(|field| {
                    let field_name = field
                        .ident
                        .as_ref()
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "_".to_string());
                    FieldInfo {
                        name: field_name,
                        type_str: quote::quote!(#field.ty).to_string(),
                        visibility: self.get_visibility(&field.vis),
                        attributes: self.get_attributes(&field.attrs),
                    }
                })
                .collect(),
            Fields::Unnamed(fields_unnamed) => fields_unnamed
                .unnamed
                .iter()
                .enumerate()
                .map(|(idx, field)| FieldInfo {
                    name: format!("_{}", idx),
                    type_str: quote::quote!(#field.ty).to_string(),
                    visibility: self.get_visibility(&field.vis),
                    attributes: self.get_attributes(&field.attrs),
                })
                .collect(),
            Fields::Unit => Vec::new(),
        };
        
        let struct_info = StructInfo {
            name: struct_name.clone(),
            fields,
            generics: self.parse_generics(&item_struct.generics),
            where_clause: self.parse_where_clause(&item_struct.generics.where_clause),
            attributes: self.get_attributes(&item_struct.attrs),
            visibility: self.get_visibility(&item_struct.vis),
            path: self.current_file.clone(),
        };
        
        self.graph.add_struct(struct_info);
        syn::visit::visit_item_struct(self, item_struct);
    }
    
    fn visit_item_fn(&mut self, item_fn: &'ast syn::ItemFn) {
        if self.current_impl_target.is_some() {
            return;
        }
        
        let func_name = item_fn.sig.ident.to_string();
        let calls = self.extract_function_calls(&item_fn.block);
        
        let parameters: Vec<ParamInfo> = item_fn
            .sig
            .inputs
            .iter()
            .filter_map(|input| match input {
                syn::FnArg::Typed(pat_type) => {
                    if let syn::Pat::Ident(pat_ident) = &*pat_type.pat {
                        Some(ParamInfo {
                            name: pat_ident.ident.to_string(),
                            type_str: quote::quote!(#pat_type.ty).to_string(),
                            is_self: false,
                        })
                    } else {
                        None
                    }
                }
                syn::FnArg::Receiver(recv) => Some(ParamInfo {
                    name: "self".to_string(),
                    type_str: if recv.reference.is_some() {
                        if recv.mutability.is_some() {
                            "&mut Self".to_string()
                        } else {
                            "&Self".to_string()
                        }
                    } else {
                        "Self".to_string()
                    },
                    is_self: true,
                }),
            })
            .collect();
        
        let return_type = match &item_fn.sig.output {
            ReturnType::Default => None,
            ReturnType::Type(_, ty) => Some(quote::quote!(#ty).to_string()),
        };
        
        let func_info = FunctionInfo {
            name: func_name.clone(),
            signature: self.format_signature(&item_fn.sig),
            return_type,
            parameters,
            calls,
            generics: self.parse_generics(&item_fn.sig.generics),
            where_clause: self.parse_where_clause(&item_fn.sig.generics.where_clause),
            attributes: self.get_attributes(&item_fn.attrs),
            visibility: self.get_visibility(&item_fn.vis),
            asyncness: item_fn.sig.asyncness.is_some(),
            unsafeness: item_fn.sig.unsafety.is_some(),
            constness: item_fn.sig.constness.is_some(),
            path: self.current_file.clone(),
            in_impl: None,
        };
        
        self.graph.add_function(func_info);
        syn::visit::visit_item_fn(self, item_fn);
    }
    
    fn visit_item_impl(&mut self, item_impl: &'ast ItemImpl) {
        let target_type = quote::quote!(#item_impl.self_ty).to_string();
        let old_target = self.current_impl_target.take();
        self.current_impl_target = Some(target_type.clone());
        
        let mut methods = Vec::new();
        
        for item in &item_impl.items {
            if let ImplItem::Fn(method) = item {
                let func_name = method.sig.ident.to_string();
                let calls = self.extract_function_calls(&method.block);
                
                let parameters: Vec<ParamInfo> = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|input| match input {
                        syn::FnArg::Typed(pat_type) => {
                            if let syn::Pat::Ident(pat_ident) = &*pat_type.pat {
                                Some(ParamInfo {
                                    name: pat_ident.ident.to_string(),
                                    type_str: quote::quote!(#pat_type.ty).to_string(),
                                    is_self: false,
                                })
                            } else {
                                None
                            }
                        }
                        syn::FnArg::Receiver(recv) => Some(ParamInfo {
                            name: "self".to_string(),
                            type_str: if recv.reference.is_some() {
                                if recv.mutability.is_some() {
                                    "&mut Self".to_string()
                                } else {
                                    "&Self".to_string()
                                }
                            } else {
                                "Self".to_string()
                            },
                            is_self: true,
                        }),
                    })
                    .collect();
                
                let return_type = match &method.sig.output {
                    ReturnType::Default => None,
                    ReturnType::Type(_, ty) => Some(quote::quote!(#ty).to_string()),
                };
                
                let func_info = FunctionInfo {
                    name: func_name.clone(),
                    signature: self.format_signature(&method.sig),
                    return_type,
                    parameters,
                    calls,
                    generics: self.parse_generics(&method.sig.generics),
                    where_clause: self.parse_where_clause(&method.sig.generics.where_clause),
                    attributes: self.get_attributes(&method.attrs),
                    visibility: self.get_visibility(&method.vis),
                    asyncness: method.sig.asyncness.is_some(),
                    unsafeness: method.sig.unsafety.is_some(),
                    constness: method.sig.constness.is_some(),
                    path: self.current_file.clone(),
                    in_impl: Some(target_type.clone()),
                };
                
                methods.push(func_info.clone());
                self.graph.add_function(func_info);
            }
        }
        
        let traits: Vec<String> = item_impl
            .trait_
            .as_ref()
            .map(|(_, path, _)| quote::quote!(#path).to_string())
            .into_iter()
            .collect();
        
        let impl_info = ImplInfo {
            target_type: target_type.clone(),
            generics: self.parse_generics(&item_impl.generics),
            where_clause: self.parse_where_clause(&item_impl.generics.where_clause),
            methods,
            traits,
            path: self.current_file.clone(),
        };
        
        self.graph.add_impl(impl_info);
        self.current_impl_target = old_target;
        syn::visit::visit_item_impl(self, item_impl);
    }
    
    fn visit_item_trait(&mut self, item_trait: &'ast ItemTrait) {
        let trait_name = item_trait.ident.to_string();
        
        let mut methods = Vec::new();
        
        for item in &item_trait.items {
            if let TraitItem::Fn(method) = item {
                let func_name = method.sig.ident.to_string();
                
                let parameters: Vec<ParamInfo> = method
                    .sig
                    .inputs
                    .iter()
                    .filter_map(|input| match input {
                        syn::FnArg::Typed(pat_type) => {
                            if let syn::Pat::Ident(pat_ident) = &*pat_type.pat {
                                Some(ParamInfo {
                                    name: pat_ident.ident.to_string(),
                                    type_str: quote::quote!(#pat_type.ty).to_string(),
                                    is_self: false,
                                })
                            } else {
                                None
                            }
                        }
                        syn::FnArg::Receiver(recv) => Some(ParamInfo {
                            name: "self".to_string(),
                            type_str: if recv.reference.is_some() {
                                if recv.mutability.is_some() {
                                    "&mut Self".to_string()
                                } else {
                                    "&Self".to_string()
                                }
                            } else {
                                "Self".to_string()
                            },
                            is_self: true,
                        }),
                    })
                    .collect();
                
                let return_type = match &method.sig.output {
                    ReturnType::Default => None,
                    ReturnType::Type(_, ty) => Some(quote::quote!(#ty).to_string()),
                };
                
                let func_info = FunctionInfo {
                    name: func_name.clone(),
                    signature: self.format_signature(&method.sig),
                    return_type,
                    parameters,
                    calls: HashSet::new(),
                    generics: self.parse_generics(&method.sig.generics),
                    where_clause: self.parse_where_clause(&method.sig.generics.where_clause),
                    attributes: self.get_attributes(&method.attrs),
                    visibility: "pub".to_string(),
                    asyncness: method.sig.asyncness.is_some(),
                    unsafeness: method.sig.unsafety.is_some(),
                    constness: method.sig.constness.is_some(),
                    path: self.current_file.clone(),
                    in_impl: None,
                };
                
                methods.push(func_info);
            }
        }
        
        let super_traits: Vec<String> = item_trait
            .supertraits
            .iter()
            .filter_map(|bound| {
                if let TypeParamBound::Trait(trait_bound) = bound {
                    Some(quote::quote!(#trait_bound).to_string())
                } else {
                    None
                }
            })
            .collect();
        
        let trait_info = TraitInfo {
            name: trait_name.clone(),
            methods,
            generics: self.parse_generics(&item_trait.generics),
            where_clause: self.parse_where_clause(&item_trait.generics.where_clause),
            super_traits,
            path: self.current_file.clone(),
        };
        
        self.graph.add_trait(trait_info);
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
                ".idea".to_string(),
                ".vscode".to_string(),
            ],
        }
    }
    
    fn scan_directory(&self, dir: &Path) -> Result<CodeGraph> {
        let mut graph = CodeGraph::default();
        self.walk_directory(dir, &mut graph)?;
        graph.resolve_calls();
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

struct Application {
    config: WcgConfig,
    scanner: CodeGraphScanner,
    show_line_numbers: bool,
    show_calls: bool,
    show_fields: bool,
}

impl Application {
    fn new() -> Result<Self> {
        let unified_config = load_unified_config()?;
        let args: Vec<String> = env::args().collect();
        
        let wcg_config = unified_config.wcg;
        
        let mut show_line_numbers = wcg_config.show_line_numbers;
        let mut show_calls = wcg_config.show_calls;
        let mut show_fields = wcg_config.show_fields;
        
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-n" => show_line_numbers = !wcg_config.show_line_numbers,
                "-c" => show_calls = !wcg_config.show_calls,
                "-f" => show_fields = !wcg_config.show_fields,
                "--show-calls" => show_calls = true,
                "--hide-calls" => show_calls = false,
                "--show-fields" => show_fields = true,
                "--hide-fields" => show_fields = false,
                _ => {}
            }
            i += 1;
        }
        
        Ok(Self {
            config: wcg_config,
            scanner: CodeGraphScanner::new(),
            show_line_numbers,
            show_calls,
            show_fields,
        })
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
        
        self.validate_directory(&target_dir)?;
        
        eprintln!("🔍 Scanning directory: {}", target_dir.display());
        let start = std::time::Instant::now();
        let graph = self.scanner.scan_directory(&target_dir)?;
        let duration = start.elapsed();
        
        eprintln!("✓ Found {} structs, {} functions, {} impls, {} traits",
            graph.structs.len(),
            graph.functions.len(),
            graph.impls.len(),
            graph.traits.len()
        );
        
        let report = self.format_report(&graph, &target_dir);
        let clipboard_content = self.format_clipboard_content(&graph, &target_dir);
        
        println!("{}", report);
        set_clipboard(&clipboard_content)?;
        
        eprintln!("\n\x1b[1;32m✓ Code graph copied to clipboard! (took {:.2?})\x1b[0m", duration);
        
        if self.show_calls {
            eprintln!("\x1b[90mℹ Function calls are shown (use -c to toggle)\x1b[0m");
        }
        if self.show_fields {
            eprintln!("\x1b[90mℹ Struct fields are shown (use -f to toggle)\x1b[0m");
        }
        
        Ok(())
    }
    
    fn validate_directory(&self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            bail!("Directory does not exist: {}", dir.display());
        }
        Ok(())
    }
    
    fn format_report(&self, graph: &CodeGraph, root: &Path) -> String {
        let mut report = String::new();
        
        report.push_str(&format!("\x1b[36m📊 Code Graph Analysis: {}\x1b[0m\n", root.display()));
        report.push_str(&format!("{}\n", "=".repeat(80)));
        
        // Summary
        report.push_str(&format!("\n\x1b[36m📈 Summary:\x1b[0m\n"));
        report.push_str(&format!("  Structs: \x1b[33m{}\x1b[0m\n", graph.structs.len()));
        report.push_str(&format!("  Functions: \x1b[33m{}\x1b[0m\n", graph.functions.len()));
        report.push_str(&format!("  Implementations: \x1b[33m{}\x1b[0m\n", graph.impls.len()));
        report.push_str(&format!("  Traits: \x1b[33m{}\x1b[0m\n", graph.traits.len()));
        
        if !graph.unresolved_calls.is_empty() {
            let total_unresolved: usize = graph.unresolved_calls.values().map(|v| v.len()).sum();
            report.push_str(&format!("  Unresolved function calls: \x1b[33m{}\x1b[0m\n", total_unresolved));
        }
        
        // Structs
        if !graph.structs.is_empty() {
            report.push_str(&format!("\n\x1b[36m📦 Structs:\x1b[0m\n"));
            report.push_str(&format!("{}\n", "-".repeat(80)));
            
            for struct_info in graph.structs.values() {
                let rel_path = struct_info.path.strip_prefix(root).unwrap_or(&struct_info.path);
                
                report.push_str(&format!(
                    "\n  \x1b[32m{}struct {}{}\x1b[0m",
                    if struct_info.visibility == "pub" { "pub " } else { "" },
                    struct_info.name,
                    if !struct_info.generics.is_empty() {
                        format!("<{}>", struct_info.generics.join(", "))
                    } else {
                        String::new()
                    }
                ));
                
                report.push_str(&format!(" \x1b[90m({})\x1b[0m", rel_path.display()));
                report.push('\n');
                
                if self.show_fields && !struct_info.fields.is_empty() {
                    report.push_str(&format!("    \x1b[36mFields:\x1b[0m\n"));
                    for field in &struct_info.fields {
                        report.push_str(&format!(
                            "      {}{}: \x1b[33m{}\x1b[0m",
                            if field.visibility == "pub" { "\x1b[32mpub \x1b[0m" } else { "" },
                            field.name,
                            field.type_str
                        ));
                        if !field.attributes.is_empty() {
                            report.push_str(&format!(" \x1b[90m[{}]\x1b[0m", field.attributes.join(", ")));
                        }
                        report.push('\n');
                    }
                }
                
                if let Some(where_clause) = &struct_info.where_clause {
                    report.push_str(&format!("    where {}\n", where_clause));
                }
            }
        }
        
        // Traits
        if !graph.traits.is_empty() {
            report.push_str(&format!("\n\x1b[36m🔧 Traits:\x1b[0m\n"));
            report.push_str(&format!("{}\n", "-".repeat(80)));
            
            for trait_info in graph.traits.values() {
                let rel_path = trait_info.path.strip_prefix(root).unwrap_or(&trait_info.path);
                
                report.push_str(&format!(
                    "\n  \x1b[32mtrait {}{}\x1b[0m",
                    trait_info.name,
                    if !trait_info.generics.is_empty() {
                        format!("<{}>", trait_info.generics.join(", "))
                    } else {
                        String::new()
                    }
                ));
                
                if !trait_info.super_traits.is_empty() {
                    report.push_str(&format!(": {}", trait_info.super_traits.join(" + ")));
                }
                
                report.push_str(&format!(" \x1b[90m({})\x1b[0m", rel_path.display()));
                report.push('\n');
                
                if !trait_info.methods.is_empty() {
                    report.push_str(&format!("    \x1b[36mMethods:\x1b[0m\n"));
                    for method in &trait_info.methods {
                        report.push_str(&format!("      • \x1b[33m{}\x1b[0m\n", method.signature));
                        if self.show_calls && !method.calls.is_empty() {
                            let calls_str: Vec<String> = method.calls.iter()
                                .map(|c| format!("\x1b[35m`{}`\x1b[0m", c))
                                .collect();
                            report.push_str(&format!("        Calls: {}\n", calls_str.join(", ")));
                        }
                    }
                }
            }
        }
        
        // Functions (top-level)
        let top_level_functions: Vec<&FunctionInfo> = graph.functions
            .values()
            .filter(|f| f.in_impl.is_none())
            .collect();
        
        if !top_level_functions.is_empty() {
            report.push_str(&format!("\n\x1b[36m⚡ Functions:\x1b[0m\n"));
            report.push_str(&format!("{}\n", "-".repeat(80)));
            
            for func in top_level_functions {
                let rel_path = func.path.strip_prefix(root).unwrap_or(&func.path);
                
                report.push_str(&format!(
                    "\n  \x1b[32m{}{}\x1b[0m",
                    if func.visibility == "pub" { "pub " } else { "" },
                    func.signature
                ));
                
                report.push_str(&format!(" \x1b[90m({})\x1b[0m", rel_path.display()));
                report.push('\n');
                
                if self.show_calls && !func.calls.is_empty() {
                    let calls_str: Vec<String> = func.calls.iter()
                        .map(|c| format!("\x1b[35m`{}`\x1b[0m", c))
                        .collect();
                    report.push_str(&format!("    Calls: {}\n", calls_str.join(", ")));
                }
            }
        }
        
        // Implementations
        if !graph.impls.is_empty() {
            report.push_str(&format!("\n\x1b[36m🏭 Implementations:\x1b[0m\n"));
            report.push_str(&format!("{}\n", "-".repeat(80)));
            
            for impl_info in graph.impls.values() {
                let rel_path = impl_info.path.strip_prefix(root).unwrap_or(&impl_info.path);
                
                report.push_str(&format!(
                    "\n  \x1b[32mimpl{}{}\x1b[0m",
                    if !impl_info.generics.is_empty() {
                        format!("<{}>", impl_info.generics.join(", "))
                    } else {
                        String::new()
                    },
                    impl_info.target_type
                ));
                
                if !impl_info.traits.is_empty() {
                    report.push_str(&format!(": {}", impl_info.traits.join(" + ")));
                }
                
                report.push_str(&format!(" \x1b[90m({})\x1b[0m", rel_path.display()));
                report.push('\n');
                
                if !impl_info.methods.is_empty() {
                    report.push_str(&format!("    \x1b[36mMethods:\x1b[0m\n"));
                    for method in &impl_info.methods {
                        report.push_str(&format!("      • \x1b[33m{}\x1b[0m\n", method.signature));
                        if self.show_calls && !method.calls.is_empty() {
                            let calls_str: Vec<String> = method.calls.iter()
                                .map(|c| format!("\x1b[35m`{}`\x1b[0m", c))
                                .collect();
                            report.push_str(&format!("        Calls: {}\n", calls_str.join(", ")));
                        }
                    }
                }
            }
        }
        
        // Unresolved calls
        if !graph.unresolved_calls.is_empty() && self.show_calls {
            report.push_str(&format!("\n\x1b[36m❓ Unresolved Function Calls:\x1b[0m\n"));
            report.push_str(&format!("{}\n", "-".repeat(80)));
            
            for (func_name, calls) in &graph.unresolved_calls {
                if !calls.is_empty() {
                    report.push_str(&format!("\n  \x1b[33m{}\x1b[0m calls:\n", func_name));
                    for call in calls {
                        report.push_str(&format!("    • \x1b[31m{}\x1b[0m (unresolved)\n", call));
                    }
                }
            }
        }
        
        report.push_str(&format!("\n{}\n", "=".repeat(80)));
        report
    }
    
    fn format_clipboard_content(&self, graph: &CodeGraph, root: &Path) -> String {
        let mut content = String::new();
        let timestamp = Local::now();
        
        content.push_str(&format!("// Code Graph Analysis\n"));
        content.push_str(&format!("// Generated: {}\n", timestamp.format("%Y-%m-%d %H:%M:%S")));
        content.push_str(&format!("// Root: {}\n", root.display()));
        content.push_str(&format!("// ============================================================\n\n"));
        
        // Structs section
        if !graph.structs.is_empty() {
            content.push_str("// 📦 STRUCTS\n");
            content.push_str("// ============================================================\n\n");
            
            for struct_info in graph.structs.values() {
                let rel_path = struct_info.path.strip_prefix(root).unwrap_or(&struct_info.path);
                
                content.push_str(&format!(
                    "{}{}struct {}{} // {}\n",
                    if struct_info.visibility == "pub" { "pub " } else { "" },
                    if !struct_info.attributes.is_empty() {
                        format!("#[{}] ", struct_info.attributes.join("]["))
                    } else {
                        String::new()
                    },
                    struct_info.name,
                    if !struct_info.generics.is_empty() {
                        format!("<{}>", struct_info.generics.join(", "))
                    } else {
                        String::new()
                    },
                    rel_path.display()
                ));
                
                if self.show_fields && !struct_info.fields.is_empty() {
                    content.push_str("{\n");
                    for field in &struct_info.fields {
                        content.push_str(&format!(
                            "    {}{}: {},\n",
                            if field.visibility == "pub" { "pub " } else { "" },
                            field.name,
                            field.type_str
                        ));
                    }
                    content.push_str("}\n");
                } else {
                    content.push_str("{ /* fields omitted */ }\n");
                }
                
                if let Some(where_clause) = &struct_info.where_clause {
                    content.push_str(&format!("where {}\n", where_clause));
                }
                content.push('\n');
            }
        }
        
        // Traits section
        if !graph.traits.is_empty() {
            content.push_str("// 🔧 TRAITS\n");
            content.push_str("// ============================================================\n\n");
            
            for trait_info in graph.traits.values() {
                let rel_path = trait_info.path.strip_prefix(root).unwrap_or(&trait_info.path);
                
                content.push_str(&format!(
                    "trait {}{} // {}\n",
                    trait_info.name,
                    if !trait_info.generics.is_empty() {
                        format!("<{}>", trait_info.generics.join(", "))
                    } else {
                        String::new()
                    },
                    rel_path.display()
                ));
                
                if !trait_info.super_traits.is_empty() {
                    content.push_str(&format!(": {}\n", trait_info.super_traits.join(" + ")));
                }
                
                content.push_str("{\n");
                
                for method in &trait_info.methods {
                    content.push_str(&format!("    {};\n", method.signature));
                }
                
                content.push_str("}\n\n");
            }
        }
        
        // Functions section
        let top_level_functions: Vec<&FunctionInfo> = graph.functions
            .values()
            .filter(|f| f.in_impl.is_none())
            .collect();
        
        if !top_level_functions.is_empty() {
            content.push_str("// ⚡ FUNCTIONS\n");
            content.push_str("// ============================================================\n\n");
            
            for func in top_level_functions {
                let rel_path = func.path.strip_prefix(root).unwrap_or(&func.path);
                
                content.push_str(&format!(
                    "{}{} {{ /* {} */ }}\n",
                    if func.visibility == "pub" { "pub " } else { "" },
                    func.signature,
                    rel_path.display()
                ));
                
                if self.show_calls && !func.calls.is_empty() {
                    let calls_vec: Vec<String> = func.calls.iter().cloned().collect();
                    content.push_str(&format!("    // Calls: {}\n", calls_vec.join(", ")));
                }
                content.push('\n');
            }
        }
        
        // Implementations section
        if !graph.impls.is_empty() {
            content.push_str("// 🏭 IMPLEMENTATIONS\n");
            content.push_str("// ============================================================\n\n");
            
            for impl_info in graph.impls.values() {
                let rel_path = impl_info.path.strip_prefix(root).unwrap_or(&impl_info.path);
                
                content.push_str(&format!(
                    "impl{}{} // {}\n",
                    if !impl_info.generics.is_empty() {
                        format!("<{}>", impl_info.generics.join(", "))
                    } else {
                        String::new()
                    },
                    impl_info.target_type,
                    rel_path.display()
                ));
                
                if !impl_info.traits.is_empty() {
                    content.push_str(&format!(": {}\n", impl_info.traits.join(" + ")));
                }
                
                content.push_str("{\n");
                
                for method in &impl_info.methods {
                    content.push_str(&format!("    {} {{\n", method.signature));
                    if self.show_calls && !method.calls.is_empty() {
                        let calls_vec: Vec<String> = method.calls.iter().cloned().collect();
                        content.push_str(&format!("        // Calls: {}\n", calls_vec.join(", ")));
                    }
                    content.push_str("    }\n\n");
                }
                
                content.push_str("}\n\n");
            }
        }
        
        content
    }
}

fn main() -> Result<()> {
    let app = Application::new()?;
    app.run()
}