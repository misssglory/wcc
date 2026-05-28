use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use syn::spanned::Spanned;
use url::Url;
use wcc::config::load_unified_config;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Position {
    line: usize,
    column: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceRange {
    start: Position,
    end: Position,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<ByteRange>,
}

#[derive(Debug, Clone)]
struct SymbolNode {
    key: String,
    label: String,
    kind: String,
    path: String,
    visibility: String,
    signature: Option<String>,
    selection_range: SourceRange,
    full_range: SourceRange,
    source_snippet: Option<String>,
}

#[derive(Debug, Clone)]
struct FileData {
    rel_path: String,
    content: Option<String>,
}

#[derive(Debug, Clone)]
struct ImplBlock {
    key: String,
    path: String,
    target_label: String,
    trait_label: Option<String>,
    full_range: SourceRange,
    selection_range: SourceRange,
    source_snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CliOptions {
    target_dir: PathBuf,
    output_path: PathBuf,
    include_sources: bool,
    rust_analyzer_bin: String,
    warmup_ms: u64,
    show_calls: bool,
    show_fields: bool,
    show_imports: bool,
    include_field_nodes: bool,
    show_impl_edges: bool,
}

impl CliOptions {
    fn parse() -> Result<Self> {
        let cfg = load_unified_config().context("loading unified config")?;
        let mut args: VecDeque<String> = env::args().skip(1).collect();
        let mut target_dir = env::current_dir()?;
        let mut output_path = PathBuf::from("code_graph.json");
        let mut include_sources = cfg.wcl.copy_file_contents;
        let mut rust_analyzer_bin = env::var("RUST_ANALYZER_BIN").unwrap_or_else(|_| "rust-analyzer".to_string());
        let mut warmup_ms = 2500u64;
        let mut show_calls = cfg.wcg.show_calls;
        let mut show_fields = cfg.wcg.show_fields;
        let mut show_imports = cfg.wcg.show_imports;
        let mut include_field_nodes = cfg.wcg.include_field_nodes;
        let mut show_impl_edges = cfg.wcg.show_impl_edges;

        while let Some(arg) = args.pop_front() {
            match arg.as_str() {
                "-o" | "--output" => {
                    output_path = PathBuf::from(args.pop_front().ok_or_else(|| anyhow!("missing value for --output"))?);
                }
                "-s" | "--sources" => {
                    include_sources = !include_sources;
                }
                "--rust-analyzer" => {
                    rust_analyzer_bin = args.pop_front().ok_or_else(|| anyhow!("missing value for --rust-analyzer"))?;
                }
                "--warmup-ms" => {
                    warmup_ms = args
                        .pop_front()
                        .ok_or_else(|| anyhow!("missing value for --warmup-ms"))?
                        .parse()
                        .context("parsing --warmup-ms")?;
                }
                "--no-calls" => show_calls = false,
                "--no-fields" => show_fields = false,
                "--no-imports" => show_imports = false,
                "--fields-as-nodes" => include_field_nodes = true,
                "--no-fields-as-nodes" => include_field_nodes = false,
                "--no-impl-edges" => show_impl_edges = false,
                other if other.starts_with('-') => bail!("unknown flag: {}", other),
                path => target_dir = PathBuf::from(path),
            }
        }

        Ok(Self {
            target_dir,
            output_path,
            include_sources,
            rust_analyzer_bin,
            warmup_ms,
            show_calls,
            show_fields,
            show_imports,
            include_field_nodes,
            show_impl_edges,
        })
    }
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
    position_encoding: String,
}

impl LspClient {
    fn start(bin: &str, root: &Path) -> Result<Self> {
        let mut child = Command::new(bin)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting {}", bin))?;

        let stdin = child.stdin.take().context("capturing rust-analyzer stdin")?;
        let stdout = child.stdout.take().context("capturing rust-analyzer stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
            position_encoding: "utf-16".to_string(),
        })
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut content_length = None::<usize>;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line)?;
            if n == 0 {
                bail!("rust-analyzer closed stdout");
            }
            let line_trim = line.trim_end();
            if line_trim.is_empty() {
                break;
            }
            if let Some(rest) = line_trim.strip_prefix("Content-Length:") {
                content_length = Some(rest.trim().parse().context("parsing Content-Length")?);
            }
        }
        let len = content_length.context("missing Content-Length header")?;
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf)?;
        Ok(serde_json::from_slice(&buf).context("decoding LSP payload")?)
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;

        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                bail!("timeout waiting for {}", method);
            }
            let msg = self.read_message()?;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = msg.get("error") {
                    bail!("LSP {} error: {}", method, err);
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn initialize(&mut self, root: &Path) -> Result<()> {
        let root_uri = path_to_uri(root)?;
        let result = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": root.display().to_string(),
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": root.file_name().and_then(|s| s.to_str()).unwrap_or("workspace")
                }],
                "clientInfo": {"name": "wcgv", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {
                    "general": {
                        "positionEncodings": ["utf-8", "utf-16"]
                    },
                    "textDocument": {
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                        "references": {},
                        "callHierarchy": {},
                        "hover": {}
                    },
                    "workspace": {
                        "workspaceFolders": true,
                        "symbol": {}
                    }
                },
                "initializationOptions": {
                    "cargo": {
                        "allFeatures": true,
                        "buildScripts": {"enable": true}
                    },
                    "procMacro": {"enable": true},
                    "checkOnSave": true
                }
            }),
            Duration::from_secs(30),
        )?;

        self.position_encoding = result
            .get("capabilities")
            .and_then(|c| c.get("positionEncoding"))
            .and_then(Value::as_str)
            .unwrap_or("utf-16")
            .to_string();

        self.notify("initialized", Value::Object(Map::new()))?;
        Ok(())
    }

    fn shutdown(mut self) -> Result<()> {
        let _ = self.request("shutdown", Value::Null, Duration::from_secs(5));
        let _ = self.notify("exit", Value::Null);
        let _ = self.child.wait();
        Ok(())
    }
}

fn main() -> Result<()> {
    run_wcgv_lsp()
}

pub fn run_wcgv_lsp() -> Result<()> {
    let opts = CliOptions::parse()?;
    let root = find_workspace_root(&opts.target_dir)?;
    let cfg = load_unified_config()?;
    let skip_dirs: HashSet<String> = cfg.wcl.skip_dirs.into_iter().collect();

    eprintln!("🔍 Starting rust-analyzer in {}", root.display());
    let mut lsp = LspClient::start(&opts.rust_analyzer_bin, &root)?;
    lsp.initialize(&root)?;
    eprintln!("📐 LSP position encoding: {}", lsp.position_encoding);

    eprintln!("📚 Collecting symbols via LSP...");
    let (mut nodes, files, mut edges) = build_nodes(&mut lsp, &root, &opts, &skip_dirs)?;
    eprintln!("  found {} candidate nodes", nodes.len());

    if opts.show_impl_edges {
        eprintln!("🧱 Building impl edges...");
        let impls = collect_impl_blocks(&root, &files, opts.include_sources)?;
        attach_impl_nodes_and_edges(&mut nodes, &mut edges, &impls);
    }

    if opts.warmup_ms > 0 {
        thread::sleep(Duration::from_millis(opts.warmup_ms));
    }

    if opts.show_calls {
        eprintln!("🔗 Building call hierarchy edges...");
        edges.extend(find_call_hierarchy_edges(&mut lsp, &root, &nodes).unwrap_or_default());
    }

    eprintln!("🔎 Building reference edges...");
    edges.extend(find_references_edges(&mut lsp, &root, &nodes).unwrap_or_default());

    if opts.show_imports {
        eprintln!("📦 Building import edges...");
        edges.extend(find_use_edges(&nodes));
    }

    if opts.show_fields {
        eprintln!("🏗 Building field/type edges...");
        edges.extend(find_field_edges(&nodes));
    }

    let mut unique = HashSet::new();
    edges.retain(|(s, t, ty)| unique.insert((s.clone(), t.clone(), *ty)));

    let output = build_json(&root, &nodes, &files, &edges, opts.include_sources, &lsp.position_encoding);
    fs::write(&opts.output_path, serde_json::to_string_pretty(&output)?)
        .with_context(|| format!("writing {}", opts.output_path.display()))?;

    eprintln!("\x1b[1;32m✓ Graph saved to: {}\x1b[0m", opts.output_path.display());
    eprintln!("\x1b[36m✓ Nodes: {}  Edges: {}\x1b[0m", nodes.len(), edges.len());
    eprintln!("\x1b[33m✓ includeSources={} (toggle with -s)\x1b[0m", opts.include_sources);
    eprintln!("\x1b[33m✓ includeFieldNodes={} (toggle with --fields-as-nodes/--no-fields-as-nodes)\x1b[0m", opts.include_field_nodes);
    println!("{}", opts.output_path.display());

    lsp.shutdown()?;
    Ok(())
}

fn find_workspace_root(path: &Path) -> Result<PathBuf> {
    let mut cur = fs::canonicalize(path).with_context(|| format!("canonicalizing {}", path.display()))?;
    if cur.is_file() {
        cur = cur.parent().context("file path has no parent")?.to_path_buf();
    }
    let mut probe = cur.clone();
    loop {
        if probe.join("Cargo.toml").exists() {
            return Ok(probe);
        }
        if !probe.pop() {
            return Ok(cur);
        }
    }
}

fn path_to_uri(path: &Path) -> Result<String> {
    let abs = fs::canonicalize(path).with_context(|| format!("canonicalizing {}", path.display()))?;
    let url = Url::from_file_path(&abs).map_err(|_| anyhow!("failed to convert path to URI: {}", abs.display()))?;
    Ok(url.to_string())
}

fn uri_to_path(uri: &str) -> Result<PathBuf> {
    let url = Url::parse(uri)?;
    url.to_file_path().map_err(|_| anyhow!("invalid file URI: {}", uri))
}

fn line_col_to_byte_index(text: &str, line1: usize, col0: usize) -> Option<usize> {
    let mut line_no = 1usize;
    let mut line_start = 0usize;

    for segment in text.split_inclusive('\n') {
        let line_text = segment.strip_suffix('\n').unwrap_or(segment);
        if line_no == line1 {
            return utf8_column_to_byte_index(line_text, col0).map(|off| line_start + off);
        }
        line_start += segment.len();
        line_no += 1;
    }

    if line_no == line1 {
        let line_text = &text[line_start..];
        return utf8_column_to_byte_index(line_text, col0).map(|off| line_start + off);
    }

    None
}

fn utf8_column_to_byte_index(line: &str, col0: usize) -> Option<usize> {
    if col0 > line.len() {
        return None;
    }
    if !line.is_char_boundary(col0) {
        return None;
    }
    Some(col0)
}

fn read_snippet(path: &Path, range: &SourceRange) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let start = line_col_to_byte_index(&content, range.start.line, range.start.column)?;
    let end = line_col_to_byte_index(&content, range.end.line, range.end.column).unwrap_or(content.len());
    if start <= end && end <= content.len() {
        Some(content[start..end].to_string())
    } else {
        None
    }
}

fn make_range(v: &Value) -> Result<SourceRange> {
    let start = v.get("start").context("range.start missing")?;
    let end = v.get("end").context("range.end missing")?;
    Ok(SourceRange {
        start: Position {
            line: start.get("line").and_then(Value::as_u64).unwrap_or(0) as usize + 1,
            column: start.get("character").and_then(Value::as_u64).unwrap_or(0) as usize,
        },
        end: Position {
            line: end.get("line").and_then(Value::as_u64).unwrap_or(0) as usize + 1,
            column: end.get("character").and_then(Value::as_u64).unwrap_or(0) as usize,
        },
        bytes: None,
    })
}

fn make_syn_range(span: proc_macro2::Span, source: &str) -> SourceRange {
    let start = span.start();
    let end = span.end();
    let start_line = start.line;
    let start_col = start.column;
    let end_line = end.line;
    let end_col = end.column;
    let start_byte = line_col_to_byte_index(source, start_line, start_col).unwrap_or(0);
    let end_byte = line_col_to_byte_index(source, end_line, end_col).unwrap_or(source.len());

    SourceRange {
        start: Position {
            line: start_line,
            column: start_col,
        },
        end: Position {
            line: end_line,
            column: end_col,
        },
        bytes: Some(ByteRange {
            start: start_byte,
            end: end_byte,
        }),
    }
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        23 => "struct",
        26 => "type_parameter",
        _ => "symbol",
    }
}

fn is_interesting_symbol(kind: &str, include_field_nodes: bool) -> bool {
    matches!(kind, "function" | "method" | "struct" | "field" | "class" | "interface")
        && (kind != "field" || include_field_nodes)
}

fn visibility_from_signature(sig: Option<&str>) -> String {
    match sig {
        Some(s) if s.contains("pub") => "public".to_string(),
        _ => "private".to_string(),
    }
}

fn make_symbol_key(kind: &str, name: &str, rel_path: &str, selection_range: &SourceRange) -> String {
    format!(
        "{}::{}::{}:{}:{}",
        kind,
        rel_path,
        name,
        selection_range.start.line,
        selection_range.start.column
    )
}

fn make_impl_key(rel_path: &str, target_label: &str, trait_label: Option<&str>, selection_range: &SourceRange) -> String {
    match trait_label {
        Some(tr) => format!(
            "impl::{}::{} for {}:{}:{}",
            rel_path,
            tr,
            target_label,
            selection_range.start.line,
            selection_range.start.column
        ),
        None => format!(
            "impl::{}::{}:{}:{}",
            rel_path,
            target_label,
            selection_range.start.line,
            selection_range.start.column
        ),
    }
}

fn collect_rust_files(root: &Path, skip_dirs: &HashSet<String>, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if skip_dirs.contains(name) {
                continue;
            }
            collect_rust_files(&path, skip_dirs, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn build_nodes(
    lsp: &mut LspClient,
    root: &Path,
    opts: &CliOptions,
    skip_dirs: &HashSet<String>,
) -> Result<(Vec<SymbolNode>, Vec<FileData>, Vec<(String, String, &'static str)>)> {
    let mut rust_files = Vec::new();
    collect_rust_files(root, skip_dirs, &mut rust_files)?;
    rust_files.sort();

    let mut nodes = Vec::new();
    let mut files = Vec::new();
    let mut edges = Vec::new();

    for file in rust_files {
        let uri = path_to_uri(&file)?;
        let text = fs::read_to_string(&file).unwrap_or_default();
        let rel_path = file.strip_prefix(root).unwrap_or(&file).display().to_string();

        lsp.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": text,
                }
            }),
        )?;

        let result = lsp.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": path_to_uri(&file)?}}),
            Duration::from_secs(20),
        )?;
        let symbols = result.as_array().cloned().unwrap_or_default();

        walk_document_symbols(
            &symbols,
            &rel_path,
            &file,
            opts.include_sources,
            opts.include_field_nodes,
            &mut nodes,
            &mut edges,
            None,
        )?;

        files.push(FileData {
            rel_path,
            content: if opts.include_sources { Some(text) } else { None },
        });
    }

    Ok((nodes, files, edges))
}

fn walk_document_symbols(
    items: &[Value],
    rel_path: &str,
    file: &Path,
    include_sources: bool,
    include_field_nodes: bool,
    nodes: &mut Vec<SymbolNode>,
    edges: &mut Vec<(String, String, &'static str)>,
    parent_key: Option<&str>,
) -> Result<()> {
    for sym in items {
        let name = sym.get("name").and_then(Value::as_str).unwrap_or("<unnamed>");
        let kind_num = sym.get("kind").and_then(Value::as_u64).unwrap_or(0);
        let kind = symbol_kind_name(kind_num).to_string();
        let full_range = make_range(sym.get("range").context("symbol missing full range")?)?;
        let selection_range = make_range(
            sym.get("selectionRange")
                .or_else(|| sym.get("range"))
                .context("symbol missing selection range")?,
        )?;
        let detail = sym.get("detail").and_then(Value::as_str).map(|s| s.to_string());
        let key = make_symbol_key(&kind, name, rel_path, &selection_range);
        let keep_node = is_interesting_symbol(&kind, include_field_nodes);

        let next_parent = if keep_node {
            let snippet = if include_sources { read_snippet(file, &full_range) } else { None };
            nodes.push(SymbolNode {
                key: key.clone(),
                label: name.to_string(),
                kind: kind.clone(),
                path: rel_path.to_string(),
                visibility: visibility_from_signature(detail.as_deref()),
                signature: detail,
                selection_range: selection_range.clone(),
                full_range: full_range.clone(),
                source_snippet: snippet,
            });
            if let Some(parent) = parent_key {
                edges.push((parent.to_string(), key.clone(), "contains"));
            }
            Some(key)
        } else {
            parent_key.map(str::to_string)
        };

        if let Some(children) = sym.get("children").and_then(Value::as_array) {
            walk_document_symbols(
                children,
                rel_path,
                file,
                include_sources,
                include_field_nodes,
                nodes,
                edges,
                next_parent.as_deref(),
            )?;
        }
    }
    Ok(())
}

fn symbol_position_params(root: &Path, node: &SymbolNode) -> Result<Value> {
    let abs = root.join(&node.path);
    Ok(json!({
        "textDocument": {"uri": path_to_uri(&abs)?},
        "position": {
            "line": node.selection_range.start.line.saturating_sub(1),
            "character": node.selection_range.start.column,
        }
    }))
}

fn contains(range: &SourceRange, line: usize, col: usize) -> bool {
    let starts_before = line > range.start.line || (line == range.start.line && col >= range.start.column);
    let ends_before_end = line < range.end.line || (line == range.end.line && col < range.end.column);
    starts_before && ends_before_end
}

fn range_span_score(range: &SourceRange) -> (usize, usize, usize, usize) {
    (
        range.end.line.saturating_sub(range.start.line),
        range.end.column.saturating_sub(range.start.column),
        range.start.line,
        range.start.column,
    )
}

fn find_enclosing_symbol<'a>(symbols: &'a [SymbolNode], line: usize, col: usize) -> Option<&'a SymbolNode> {
    symbols
        .iter()
        .filter(|sym| contains(&sym.full_range, line, col))
        .min_by_key(|sym| range_span_score(&sym.full_range))
}

fn build_file_symbol_index(nodes: &[SymbolNode]) -> HashMap<String, Vec<SymbolNode>> {
    let mut by_file: HashMap<String, Vec<SymbolNode>> = HashMap::new();
    for node in nodes {
        by_file.entry(node.path.clone()).or_default().push(node.clone());
    }
    for symbols in by_file.values_mut() {
        symbols.sort_by_key(|sym| {
            (
                sym.full_range.start.line,
                sym.full_range.start.column,
                sym.full_range.end.line,
                sym.full_range.end.column,
            )
        });
    }
    by_file
}

fn find_references_edges(
    lsp: &mut LspClient,
    root: &Path,
    nodes: &[SymbolNode],
) -> Result<Vec<(String, String, &'static str)>> {
    let mut edges = Vec::new();
    let file_symbols = build_file_symbol_index(nodes);

    for decl in nodes {
        if decl.kind == "field" || decl.kind == "impl" {
            continue;
        }

        let mut obj = symbol_position_params(root, decl)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        obj.insert("context".to_string(), json!({"includeDeclaration": false}));

        let refs = lsp
            .request(
                "textDocument/references",
                Value::Object(obj),
                Duration::from_secs(20),
            )
            .unwrap_or(Value::Null);

        let Some(arr) = refs.as_array() else { continue; };
        for loc in arr {
            let Some(uri) = loc.get("uri").and_then(Value::as_str) else { continue; };
            let Ok(path) = uri_to_path(uri) else { continue; };
            let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
            let Some(range_v) = loc.get("range") else { continue; };
            let Ok(range) = make_range(range_v) else { continue; };
            let Some(symbols) = file_symbols.get(&rel) else { continue; };
            let Some(container) = find_enclosing_symbol(symbols, range.start.line, range.start.column) else { continue; };
            if container.key != decl.key {
                edges.push((container.key.clone(), decl.key.clone(), "references"));
            }
        }
    }

    Ok(edges)
}

fn find_call_hierarchy_edges(
    lsp: &mut LspClient,
    root: &Path,
    nodes: &[SymbolNode],
) -> Result<Vec<(String, String, &'static str)>> {
    let callable: Vec<&SymbolNode> = nodes
        .iter()
        .filter(|n| n.kind == "function" || n.kind == "method")
        .collect();

    let mut edges = Vec::new();
    let mut callable_lookup: HashMap<(String, usize, usize), String> = HashMap::new();
    for node in &callable {
        callable_lookup.insert(
            (
                node.path.clone(),
                node.selection_range.start.line,
                node.selection_range.start.column,
            ),
            node.key.clone(),
        );
    }

    for node in callable {
        let prepared = lsp
            .request(
                "textDocument/prepareCallHierarchy",
                symbol_position_params(root, node)?,
                Duration::from_secs(20),
            )
            .unwrap_or(Value::Null);

        let Some(items) = prepared.as_array() else { continue; };
        for item in items {
            let outgoing = lsp
                .request(
                    "callHierarchy/outgoingCalls",
                    json!({"item": item}),
                    Duration::from_secs(20),
                )
                .unwrap_or(Value::Null);

            let Some(arr) = outgoing.as_array() else { continue; };
            for call in arr {
                let Some(to) = call.get("to") else { continue; };
                let Some(uri) = to.get("uri").and_then(Value::as_str) else { continue; };
                let Ok(path) = uri_to_path(uri) else { continue; };
                let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
                let Some(sel) = to.get("selectionRange").or_else(|| to.get("range")) else { continue; };
                let Ok(range) = make_range(sel) else { continue; };
                if let Some(target_key) = callable_lookup.get(&(rel, range.start.line, range.start.column)) {
                    edges.push((node.key.clone(), target_key.clone(), "calls"));
                }
            }
        }
    }

    Ok(edges)
}

fn extract_ident_tokens(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            out.insert(current.clone());
            current.clear();
        }
    }
    if !current.is_empty() {
        out.insert(current);
    }
    out
}

fn find_use_edges(nodes: &[SymbolNode]) -> Vec<(String, String, &'static str)> {
    let mut edges = Vec::new();
    let mut type_nodes: HashMap<String, String> = HashMap::new();
    for n in nodes {
        if n.kind == "struct" || n.kind == "class" || n.kind == "interface" {
            type_nodes.entry(n.label.clone()).or_insert_with(|| n.key.clone());
        }
    }

    for n in nodes {
        let Some(sig) = &n.signature else { continue; };
        let tokens = extract_ident_tokens(sig);
        for token in tokens {
            if let Some(target) = type_nodes.get(&token) {
                if target != &n.key {
                    edges.push((n.key.clone(), target.clone(), "uses_type"));
                }
            }
        }
    }
    edges
}

fn find_field_edges(nodes: &[SymbolNode]) -> Vec<(String, String, &'static str)> {
    let mut edges = Vec::new();
    let mut structs_by_name: HashMap<String, String> = HashMap::new();
    let mut fields = Vec::new();
    let mut field_owner: HashMap<String, String> = HashMap::new();

    for n in nodes {
        if n.kind == "struct" {
            structs_by_name.entry(n.label.clone()).or_insert_with(|| n.key.clone());
        }
    }

    for n in nodes {
        if n.kind == "field" {
            fields.push(n);
        }
    }

    for n in nodes {
        if n.kind == "struct" {
            for field in &fields {
                if field.path == n.path && contains(&n.full_range, field.selection_range.start.line, field.selection_range.start.column) {
                    field_owner.insert(field.key.clone(), n.key.clone());
                }
            }
        }
    }

    for field in fields {
        let Some(sig) = &field.signature else { continue; };
        let tokens = extract_ident_tokens(sig);
        let source_key = field_owner.get(&field.key).unwrap_or(&field.key);
        for token in tokens {
            if let Some(target) = structs_by_name.get(&token) {
                if target != source_key {
                    edges.push((source_key.clone(), target.clone(), "field_type"));
                }
            }
        }
    }

    edges
}

fn collect_impl_blocks(root: &Path, files: &[FileData], include_sources: bool) -> Result<Vec<ImplBlock>> {
    let mut out = Vec::new();

    for file in files {
        let abs = root.join(&file.rel_path);
        let source = fs::read_to_string(&abs).with_context(|| format!("reading {}", abs.display()))?;
        let syntax = syn::parse_file(&source).with_context(|| format!("parsing {}", abs.display()))?;

        for item in syntax.items {
            let syn::Item::Impl(item_impl) = item else { continue; };
            let self_ty = item_impl.self_ty.as_ref();
            let target_label = type_to_label(self_ty);
            let trait_label = item_impl.trait_.as_ref().map(|(_, path, _)| path_to_label(path));
            let full_range = make_syn_range(item_impl.span(), &source);
            let selection_range = make_impl_selection_range(&item_impl, &source);
            let key = make_impl_key(&file.rel_path, &target_label, trait_label.as_deref(), &selection_range);
            let source_snippet = if include_sources {
                read_range_snippet(&source, &full_range)
            } else {
                None
            };

            out.push(ImplBlock {
                key,
                path: file.rel_path.clone(),
                target_label,
                trait_label,
                full_range,
                selection_range,
                source_snippet,
            });
        }
    }

    Ok(out)
}

fn read_range_snippet(source: &str, range: &SourceRange) -> Option<String> {
    let start = range.bytes.as_ref().map(|b| b.start)?;
    let end = range.bytes.as_ref().map(|b| b.end)?;
    if start <= end && end <= source.len() {
        Some(source[start..end].to_string())
    } else {
        None
    }
}

fn type_to_label(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(tp) => tp.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_else(|| "<type>".to_string()),
        _ => "<type>".to_string(),
    }
}

fn path_to_label(path: &syn::Path) -> String {
    path.segments.iter().map(|s| s.ident.to_string()).collect::<Vec<_>>().join("::")
}

fn make_impl_selection_range(item_impl: &syn::ItemImpl, source: &str) -> SourceRange {
    let token = if let Some((_, trait_path, _)) = &item_impl.trait_ {
        trait_path.segments.last().map(|s| s.ident.span()).unwrap_or_else(|| item_impl.self_ty.span())
    } else {
        item_impl.self_ty.span()
    };
    make_syn_range(token, source)
}

fn attach_impl_nodes_and_edges(
    nodes: &mut Vec<SymbolNode>,
    edges: &mut Vec<(String, String, &'static str)>,
    impls: &[ImplBlock],
) {
    let mut type_by_label: HashMap<String, String> = HashMap::new();
    for n in nodes.iter() {
        if n.kind == "struct" || n.kind == "class" || n.kind == "interface" {
            type_by_label.entry(n.label.clone()).or_insert_with(|| n.key.clone());
        }
    }

    let method_nodes: Vec<SymbolNode> = nodes.iter().filter(|n| n.kind == "method").cloned().collect();

    for imp in impls {
        nodes.push(SymbolNode {
            key: imp.key.clone(),
            label: match &imp.trait_label {
                Some(tr) => format!("impl {} for {}", tr, imp.target_label),
                None => format!("impl {}", imp.target_label),
            },
            kind: "impl".to_string(),
            path: imp.path.clone(),
            visibility: "private".to_string(),
            signature: Some(match &imp.trait_label {
                Some(tr) => format!("impl {} for {}", tr, imp.target_label),
                None => format!("impl {}", imp.target_label),
            }),
            selection_range: imp.selection_range.clone(),
            full_range: imp.full_range.clone(),
            source_snippet: imp.source_snippet.clone(),
        });

        if let Some(type_key) = type_by_label.get(&imp.target_label) {
            edges.push((imp.key.clone(), type_key.clone(), "associated_with"));
        }

        for method in &method_nodes {
            if method.path == imp.path
                && contains(&imp.full_range, method.selection_range.start.line, method.selection_range.start.column)
            {
                edges.push((imp.key.clone(), method.key.clone(), "contains"));
            }
        }
    }
}

fn build_json(
    root: &Path,
    nodes: &[SymbolNode],
    files: &[FileData],
    edges: &[(String, String, &'static str)],
    include_sources: bool,
    position_encoding: &str,
) -> Value {
    let node_json: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "key": n.key,
                "attributes": {
                    "label": n.label,
                    "type": n.kind,
                    "path": n.path,
                    "visibility": n.visibility,
                    "level": 0,
                    "signature": n.signature,
                    "sourceSnippet": if include_sources { n.source_snippet.clone() } else { None },
                    "range": {
                        "start": {"line": n.full_range.start.line, "column": n.full_range.start.column},
                        "end": {"line": n.full_range.end.line, "column": n.full_range.end.column},
                        "bytes": n.full_range.bytes.as_ref().map(|b| json!({"start": b.start, "end": b.end}))
                    },
                    "selectionRange": {
                        "start": {"line": n.selection_range.start.line, "column": n.selection_range.start.column},
                        "end": {"line": n.selection_range.end.line, "column": n.selection_range.end.column},
                        "bytes": n.selection_range.bytes.as_ref().map(|b| json!({"start": b.start, "end": b.end}))
                    }
                }
            })
        })
        .collect();

    let edge_json: Vec<Value> = edges
        .iter()
        .map(|(s, t, ty)| {
            json!({
                "source": s,
                "target": t,
                "attributes": {"type": ty}
            })
        })
        .collect();

    let file_json: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "path": f.rel_path,
                "content": if include_sources { f.content.clone() } else { None }
            })
        })
        .collect();

    json!({
        "graph": {
            "nodes": node_json,
            "edges": edge_json,
        },
        "files": file_json,
        "metadata": {
            "timestamp": Local::now().to_rfc3339(),
            "root": root.display().to_string(),
            "includeSources": include_sources,
            "positionEncoding": position_encoding,
            "stats": {
                "nodes": nodes.len(),
                "edges": edges.len(),
                "files": files.len(),
            }
        }
    })
}
