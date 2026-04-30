use anyhow::{bail, Context, Result};
use chrono::Local;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};
use syn::{
    parse_file, visit::Visit, Attribute, Field, Fields, GenericParam, Generics, ImplItem, ItemImpl,
    ItemStruct, ItemTrait, ReturnType, Signature, TraitItem, TypeParamBound, WhereClause,
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
    structs: BTreeMap<String, StructInfo>,
    functions: BTreeMap<String, FunctionInfo>,
    impls: BTreeMap<String, ImplInfo>,
    traits: BTreeMap<String, TraitInfo>,
    unresolved_calls: BTreeMap<String, HashSet<String>>,
    file_imports: BTreeMap<PathBuf, FileImports>,
}
impl CodeGraph {
    fn add_struct(&mut self, struct_info: StructInfo) {
        self.structs.insert(struct_info.name.clone(), struct_info);
    }
    fn add_function(&mut self, func_info: FunctionInfo) {
        self.functions.insert(func_info.name.clone(), func_info);
    }
    fn add_impl(&mut self, impl_info: ImplInfo) {
        self.impls.insert(impl_info.target_type.clone(), impl_info);
    }
    fn add_trait(&mut self, trait_info: TraitInfo) {
        self.traits.insert(trait_info.name.clone(), trait_info);
    }
    fn resolve_imported_call(&self, caller: &FunctionInfo, call: &str) -> Option<String> {
        let imports = self.file_imports.get(&caller.path)?;
        for import in &imports.imports {
            if !import_matches_call(import, call) {
                continue;
            }
            let qualified = qualify_call_from_import(import, call);
            if self.functions.contains_key(&qualified) || self.traits.contains_key(&qualified) {
                return Some(qualified);
            }
            if self.functions.contains_key(call) || self.traits.contains_key(call) {
                return Some(call.to_string());
            }
            if import.is_glob {
                let candidate = format!("{}::{}", import.full_path.trim_end_matches("::*"), call);
                if self.functions.contains_key(&candidate) || self.traits.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }
    fn resolve_calls(&mut self) {
        let mut resolved = HashMap::new();
        self.unresolved_calls.clear();
        for (func_name, func_info) in &self.functions {
            let mut resolved_calls = HashSet::new();
            for call in &func_info.calls {
                if is_ignored_call(call) {
                    continue;
                }
                if self.functions.contains_key(call) {
                    resolved_calls.insert(call.clone());
                } else if self.traits.contains_key(call) {
                    resolved_calls.insert(format!("trait::{}", call));
                } else if let Some(imported) = self.resolve_imported_call(func_info, call) {
                    resolved_calls.insert(imported);
                } else {
                    self.unresolved_calls
                        .entry(func_name.clone())
                        .or_default()
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
    fn add_import(&mut self, path: PathBuf, import: ImportInfo) {
        self.file_imports
            .entry(path)
            .or_default()
            .imports
            .push(import);
    }
    fn render_file_imports(&self, path: &Path, root: &Path) -> String {
        let mut out = String::new();
        let rel_path = path.strip_prefix(root).unwrap_or(path);
        out.push_str(&format!("// imports: {}\n", rel_path.display()));
        if let Some(file_imports) = self.file_imports.get(path) {
            let mut items: Vec<String> = file_imports
                .imports
                .iter()
                .map(|i| {
                    if let Some(alias) = &i.alias {
                        format!("use {} as {};", i.full_path, alias)
                    } else {
                        format!("use {};", i.full_path)
                    }
                })
                .collect();
            items.sort();
            items.dedup();
            for item in items {
                out.push_str(&format!("//   {}\n", item));
            }
        } else {
            out.push_str("//   (no imports found)\n");
        }
        out.push('\n');
        out
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
                format!("pub({})", quote::quote!(# restricted))
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
                    Some(quote::quote!(# path).to_string())
                }
            })
            .collect()
    }
    fn parse_generics(&self, generics: &Generics) -> Vec<String> {
        generics
            .params
            .iter()
            .filter_map(|param| match param {
                GenericParam::Type(type_param) => Some(type_param.ident.to_string()),
                GenericParam::Lifetime(lifetime_def) => {
                    Some(lifetime_def.lifetime.ident.to_string())
                }
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
                "Ok" | "Err"
                    | "Some"
                    | "None"
                    | "Self"
                    | "default"
                    | "new"
                    | "into"
                    | "from"
                    | "try_from"
                    | "try_into"
                    | "as_ref"
                    | "as_mut"
                    | "as_deref"
                    | "borrow"
                    | "to_owned"
                    | "to_string"
                    | "clone"
                    | "cloned"
                    | "copied"
                    | "iter"
                    | "iter_mut"
                    | "into_iter"
                    | "any"
                    | "all"
                    | "map"
                    | "filter"
                    | "filter_map"
                    | "find"
                    | "find_map"
                    | "flat_map"
                    | "fold"
                    | "for_each"
                    | "enumerate"
                    | "zip"
                    | "skip"
                    | "take"
                    | "collect"
                    | "collect::<Vec<_>>"
                    | "count"
                    | "len"
                    | "is_empty"
                    | "is_some"
                    | "is_none"
                    | "contains"
                    | "starts_with"
                    | "ends_with"
                    | "trim"
                    | "trim_start"
                    | "trim_end"
                    | "split"
                    | "split_whitespace"
                    | "join"
                    | "lines"
                    | "chars"
                    | "bytes"
                    | "push"
                    | "push_str"
                    | "insert"
                    | "remove"
                    | "get"
                    | "get_mut"
                    | "entry"
                    | "or_insert"
                    | "or_insert_with"
                    | "unwrap"
                    | "unwrap_or"
                    | "unwrap_or_else"
                    | "expect"
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
    fn extract_call_path(&self, expr: &syn::Expr) -> Option<String> {
        match expr {
            syn::Expr::Path(expr_path) => {
                let path = &expr_path.path;
                let segments: Vec<String> = path
                    .segments
                    .iter()
                    .map(|seg| seg.ident.to_string())
                    .collect();
                Some(segments.join("::"))
            }
            syn::Expr::MethodCall(method_call) => Some(method_call.method.to_string()),
            _ => None,
        }
    }
}
impl<'a> Visit<'a> for FunctionCallVisitor<'a> {
    fn visit_expr_call(&mut self, expr_call: &'a syn::ExprCall) {
        if let Some(call_path) = self.extract_call_path(&expr_call.func) {
            self.calls.insert(call_path);
        }
        syn::visit::visit_expr_call(self, expr_call);
    }
    fn visit_expr_method_call(&mut self, method_call: &'a syn::ExprMethodCall) {
        let method_name = method_call.method.to_string();
        self.calls.insert(method_name);
        syn::visit::visit_expr_method_call(self, method_call);
    }
}
impl<'ast> Visit<'ast> for CodeGraphVisitor {
    fn visit_item_fn(&mut self, item_fn: &'ast syn::ItemFn) {
        use quote::ToTokens;
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
                            type_str: pat_type.ty.as_ref().to_token_stream().to_string(),
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
            ReturnType::Type(_, ty) => Some(ty.as_ref().to_token_stream().to_string()),
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
        use quote::ToTokens;
        let target_type = item_impl.self_ty.as_ref().to_token_stream().to_string();
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
                                    type_str: pat_type.ty.as_ref().to_token_stream().to_string(),
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
                    ReturnType::Type(_, ty) => Some(ty.as_ref().to_token_stream().to_string()),
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
        self.graph.add_impl(impl_info);
        self.current_impl_target = old_target;
        syn::visit::visit_item_impl(self, item_impl);
    }
    fn visit_item_use(&mut self, item_use: &'ast syn::ItemUse) {
        let mut imports = Vec::new();
        collect_use_tree(String::new(), &item_use.tree, &mut imports);
        for import in imports {
            self.graph.add_import(self.current_file.clone(), import);
        }
        syn::visit::visit_item_use(self, item_use);
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
        for (path, file_imports) in source.file_imports {
            target
                .file_imports
                .entry(path)
                .or_default()
                .imports
                .extend(file_imports.imports);
        }
    }
}
#[derive(Debug, Clone, Copy)]
struct ClipboardStats {
    lines: usize,
    words: usize,
    bytes: usize,
}
fn calc_clipboard_stats(s: &str) -> ClipboardStats {
    ClipboardStats {
        lines: s.lines().count(),
        words: s.split_whitespace().count(),
        bytes: s.as_bytes().len(),
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
        eprintln!("Scanning directory: {}", target_dir.display());
        let start = std::time::Instant::now();
        let graph = self.scanner.scan_directory(&target_dir)?;
        let duration = start.elapsed();
        eprintln!(
            "Found {} structs, {} functions, {} impls, {} traits",
            graph.structs.len(),
            graph.functions.len(),
            graph.impls.len(),
            graph.traits.len()
        );
        let report = self.format_report(&graph, &target_dir);
        let clipboard_content = self.format_clipboard_content(&graph, &target_dir);
        println!("{}", report);
        set_clipboard(&clipboard_content)?;
        let stats = calc_clipboard_stats(&clipboard_content);
        eprintln!(
            "\n\x1b[1;32m✓ Code graph copied to clipboard! (took {:.2?})\x1b[0m",
            duration
        );
        eprintln!(
            "\x1b[90m└─ clipboard:\x1b[0m  \x1b[36m{} lines\x1b[0m  \x1b[33m{} words\x1b[0m  \x1b[35m{} bytes\x1b[0m",
            stats.lines, stats.words, stats.bytes
        );
        if self.show_line_numbers {
            eprintln!("\x1b[90mLine numbers are shown; use -n to toggle\x1b[0m");
        }
        if self.show_calls {
            eprintln!("\x1b[90mFunction calls are shown; use -c to toggle\x1b[0m");
        }
        if self.show_fields {
            eprintln!("\x1b[90mStruct fields are shown; use -f to toggle\x1b[0m");
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
        report.push_str(&format!(
            "\x1b[36m📊 Code Graph Analysis: {}\x1b[0m\n",
            root.display()
        ));
        report.push_str(&format!("{}\n", "=".repeat(80)));
        report.push_str("\n\x1b[36m📈 Summary:\x1b[0m\n");
        report.push_str(&format!(
            "  Structs: \x1b[33m{}\x1b[0m\n",
            graph.structs.len()
        ));
        report.push_str(&format!(
            "  Functions: \x1b[33m{}\x1b[0m\n",
            graph.functions.len()
        ));
        report.push_str(&format!(
            "  Implementations: \x1b[33m{}\x1b[0m\n",
            graph.impls.len()
        ));
        report.push_str(&format!(
            "  Traits: \x1b[33m{}\x1b[0m\n",
            graph.traits.len()
        ));
        if !graph.unresolved_calls.is_empty() {
            let total_unresolved: usize = graph.unresolved_calls.values().map(|v| v.len()).sum();
            report.push_str(&format!(
                "  Unresolved function calls: \x1b[33m{}\x1b[0m\n",
                total_unresolved
            ));
        }
        if !graph.structs.is_empty() {
            report.push_str("\n\x1b[36m📦 Structs:\x1b[0m\n");
            report.push_str(&format!("{}\n", "-".repeat(80)));
            for struct_info in graph.structs.values() {
                let rel_path = struct_info
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&struct_info.path);
                let visibility = if struct_info.visibility == "pub" {
                    "pub "
                } else {
                    ""
                };
                let generics = if !struct_info.generics.is_empty() {
                    format!("<{}>", struct_info.generics.join(", "))
                } else {
                    String::new()
                };
                report.push_str(&format!(
                    "\n  \x1b[32m{}struct {}{}\x1b[0m \x1b[90m({})\x1b[0m\n",
                    visibility,
                    struct_info.name,
                    generics,
                    rel_path.display()
                ));
                if self.show_fields && !struct_info.fields.is_empty() {
                    report.push_str("    \x1b[36mFields:\x1b[0m\n");
                    for field in &struct_info.fields {
                        let field_vis = if field.visibility == "pub" {
                            "\x1b[32mpub \x1b[0m"
                        } else {
                            ""
                        };
                        report.push_str(&format!(
                            "      {}{}: \x1b[33m{}\x1b[0m\n",
                            field_vis, field.name, field.type_str
                        ));
                    }
                }
                if let Some(where_clause) = &struct_info.where_clause {
                    report.push_str(&format!("    where {}\n", where_clause));
                }
            }
        }
        if !graph.traits.is_empty() {
            report.push_str("\n\x1b[36m🔧 Traits:\x1b[0m\n");
            report.push_str(&format!("{}\n", "-".repeat(80)));
            for trait_info in graph.traits.values() {
                let rel_path = trait_info
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&trait_info.path);
                let generics = if !trait_info.generics.is_empty() {
                    format!("<{}>", trait_info.generics.join(", "))
                } else {
                    String::new()
                };
                report.push_str(&format!(
                    "\n  \x1b[32mtrait {}{}\x1b[0m",
                    trait_info.name, generics
                ));
                if !trait_info.super_traits.is_empty() {
                    report.push_str(&format!(": {}", trait_info.super_traits.join(" + ")));
                }
                report.push_str(&format!(" \x1b[90m({})\x1b[0m\n", rel_path.display()));
                if !trait_info.methods.is_empty() {
                    report.push_str("    \x1b[36mMethods:\x1b[0m\n");
                    for method in &trait_info.methods {
                        report.push_str(&format!("      • \x1b[33m{}\x1b[0m\n", method.signature));
                    }
                }
            }
        }
        let top_level_functions: Vec<&FunctionInfo> = graph
            .functions
            .values()
            .filter(|f| f.in_impl.is_none())
            .collect();
        if !top_level_functions.is_empty() {
            report.push_str("\n\x1b[36m⚡ Functions:\x1b[0m\n");
            report.push_str(&format!("{}\n", "-".repeat(80)));
            for func in top_level_functions {
                let rel_path = func.path.strip_prefix(root).unwrap_or(&func.path);
                let visibility = if func.visibility == "pub" { "pub " } else { "" };
                report.push_str(&format!(
                    "\n  \x1b[32m{}{}\x1b[0m \x1b[90m({})\x1b[0m\n",
                    visibility,
                    func.signature,
                    rel_path.display()
                ));
                if self.show_calls && !func.calls.is_empty() {
                    let mut calls_vec: Vec<_> = func.calls.iter().cloned().collect();
                    calls_vec.sort();
                    let calls_str: Vec<String> = calls_vec
                        .into_iter()
                        .map(|c| format!("\x1b[35m`{}`\x1b[0m", c))
                        .collect();
                    report.push_str(&format!("    Calls: {}\n", calls_str.join(", ")));
                }
            }
        }
        if !graph.impls.is_empty() {
            report.push_str("\n\x1b[36m🏭 Implementations:\x1b[0m\n");
            report.push_str(&format!("{}\n", "-".repeat(80)));
            for impl_info in graph.impls.values() {
                let rel_path = impl_info.path.strip_prefix(root).unwrap_or(&impl_info.path);
                let generics = if !impl_info.generics.is_empty() {
                    format!("<{}>", impl_info.generics.join(", "))
                } else {
                    String::new()
                };
                let header = if !impl_info.traits.is_empty() {
                    format!(
                        "impl{} {} for {}",
                        generics,
                        impl_info.traits.join(" + "),
                        impl_info.target_type
                    )
                } else {
                    format!("impl{} {}", generics, impl_info.target_type)
                };
                report.push_str(&format!(
                    "\n  \x1b[32m{}\x1b[0m \x1b[90m({})\x1b[0m\n",
                    header,
                    rel_path.display()
                ));
                if let Some(where_clause) = &impl_info.where_clause {
                    report.push_str(&format!("    where {}\n", where_clause));
                }
                if !impl_info.methods.is_empty() {
                    report.push_str("    \x1b[36mMethods:\x1b[0m\n");
                    for method in &impl_info.methods {
                        report.push_str(&format!("      • \x1b[33m{}\x1b[0m\n", method.signature));
                        if self.show_calls && !method.calls.is_empty() {
                            let mut calls_vec: Vec<_> = method.calls.iter().cloned().collect();
                            calls_vec.sort();
                            let calls_str: Vec<String> = calls_vec
                                .into_iter()
                                .map(|c| format!("\x1b[35m`{}`\x1b[0m", c))
                                .collect();
                            report.push_str(&format!("        Calls: {}\n", calls_str.join(", ")));
                        }
                    }
                }
            }
        }
        if !graph.unresolved_calls.is_empty() && self.show_calls {
            report.push_str("\n\x1b[36m❓ Unresolved Function Calls:\x1b[0m\n");
            report.push_str(&format!("{}\n", "-".repeat(80)));
            for (func_name, calls) in &graph.unresolved_calls {
                if !calls.is_empty() {
                    report.push_str(&format!("\n  \x1b[33m{}\x1b[0m calls:\n", func_name));
                    let mut calls_vec: Vec<_> = calls.iter().cloned().collect();
                    calls_vec.sort();
                    for call in calls_vec {
                        report.push_str(&format!("    • \x1b[31m{}\x1b[0m\n", call));
                    }
                }
            }
        }
        report.push_str(&format!("\n{}\n", "=".repeat(80)));
        report
    }
    fn format_clipboard_content(&self, graph: &CodeGraph, root: &Path) -> String {
        use std::collections::HashSet;
        let mut content = String::new();
        let mut printed_imports: HashSet<PathBuf> = HashSet::new();
        let timestamp = Local::now();
        content.push_str("// Code Graph Analysis\n");
        content.push_str(&format!(
            "// Generated: {}\n",
            timestamp.format("%Y-%m-%d %H:%M:%S")
        ));
        content.push_str(&format!("// Root: {}\n", root.display()));
        content.push_str("// ============================================================\n\n");
        if !graph.structs.is_empty() {
            content.push_str("// 📦 STRUCTS\n");
            content.push_str("// ============================================================\n\n");
            for struct_info in graph.structs.values() {
                let rel_path = struct_info
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&struct_info.path);
                if printed_imports.insert(struct_info.path.clone()) {
                    content.push_str(&graph.render_file_imports(&struct_info.path, root));
                }
                let visibility = if struct_info.visibility == "pub" {
                    "pub "
                } else {
                    ""
                };
                let generics = if !struct_info.generics.is_empty() {
                    format!("<{}>", struct_info.generics.join(", "))
                } else {
                    String::new()
                };
                content.push_str(&format!("// {}\n", rel_path.display()));
                content.push_str(&format!(
                    "{}struct {}{}",
                    visibility, struct_info.name, generics
                ));
                if let Some(where_clause) = &struct_info.where_clause {
                    content.push_str(&format!(" {}\n", where_clause));
                } else {
                    content.push('\n');
                }
                content.push_str("{\n");
                if self.show_fields && !struct_info.fields.is_empty() {
                    for field in &struct_info.fields {
                        let field_vis = if field.visibility == "pub" {
                            "pub "
                        } else {
                            ""
                        };
                        content.push_str(&format!(
                            "    {}{}: {},\n",
                            field_vis, field.name, field.type_str
                        ));
                    }
                } else {
                    content.push_str("    // fields omitted\n");
                }
                content.push_str("}\n\n");
            }
        }
        if !graph.traits.is_empty() {
            content.push_str("// 🔧 TRAITS\n");
            content.push_str("// ============================================================\n\n");
            for trait_info in graph.traits.values() {
                let rel_path = trait_info
                    .path
                    .strip_prefix(root)
                    .unwrap_or(&trait_info.path);
                if printed_imports.insert(trait_info.path.clone()) {
                    content.push_str(&graph.render_file_imports(&trait_info.path, root));
                }
                let generics = if !trait_info.generics.is_empty() {
                    format!("<{}>", trait_info.generics.join(", "))
                } else {
                    String::new()
                };
                content.push_str(&format!("// {}\n", rel_path.display()));
                content.push_str(&format!("trait {}{}", trait_info.name, generics));
                if !trait_info.super_traits.is_empty() {
                    content.push_str(&format!(": {}", trait_info.super_traits.join(" + ")));
                }
                content.push_str(" {\n");
                for method in &trait_info.methods {
                    content.push_str(&format!("    {};\n", method.signature));
                }
                content.push_str("}\n\n");
            }
        }
        let top_level_functions: Vec<&FunctionInfo> = graph
            .functions
            .values()
            .filter(|f| f.in_impl.is_none())
            .collect();
        if !top_level_functions.is_empty() {
            content.push_str("// ⚡ FUNCTIONS\n");
            content.push_str("// ============================================================\n\n");
            for func in top_level_functions {
                let rel_path = func.path.strip_prefix(root).unwrap_or(&func.path);
                if printed_imports.insert(func.path.clone()) {
                    content.push_str(&graph.render_file_imports(&func.path, root));
                }
                let visibility = if func.visibility == "pub" { "pub " } else { "" };
                content.push_str(&format!("// {}\n", rel_path.display()));
                content.push_str(&format!("{}{} {{\n", visibility, func.signature));
                if self.show_calls && !func.calls.is_empty() {
                    let mut calls_vec: Vec<_> = func.calls.iter().cloned().collect();
                    calls_vec.sort();
                    content.push_str(&format!("    // Calls: {}\n", calls_vec.join(", ")));
                }
                content.push_str("}\n\n");
            }
        }
        if !graph.impls.is_empty() {
            content.push_str("// 🏭 IMPLEMENTATIONS\n");
            content.push_str("// ============================================================\n\n");
            for impl_info in graph.impls.values() {
                let rel_path = impl_info.path.strip_prefix(root).unwrap_or(&impl_info.path);
                if printed_imports.insert(impl_info.path.clone()) {
                    content.push_str(&graph.render_file_imports(&impl_info.path, root));
                }
                let generics = if !impl_info.generics.is_empty() {
                    format!("<{}>", impl_info.generics.join(", "))
                } else {
                    String::new()
                };
                let header = if !impl_info.traits.is_empty() {
                    format!(
                        "impl{} {} for {}",
                        generics,
                        impl_info.traits.join(" + "),
                        impl_info.target_type
                    )
                } else {
                    format!("impl{} {}", generics, impl_info.target_type)
                };
                content.push_str(&format!("// {}\n", rel_path.display()));
                content.push_str(&header);
                if let Some(where_clause) = &impl_info.where_clause {
                    content.push_str(&format!(" {}\n", where_clause));
                } else {
                    content.push('\n');
                }
                content.push_str("{\n");
                for method in &impl_info.methods {
                    content.push_str(&format!("    {};\n", method.signature));
                    if self.show_calls && !method.calls.is_empty() {
                        let mut calls_vec: Vec<_> = method.calls.iter().cloned().collect();
                        calls_vec.sort();
                        content.push_str(&format!("        // Calls: {}\n", calls_vec.join(", ")));
                    }
                }
                content.push_str("}\n\n");
            }
        }
        if !graph.unresolved_calls.is_empty() && self.show_calls {
            content.push_str("// ❓ UNRESOLVED FUNCTION CALLS\n");
            content.push_str("// ============================================================\n\n");
            for (func_name, calls) in &graph.unresolved_calls {
                if !calls.is_empty() {
                    content.push_str(&format!("// {} calls:\n", func_name));
                    let mut calls_vec: Vec<_> = calls.iter().cloned().collect();
                    calls_vec.sort();
                    for call in calls_vec {
                        content.push_str(&format!("//   - {}\n", call));
                    }
                    content.push('\n');
                }
            }
        }
        content
    }
}
fn collect_use_tree(prefix: String, tree: &syn::UseTree, out: &mut Vec<ImportInfo>) {
    match tree {
        syn::UseTree::Path(use_path) => {
            let new_prefix = if prefix.is_empty() {
                use_path.ident.to_string()
            } else {
                format!("{}::{}", prefix, use_path.ident)
            };
            collect_use_tree(new_prefix, &use_path.tree, out);
        }
        syn::UseTree::Name(use_name) => {
            let full = if prefix.is_empty() {
                use_name.ident.to_string()
            } else {
                format!("{}::{}", prefix, use_name.ident)
            };
            out.push(ImportInfo {
                full_path: full.clone(),
                alias: None,
                last_segment: use_name.ident.to_string(),
                is_glob: false,
            });
        }
        syn::UseTree::Rename(use_rename) => {
            let full = if prefix.is_empty() {
                use_rename.ident.to_string()
            } else {
                format!("{}::{}", prefix, use_rename.ident)
            };
            out.push(ImportInfo {
                full_path: full,
                alias: Some(use_rename.rename.to_string()),
                last_segment: use_rename.ident.to_string(),
                is_glob: false,
            });
        }
        syn::UseTree::Glob(_) => {
            out.push(ImportInfo {
                full_path: format!("{}::*", prefix),
                alias: None,
                last_segment: "*".to_string(),
                is_glob: true,
            });
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_tree(prefix.clone(), item, out);
            }
        }
    }
}
fn path_to_string(path: &syn::Path) -> String {
    use quote::ToTokens;
    path.to_token_stream().to_string()
}
fn last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|s| s.ident.to_string())
        .unwrap_or_default()
}
fn import_matches_call(import: &ImportInfo, call: &str) -> bool {
    if let Some(alias) = &import.alias {
        if alias == call {
            return true;
        }
    }
    if import.last_segment == call {
        return true;
    }
    if import.is_glob {
        return true;
    }
    false
}
fn qualify_call_from_import(import: &ImportInfo, call: &str) -> String {
    if import.is_glob {
        format!("{}::{}", import.full_path.trim_end_matches("::*"), call)
    } else if import.alias.is_some() {
        import.full_path.clone()
    } else {
        import.full_path.clone()
    }
}
fn is_ignored_call(name: &str) -> bool {
    let simple = name.rsplit("::").next().unwrap_or(name);
    if matches!(
        name,
        "Local::now"
            | "Utc::now"
            | "Instant::now"
            | "std::time::Instant::now"
            | "fs::read_to_string"
            | "fs::read_dir"
            | "fs::metadata"
            | "parse_file"
            | "parse_str"
            | "prettyplease::unparse"
            | "serde_json::from_str"
            | "dirs::home_dir"
            | "mpsc::channel"
            | "flag::register"
            | "Stdio::piped"
    ) {
        return true;
    }
    matches!(
        simple,
        "Ok" | "Err"
            | "Some"
            | "None"
            | "Self"
            | "default"
            | "new"
            | "into"
            | "from"
            | "try_from"
            | "try_into"
            | "as_ref"
            | "as_mut"
            | "as_deref"
            | "as_str"
            | "borrow"
            | "to_owned"
            | "to_string"
            | "to_string_lossy"
            | "to_ascii_lowercase"
            | "to_path_buf"
            | "to_str"
            | "clone"
            | "cloned"
            | "copied"
            | "iter"
            | "iter_mut"
            | "into_iter"
            | "any"
            | "all"
            | "map"
            | "filter"
            | "filter_map"
            | "find"
            | "find_map"
            | "flat_map"
            | "fold"
            | "for_each"
            | "enumerate"
            | "zip"
            | "skip"
            | "take"
            | "collect"
            | "count"
            | "sum"
            | "len"
            | "is_empty"
            | "is_some"
            | "is_none"
            | "is_ok"
            | "is_err"
            | "contains"
            | "starts_with"
            | "ends_with"
            | "trim"
            | "trim_start"
            | "trim_end"
            | "split"
            | "split_whitespace"
            | "join"
            | "lines"
            | "chars"
            | "bytes"
            | "push"
            | "push_str"
            | "insert"
            | "remove"
            | "extend"
            | "get"
            | "get_mut"
            | "entry"
            | "or_insert"
            | "or_insert_with"
            | "unwrap"
            | "unwrap_or"
            | "unwrap_or_default"
            | "unwrap_or_else"
            | "expect"
            | "and_then"
            | "path"
            | "display"
            | "extension"
            | "file_name"
            | "strip_prefix"
            | "values"
            | "sort"
            | "min"
            | "max"
            | "last"
            | "args"
            | "stdin"
            | "stdout"
            | "stderr"
            | "spawn"
            | "wait"
            | "wait_with_output"
            | "output"
            | "success"
            | "code"
            | "elapsed"
            | "as_millis"
            | "timestamp_millis"
            | "load"
            | "ok"
            | "context"
            | "with_context"
            | "exists"
            | "next"
            | "nth"
            | "floor"
            | "log10"
            | "saturating_sub"
            | "num_seconds"
            | "signed_duration_since"
    )
}
fn main() -> Result<()> {
    let app = Application::new()?;
    app.run()
}
