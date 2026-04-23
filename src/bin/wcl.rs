use std::{
    collections::BTreeMap,
    env,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use wcc::common::*;

#[derive(Debug, Clone, Default)]
struct FunctionInfo {
    name: String,
    lines: usize,
    words: usize,
    chars: usize,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug, Clone, Default)]
struct ClassInfo {
    name: String,
    lines: usize,
    words: usize,
    chars: usize,
    start_line: usize,
    end_line: usize,
    methods: Vec<FunctionInfo>,
}

#[derive(Debug, Clone, Default)]
struct FileStats {
    path: PathBuf,
    lines: usize,
    words: usize,
    chars: usize,
    bytes: usize,
    functions: Vec<FunctionInfo>,
    classes: Vec<ClassInfo>,
    error: Option<String>,
    uses: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Default)]
struct DirectoryStats {
    files: usize,
    total_lines: usize,
    total_words: usize,
    total_chars: usize,
    total_bytes: usize,
    total_functions: usize,
    total_classes: usize,
    file_stats: Vec<FileStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    max_file_size_kb: usize,
    skip_patterns: Vec<String>,
    skip_dirs: Vec<String>,
    show_empty_files: bool,
    show_stats_per_file: bool,
    show_function_details: bool,
    show_class_details: bool,
    show_usage_stats: bool,
    max_files_to_display: usize,
    min_function_lines: usize,
    min_class_lines: usize,
    max_functions_per_file: usize,
    max_classes_per_file: usize,
    parallel_processing: bool,
    max_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_file_size_kb: 50,
            skip_patterns: vec![
                ".o".to_string(), ".pyc".to_string(), ".pyo".to_string(),
                ".so".to_string(), ".dll".to_string(), ".dylib".to_string(),
                ".exe".to_string(), ".class".to_string(), ".jar".to_string(),
                ".war".to_string(), ".ear".to_string(), ".zip".to_string(),
                ".tar".to_string(), ".gz".to_string(), ".bz2".to_string(),
                ".xz".to_string(), ".7z".to_string(), ".rar".to_string(),
                ".png".to_string(), ".jpg".to_string(), ".jpeg".to_string(),
                ".gif".to_string(), ".bmp".to_string(), ".ico".to_string(),
                ".mp3".to_string(), ".mp4".to_string(), ".avi".to_string(),
                ".mov".to_string(), ".pdf".to_string(), ".doc".to_string(),
                ".docx".to_string(), ".bkp".to_string(),
            ],
            skip_dirs: vec![
                "target".to_string(), "node_modules".to_string(),
                ".git".to_string(), ".svn".to_string(), ".hg".to_string(),
                "build".to_string(), "dist".to_string(), "__pycache__".to_string(),
                ".cache".to_string(), ".cargo".to_string(), ".idea".to_string(),
                ".vscode".to_string(),
            ],
            show_empty_files: false,
            show_stats_per_file: true,
            show_function_details: true,
            show_class_details: true,
            show_usage_stats: false,
            max_files_to_display: 500,
            min_function_lines: 1,
            min_class_lines: 1,
            max_functions_per_file: 100,
            max_classes_per_file: 50,
            parallel_processing: true,
            max_threads: 8,
        }
    }
}

fn load_config() -> Result<Config> {
    let mut config_paths = vec![];

    if let Some(mut path) = dirs::config_dir() {
        path.push("wcl/config.toml");
        config_paths.push(path);
    }

    if let Ok(current_dir) = env::current_dir() {
        let mut path = current_dir.clone();
        path.push(".wclrc");
        config_paths.push(path);
        
        let mut path2 = current_dir;
        path2.push("wcl.toml");
        config_paths.push(path2);
    }

    if let Some(mut path) = dirs::home_dir() {
        path.push(".wclrc");
        config_paths.push(path);
    }

    for path in config_paths {
        if path.exists() {
            let data = fs::read_to_string(&path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            if let Ok(config) = toml::from_str(&data) {
                eprintln!("✓ Loaded config from: {}", path.display());
                return Ok(config);
            }
        }
    }

    if let Some(mut path) = dirs::config_dir() {
        path.push("wcl");
        fs::create_dir_all(&path)?;
        path.push("config.toml");
        if !path.exists() {
            let default_config = Config::default();
            let toml_str = toml::to_string_pretty(&default_config)?;
            fs::write(&path, toml_str)?;
            eprintln!("✓ Created default config at: {}", path.display());
        }
    }

    Ok(Config::default())
}

fn should_skip_file(path: &Path, config: &Config) -> bool {
    // Check if file is too large
    if let Ok(metadata) = fs::metadata(path) {
        let size_kb = metadata.len() / 1024;
        if size_kb > config.max_file_size_kb as u64 {
            return true;
        }
    }

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let ext_with_dot = format!(".{}", ext.to_lowercase());
    
    // Check if file should be skipped
    for pattern in &config.skip_patterns {
        // Check extension
        if ext_with_dot == *pattern {
            return true;
        }
        // Check if filename ends with pattern (for .bkp files)
        if filename.ends_with(pattern) {
            return true;
        }
        // Check exact filename match
        if filename == *pattern {
            return true;
        }
    }

    // Check if it's a binary file (by content)
    if let Ok(content) = fs::read(path) {
        if content.iter().take(1024).any(|&b| b == 0) {
            return true;
        }
    }

    false
}

fn should_skip_dir(path: &Path, config: &Config) -> bool {
    if let Some(name) = path.file_name() {
        let name_str = name.to_string_lossy();
        for pattern in &config.skip_dirs {
            if name_str == *pattern {
                return true;
            }
        }
    }
    false
}

fn extract_function_body(lines: &[&str], start_idx: usize, ext: &str) -> usize {
    let mut brace_count = 0;
    let mut paren_count = 0;
    let mut found_opening_brace = false;
    
    for i in start_idx..lines.len().min(start_idx + 500) {
        let line = lines[i];
        
        for ch in line.chars() {
            match ch {
                '{' => {
                    brace_count += 1;
                    found_opening_brace = true;
                }
                '}' => {
                    if brace_count > 0 {
                        brace_count -= 1;
                    }
                }
                '(' => paren_count += 1,
                ')' => {
                    if paren_count > 0 {
                        paren_count -= 1;
                    }
                }
                _ => {}
            }
        }
        
        if ext == "py" {
            if i > start_idx {
                let current_indent = line.len() - line.trim_start().len();
                let start_indent = lines[start_idx].len() - lines[start_idx].trim_start().len();
                if current_indent <= start_indent && !line.trim().is_empty() {
                    return i - 1;
                }
            }
            continue;
        }
        
        if found_opening_brace && brace_count == 0 && paren_count == 0 {
            return i;
        }
    }
    
    start_idx
}

fn detect_functions_in_file(content: &str, ext: &str, config: &Config) -> Vec<FunctionInfo> {
    let mut functions = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    
    for (idx, line) in lines.iter().enumerate().take(2000) {
        let trimmed = line.trim();
        let mut func_name = None;
        
        match ext {
            "rs" => {
                if trimmed.contains("fn ") && !trimmed.contains("//") {
                    func_name = extract_function_name(trimmed, "fn ");
                }
            }
            "py" => {
                if trimmed.contains("def ") && !trimmed.starts_with('#') {
                    func_name = extract_function_name(trimmed, "def ");
                }
            }
            "js" | "ts" | "jsx" | "tsx" => {
                if trimmed.contains("function ") && !trimmed.contains("//") {
                    func_name = extract_function_name(trimmed, "function ");
                } else if trimmed.contains("=>") && trimmed.contains("const ") {
                    func_name = extract_arrow_function_name(trimmed);
                }
            }
            "c" | "cc" | "cpp" | "h" | "hpp" => {
                if trimmed.contains('(') && trimmed.contains(')') && !trimmed.starts_with("//") && !trimmed.starts_with("/*") {
                    func_name = extract_c_function_name(trimmed);
                }
            }
            "go" => {
                if trimmed.contains("func ") {
                    func_name = extract_function_name(trimmed, "func ");
                }
            }
            _ => {}
        }
        
        if let Some(name) = func_name {
            let end_idx = extract_function_body(&lines, idx, ext);
            if end_idx > idx {
                let func_content = lines[idx..=end_idx].join("\n");
                let stats = calc_stats(&func_content);
                
                if stats.lines >= config.min_function_lines {
                    functions.push(FunctionInfo {
                        name,
                        lines: stats.lines,
                        words: stats.words,
                        chars: stats.chars,
                        start_line: idx + 1,
                        end_line: end_idx + 1,
                    });
                }
            }
            
            if functions.len() >= config.max_functions_per_file {
                break;
            }
        }
    }
    
    functions
}

fn detect_classes_in_file(content: &str, ext: &str, config: &Config) -> Vec<ClassInfo> {
    let mut classes = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    
    for (idx, line) in lines.iter().enumerate().take(2000) {
        let trimmed = line.trim();
        let mut class_name = None;
        let mut class_type = "";
        
        match ext {
            "rs" => {
                if trimmed.contains("struct ") && !trimmed.contains("//") {
                    class_name = extract_class_name(trimmed, "struct ");
                    class_type = "struct";
                } else if trimmed.contains("enum ") && !trimmed.contains("//") {
                    class_name = extract_class_name(trimmed, "enum ");
                    class_type = "enum";
                } else if trimmed.contains("trait ") && !trimmed.contains("//") {
                    class_name = extract_class_name(trimmed, "trait ");
                    class_type = "trait";
                }
            }
            "py" => {
                if trimmed.contains("class ") && !trimmed.starts_with('#') {
                    class_name = extract_class_name(trimmed, "class ");
                    class_type = "class";
                }
            }
            "js" | "ts" | "jsx" | "tsx" => {
                if trimmed.contains("class ") && !trimmed.contains("//") {
                    class_name = extract_class_name(trimmed, "class ");
                    class_type = "class";
                }
            }
            "c" | "cc" | "cpp" | "h" | "hpp" => {
                if trimmed.contains("struct ") && !trimmed.starts_with("//") {
                    class_name = extract_class_name(trimmed, "struct ");
                    class_type = "struct";
                } else if trimmed.contains("class ") && !trimmed.starts_with("//") {
                    class_name = extract_class_name(trimmed, "class ");
                    class_type = "class";
                } else if trimmed.contains("enum ") && !trimmed.starts_with("//") {
                    class_name = extract_class_name(trimmed, "enum ");
                    class_type = "enum";
                }
            }
            "go" => {
                if trimmed.contains("type ") && trimmed.contains("struct") {
                    class_name = extract_class_name(trimmed, "type ");
                    class_type = "type";
                }
            }
            _ => {}
        }
        
        if let Some(name) = class_name {
            let end_idx = extract_function_body(&lines, idx, ext);
            if end_idx > idx {
                let class_content = lines[idx..=end_idx].join("\n");
                let stats = calc_stats(&class_content);
                
                if stats.lines >= config.min_class_lines {
                    let methods = if ext == "rs" || ext == "py" || ext == "js" || ext == "ts" {
                        detect_functions_in_file(&class_content, ext, config)
                    } else {
                        Vec::new()
                    };
                    
                    classes.push(ClassInfo {
                        name: format!("{} {}", class_type, name),
                        lines: stats.lines,
                        words: stats.words,
                        chars: stats.chars,
                        start_line: idx + 1,
                        end_line: end_idx + 1,
                        methods,
                    });
                }
            }
            
            if classes.len() >= config.max_classes_per_file {
                break;
            }
        }
    }
    
    classes
}

fn detect_usage_in_file(content: &str, functions: &[FunctionInfo], classes: &[ClassInfo]) -> BTreeMap<String, usize> {
    let mut usage = BTreeMap::new();
    
    for func in functions {
        let count = content.matches(&func.name).count();
        if count > 0 {
            usage.insert(func.name.clone(), count);
        }
    }
    
    for class in classes {
        let simple_name = class.name.split_whitespace().last().unwrap_or(&class.name);
        let count = content.matches(simple_name).count();
        if count > 0 {
            usage.insert(class.name.clone(), count);
        }
    }
    
    usage
}

fn extract_function_name(line: &str, keyword: &str) -> Option<String> {
    let after_keyword = line.split(keyword).nth(1)?;
    let name = after_keyword
        .split(|c: char| c == '(' || c == ' ' || c == '<' || c == '{')
        .next()?
        .trim();
    
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

fn extract_arrow_function_name(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.split("=>").collect();
    if parts.len() >= 2 {
        let before = parts[0].trim();
        if before.contains("const ") {
            let after_const = before.split("const ").nth(1)?;
            let name = after_const.split('=').next()?.trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn extract_c_function_name(line: &str) -> Option<String> {
    let before_paren = line.split('(').next()?;
    let words: Vec<&str> = before_paren.split_whitespace().collect();
    let name = words.last()?;
    
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

fn extract_class_name(line: &str, keyword: &str) -> Option<String> {
    let after_keyword = line.split(keyword).nth(1)?;
    let name = after_keyword
        .split(|c: char| c == ' ' || c == '{' || c == '(' || c == ':')
        .next()?
        .trim();
    
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(name.to_string())
    } else {
        None
    }
}

fn analyze_file(path: &Path, config: &Config) -> FileStats {
    let mut stats = FileStats {
        path: path.to_path_buf(),
        ..Default::default()
    };

    if should_skip_file(path, config) {
        stats.error = Some("Skipped (binary/large/pattern)".to_string());
        return stats;
    }

    match fs::read_to_string(path) {
        Ok(content) => {
            let calc = calc_stats(&content);
            stats.lines = calc.lines;
            stats.words = calc.words;
            stats.chars = calc.chars;
            stats.bytes = calc.bytes;

            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();

            if config.show_function_details {
                stats.functions = detect_functions_in_file(&content, &ext, config);
            }
            
            if config.show_class_details {
                stats.classes = detect_classes_in_file(&content, &ext, config);
            }
            
            if config.show_usage_stats && (config.show_function_details || config.show_class_details) {
                stats.uses = detect_usage_in_file(&content, &stats.functions, &stats.classes);
            }
        }
        Err(e) => {
            stats.error = Some(format!("Failed to read: {}", e));
        }
    }

    stats
}

fn walk_directory(dir: &Path, config: &Config) -> Result<DirectoryStats> {
    let mut all_paths = Vec::new();
    
    if !dir.exists() {
        bail!("Directory does not exist: {}", dir.display());
    }

    if dir.is_file() {
        let file_stats = analyze_file(dir, config);
        let mut stats = DirectoryStats::default();
        stats.files = 1;
        stats.total_lines = file_stats.lines;
        stats.total_words = file_stats.words;
        stats.total_chars = file_stats.chars;
        stats.total_bytes = file_stats.bytes;
        stats.total_functions = file_stats.functions.len();
        stats.total_classes = file_stats.classes.len();
        stats.file_stats.push(file_stats);
        return Ok(stats);
    }

    fn collect_paths(dir: &Path, paths: &mut Vec<PathBuf>, config: &Config) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            
            if path.is_dir() {
                if should_skip_dir(&path, config) {
                    continue;
                }
                collect_paths(&path, paths, config)?;
            } else if path.is_file() {
                paths.push(path);
            }
        }
        Ok(())
    }
    
    collect_paths(dir, &mut all_paths, config)?;
    
    let processed_files: Vec<FileStats> = if config.parallel_processing {
        let counter = Arc::new(AtomicUsize::new(0));
        let total = all_paths.len();
        
        all_paths
            .par_iter()
            .map(|path| {
                let count = counter.fetch_add(1, Ordering::Relaxed);
                if count % 100 == 0 && count > 0 {
                    eprintln!("  Progress: {}/{} files", count, total);
                }
                analyze_file(path, config)
            })
            .collect()
    } else {
        all_paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                if i % 100 == 0 && i > 0 {
                    eprintln!("  Progress: {}/{} files", i, all_paths.len());
                }
                analyze_file(path, config)
            })
            .collect()
    };
    
    let mut stats = DirectoryStats::default();
    for file_stats in processed_files {
        if (file_stats.lines > 0 || config.show_empty_files) && file_stats.error.is_none() {
            stats.files += 1;
            stats.total_lines += file_stats.lines;
            stats.total_words += file_stats.words;
            stats.total_chars += file_stats.chars;
            stats.total_bytes += file_stats.bytes;
            stats.total_functions += file_stats.functions.len();
            stats.total_classes += file_stats.classes.len();
            stats.file_stats.push(file_stats);
        }
    }
    
    Ok(stats)
}

fn format_report(stats: &DirectoryStats, root: &Path, config: &Config) -> String {
    let mut report = String::new();
    
    report.push_str(&format!("📊 Directory Analysis: {}\n", root.display()));
    report.push_str(&format!("{}\n", "=".repeat(80)));
    
    report.push_str(&format!("\n📈 Summary:\n"));
    report.push_str(&format!("  Files analyzed: {}\n", stats.files));
    report.push_str(&format!("  Total lines: {}\n", stats.total_lines));
    report.push_str(&format!("  Total words: {}\n", stats.total_words));
    report.push_str(&format!("  Total chars: {}\n", stats.total_chars));
    report.push_str(&format!("  Total bytes: {}\n", stats.total_bytes));
    
    if config.show_function_details {
        report.push_str(&format!("  Total functions: {}\n", stats.total_functions));
    }
    if config.show_class_details {
        report.push_str(&format!("  Total classes/structs: {}\n", stats.total_classes));
    }
    
    if config.show_stats_per_file && !stats.file_stats.is_empty() {
        report.push_str(&format!("\n📁 Per-file Statistics:\n"));
        report.push_str(&format!("{}\n", "-".repeat(80)));
        
        for file_stat in stats.file_stats.iter().take(config.max_files_to_display) {
            let rel_path = file_stat.path.strip_prefix(root).unwrap_or(&file_stat.path);
            report.push_str(&format!("\n  📄 {}\n", rel_path.display()));
            
            if let Some(ref err) = file_stat.error {
                report.push_str(&format!("     ⚠ {}\n", err));
                continue;
            }
            
            report.push_str(&format!("     Lines: {} | Words: {} | Chars: {} | Bytes: {}\n",
                file_stat.lines, file_stat.words, file_stat.chars, file_stat.bytes
            ));
            
            if config.show_function_details && !file_stat.functions.is_empty() {
                let max_func_display = config.max_functions_per_file;
                report.push_str(&format!("     🎯 Functions ({})\n", file_stat.functions.len()));
                for func in file_stat.functions.iter().take(max_func_display) {
                    report.push_str(&format!("        • {} ({} lines, {} words, {} chars) [L{}-L{}]\n",
                        func.name, func.lines, func.words, func.chars, func.start_line, func.end_line));
                }
                if file_stat.functions.len() > max_func_display {
                    report.push_str(&format!("        ... and {} more\n", file_stat.functions.len() - max_func_display));
                }
            }
            
            if config.show_class_details && !file_stat.classes.is_empty() {
                let max_class_display = config.max_classes_per_file;
                report.push_str(&format!("     📦 Classes/Structs ({})\n", file_stat.classes.len()));
                for class in file_stat.classes.iter().take(max_class_display) {
                    report.push_str(&format!("        • {} ({} lines, {} words, {} chars) [L{}-L{}]\n",
                        class.name, class.lines, class.words, class.chars, class.start_line, class.end_line));
                    
                    if !class.methods.is_empty() {
                        for method in class.methods.iter().take(10) {
                            report.push_str(&format!("          └─ {} ({} lines) [L{}-L{}]\n",
                                method.name, method.lines, method.start_line, method.end_line));
                        }
                        if class.methods.len() > 10 {
                            report.push_str(&format!("          └─ ... and {} more methods\n", class.methods.len() - 10));
                        }
                    }
                }
                if file_stat.classes.len() > max_class_display {
                    report.push_str(&format!("        ... and {} more\n", file_stat.classes.len() - max_class_display));
                }
            }
            
            if config.show_usage_stats && !file_stat.uses.is_empty() {
                report.push_str(&format!("     📊 Usage statistics:\n"));
                for (name, count) in file_stat.uses.iter().take(10) {
                    report.push_str(&format!("        • {} used {} times\n", name, count));
                }
                if file_stat.uses.len() > 10 {
                    report.push_str(&format!("        ... and {} more\n", file_stat.uses.len() - 10));
                }
            }
        }
        
        if stats.file_stats.len() > config.max_files_to_display {
            report.push_str(&format!("\n  ... and {} more files not shown\n", 
                stats.file_stats.len() - config.max_files_to_display));
        }
    }
    
    report.push_str(&format!("\n{}\n", "=".repeat(80)));
    
    report
}

fn set_clipboard_content(content: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let mut child = Command::new("wl-copy")
                .arg("--type")
                .arg("text/plain;charset=utf-8")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("failed to spawn wl-copy")?;
            {
                let mut stdin = child.stdin.take().context("failed to open wl-copy stdin")?;
                stdin.write_all(content.as_bytes())?;
                stdin.flush()?;
            }
            let _ = child.wait();
            return Ok(());
        }
    }
    
    let mut cb = arboard::Clipboard::new().context("clipboard init failed")?;
    cb.set_text(content.to_string())?;
    
    #[cfg(target_os = "linux")]
    {
        thread::sleep(Duration::from_millis(200));
    }
    
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let target_dir = if args.is_empty() {
        env::current_dir()?
    } else {
        PathBuf::from(&args[0])
    };
    
    let config = load_config()?;
    
    eprintln!("🔍 Analyzing: {}", target_dir.display());
    eprintln!("📋 Config: max_size={}KB, threads={}, parallel={}", 
        config.max_file_size_kb, config.max_threads, config.parallel_processing);
    eprintln!("📋 Skip patterns: {:?}", config.skip_patterns);
    
    if config.parallel_processing {
        rayon::ThreadPoolBuilder::new()
            .num_threads(config.max_threads)
            .build_global()
            .context("Failed to build thread pool")?;
    }
    
    let start = std::time::Instant::now();
    let stats = walk_directory(&target_dir, &config)?;
    let duration = start.elapsed();
    
    let report = format_report(&stats, &target_dir, &config);
    
    println!("{}", report);
    
    set_clipboard_content(&report)?;
    eprintln!("\n✓ Report copied to clipboard! (took {:.2?})", duration);
    
    Ok(())
}