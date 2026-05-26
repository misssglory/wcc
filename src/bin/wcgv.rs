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
        })
    }
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
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
        let _ = self.request(
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

    eprintln!("📚 Collecting symbols via LSP...");
    let (nodes, files) = build_nodes(&mut lsp, &root, opts.include_sources, &skip_dirs)?;
    eprintln!("  found {} candidate nodes", nodes.len());

    if opts.warmup_ms > 0 {
        thread::sleep(Duration::from_millis(opts.warmup_ms));
    }

    let mut edges: Vec<(String, String, &'static str)> = Vec::new();

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

    let output = build_json(&root, &nodes, &files, &edges, opts.include_sources);
    fs::write(&opts.output_path, serde_json::to_string_pretty(&output)?)
        .with_context(|| format!("writing {}", opts.output_path.display()))?;

    eprintln!("\x1b[1;32m✓ Graph saved to: {}\x1b[0m", opts.output_path.display());
    eprintln!("\x1b[36m✓ Nodes: {}  Edges: {}\x1b[0m", nodes.len(), edges.len());
    eprintln!("\x1b[33m✓ includeSources={} (toggle with -s)\x1b[0m", opts.include_sources);
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
    let mut current_line = 1usize;
    let mut current_col = 0usize;

    for (idx, ch) in text.char_indices() {
        if current_line == line1 && current_col == col0 {
            return Some(idx);
        }
        if ch == '\n' {
            current_line += 1;
            current_col = 0;
        } else {
            current_col += 1;
        }
    }

    if current_line == line1 && current_col == col0 {
        Some(text.len())
    } else {
        None
    }
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

fn is_interesting_symbol(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "struct" | "field" | "class" | "interface")
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

fn flatten_document_symbols(items: &[Value], out: &mut Vec<Value>) {
    for item in items {
        out.push(item.clone());
        if let Some(children) = item.get("children").and_then(Value::as_array) {
            flatten_document_symbols(children, out);
        }
    }
}

fn build_nodes(
    lsp: &mut LspClient,
    root: &Path,
    include_sources: bool,
    skip_dirs: &HashSet<String>,
) -> Result<(Vec<SymbolNode>, Vec<FileData>)> {
    let mut rust_files = Vec::new();
    collect_rust_files(root, skip_dirs, &mut rust_files)?;
    rust_files.sort();

    let mut nodes = Vec::new();
    let mut files = Vec::new();

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
        let mut flat = Vec::new();
        flatten_document_symbols(&symbols, &mut flat);

        for sym in flat {
            let name = sym.get("name").and_then(Value::as_str).unwrap_or("<unnamed>");
            let kind_num = sym.get("kind").and_then(Value::as_u64).unwrap_or(0);
            let kind = symbol_kind_name(kind_num).to_string();
            if !is_interesting_symbol(&kind) {
                continue;
            }

            let full_range = make_range(sym.get("range").context("symbol missing full range")?)?;
            let selection_range = make_range(
                sym.get("selectionRange")
                    .or_else(|| sym.get("range"))
                    .context("symbol missing selection range")?,
            )?;
            let detail = sym.get("detail").and_then(Value::as_str).map(|s| s.to_string());
            let key = make_symbol_key(&kind, name, &rel_path, &selection_range);
            let snippet = if include_sources { read_snippet(&file, &full_range) } else { None };

            nodes.push(SymbolNode {
                key,
                label: name.to_string(),
                kind,
                path: rel_path.clone(),
                visibility: visibility_from_signature(detail.as_deref()),
                signature: detail,
                selection_range,
                full_range,
                source_snippet: snippet,
            });
        }

        files.push(FileData {
            rel_path,
            content: if include_sources { Some(text) } else { None },
        });
    }

    Ok((nodes, files))
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
    let ends_after = line < range.end.line || (line == range.end.line && col <= range.end.column);
    starts_before && ends_after
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
        symbols.sort_by_key(|sym| (
            sym.full_range.start.line,
            sym.full_range.start.column,
            sym.full_range.end.line,
            sym.full_range.end.column,
        ));
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
        let mut obj = symbol_position_params(root, decl)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        obj.insert("context".to_string(), json!({"includeDeclaration": false}));

        let refs = lsp.request(
            "textDocument/references",
            Value::Object(obj),
            Duration::from_secs(20),
        ).unwrap_or(Value::Null);

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
        let prepared = lsp.request(
            "textDocument/prepareCallHierarchy",
            symbol_position_params(root, node)?,
            Duration::from_secs(20),
        ).unwrap_or(Value::Null);

        let Some(items) = prepared.as_array() else { continue; };
        for item in items {
            let outgoing = lsp.request(
                "callHierarchy/outgoingCalls",
                json!({"item": item}),
                Duration::from_secs(20),
            ).unwrap_or(Value::Null);

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

    for n in nodes {
        if n.kind == "struct" {
            structs_by_name.entry(n.label.clone()).or_insert_with(|| n.key.clone());
        }
        if n.kind == "field" {
            fields.push(n);
        }
    }

    for field in fields {
        let Some(sig) = &field.signature else { continue; };
        let tokens = extract_ident_tokens(sig);
        for token in tokens {
            if let Some(target) = structs_by_name.get(&token) {
                if target != &field.key {
                    edges.push((field.key.clone(), target.clone(), "field_type"));
                }
            }
        }
    }

    edges
}

fn build_json(
    root: &Path,
    nodes: &[SymbolNode],
    files: &[FileData],
    edges: &[(String, String, &'static str)],
    include_sources: bool,
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
                        "end": {"line": n.full_range.end.line, "column": n.full_range.end.column}
                    },
                    "selectionRange": {
                        "start": {"line": n.selection_range.start.line, "column": n.selection_range.start.column},
                        "end": {"line": n.selection_range.end.line, "column": n.selection_range.end.column}
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
            "stats": {
                "nodes": nodes.len(),
                "edges": edges.len(),
                "files": files.len(),
            }
        }
    })
}
