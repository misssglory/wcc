use std::{
    env,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use wcc::common::*;

#[derive(Debug, Clone, Default)]
struct FileStats {
    path: PathBuf,
    lines: usize,
    words: usize,
    chars: usize,
    bytes: usize,
    functions: Vec<String>,
    classes: Vec<String>,
    structs: Vec<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct DirectoryStats {
    files: usize,
    total_lines: usize,
    total_words: usize,
    total_chars: usize,
    total_bytes: usize,
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
    max_files_to_display: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_file_size_kb: 50,
            skip_patterns: vec![
                ".o".to_string(),
                ".pyc".to_string(),
                ".pyo".to_string(),
                ".so".to_string(),
                ".dll".to_string(),
                ".dylib".to_string(),
                ".exe".to_string(),
                ".class".to_string(),
                ".jar".to_string(),
                ".war".to_string(),
                ".ear".to_string(),
                ".zip".to_string(),
                ".tar".to_string(),
                ".gz".to_string(),
                ".bz2".to_string(),
                ".xz".to_string(),
                ".7z".to_string(),
                ".rar".to_string(),
                ".png".to_string(),
                ".jpg".to_string(),
                ".jpeg".to_string(),
                ".gif".to_string(),
                ".bmp".to_string(),
                ".ico".to_string(),
                ".mp3".to_string(),
                ".mp4".to_string(),
                ".avi".to_string(),
                ".mov".to_string(),
                ".pdf".to_string(),
                ".doc".to_string(),
                ".docx".to_string(),
            ],
            skip_dirs: vec![
                "target".to_string(),
                "node_modules".to_string(),
                ".git".to_string(),
                ".svn".to_string(),
                ".hg".to_string(),
                "build".to_string(),
                "dist".to_string(),
                "__pycache__".to_string(),
                ".cache".to_string(),
                ".cargo".to_string(),
                ".idea".to_string(),
                ".vscode".to_string(),
            ],
            show_empty_files: false,
            show_stats_per_file: true,
            show_function_details: true,
            max_files_to_display: 500,
        }
    }
}

fn load_config() -> Result<Config> {
    let mut config_paths = vec![];

    // User config directory
    if let Some(mut path) = dirs::config_dir() {
        path.push("wcl/config.toml");
        config_paths.push(path);
    }

    // Current directory config
    if let Ok(current_dir) = env::current_dir() {
        let mut path = current_dir.clone();
        path.push(".wclrc");
        config_paths.push(path);
        
        let mut path2 = current_dir;
        path2.push("wcl.toml");
        config_paths.push(path2);
    }

    // Home directory config
    if let Some(mut path) = dirs::home_dir() {
        path.push(".wclrc");
        config_paths.push(path);
    }

    // Try each config path
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

    // Create default config in user config directory if it doesn't exist
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
            eprintln!("  ⚠ Skipping {} ({} KB > {} KB limit)", path.display(), size_kb, config.max_file_size_kb);
            return true;
        }
    }

    // Check extension patterns
    if let Some(ext) = path.extension() {
        let ext_str = format!(".{}", ext.to_string_lossy().to_lowercase());
        for pattern in &config.skip_patterns {
            if ext_str == *pattern {
                return true;
            }
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

fn detect_functions_in_file(content: &str, ext: &str) -> Vec<String> {
    let mut functions = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    
    for line in lines.iter().take(500) {
        let trimmed = line.trim();
        
        match ext {
            "rs" => {
                if trimmed.contains("fn ") && !trimmed.contains("//") {
                    if let Some(name) = extract_function_name(trimmed, "fn ") {
                        functions.push(name);
                    }
                }
            }
            "py" => {
                if trimmed.contains("def ") && !trimmed.starts_with('#') {
                    if let Some(name) = extract_function_name(trimmed, "def ") {
                        functions.push(name);
                    }
                }
            }
            "js" | "ts" | "jsx" | "tsx" => {
                if trimmed.contains("function ") && !trimmed.contains("//") {
                    if let Some(name) = extract_function_name(trimmed, "function ") {
                        functions.push(name);
                    }
                } else if trimmed.contains("=>") && trimmed.contains("const ") {
                    if let Some(name) = extract_arrow_function_name(trimmed) {
                        functions.push(name);
                    }
                }
            }
            "c" | "cc" | "cpp" | "h" | "hpp" => {
                if trimmed.contains('(') && trimmed.contains(')') && !trimmed.starts_with("//") && !trimmed.starts_with("/*") {
                    if let Some(name) = extract_c_function_name(trimmed) {
                        functions.push(name);
                    }
                }
            }
            "go" => {
                if trimmed.contains("func ") {
                    if let Some(name) = extract_function_name(trimmed, "func ") {
                        functions.push(name);
                    }
                }
            }
            _ => {}
        }

        if functions.len() >= 50 {
            break;
        }
    }
    
    functions.sort();
    functions.dedup();
    functions
}

fn detect_classes_in_file(content: &str, ext: &str) -> Vec<String> {
    let mut classes = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    
    for line in lines.iter().take(500) {
        let trimmed = line.trim();
        
        match ext {
            "rs" => {
                if trimmed.contains("struct ") && !trimmed.contains("//") {
                    if let Some(name) = extract_class_name(trimmed, "struct ") {
                        classes.push(format!("struct {}", name));
                    }
                } else if trimmed.contains("enum ") && !trimmed.contains("//") {
                    if let Some(name) = extract_class_name(trimmed, "enum ") {
                        classes.push(format!("enum {}", name));
                    }
                } else if trimmed.contains("trait ") && !trimmed.contains("//") {
                    if let Some(name) = extract_class_name(trimmed, "trait ") {
                        classes.push(format!("trait {}", name));
                    }
                }
            }
            "py" => {
                if trimmed.contains("class ") && !trimmed.starts_with('#') {
                    if let Some(name) = extract_class_name(trimmed, "class ") {
                        classes.push(name);
                    }
                }
            }
            "js" | "ts" | "jsx" | "tsx" => {
                if trimmed.contains("class ") && !trimmed.contains("//") {
                    if let Some(name) = extract_class_name(trimmed, "class ") {
                        classes.push(name);
                    }
                }
            }
            "c" | "cc" | "cpp" | "h" | "hpp" => {
                if trimmed.contains("struct ") && !trimmed.starts_with("//") {
                    if let Some(name) = extract_class_name(trimmed, "struct ") {
                        classes.push(format!("struct {}", name));
                    }
                } else if trimmed.contains("class ") && !trimmed.starts_with("//") {
                    if let Some(name) = extract_class_name(trimmed, "class ") {
                        classes.push(format!("class {}", name));
                    }
                } else if trimmed.contains("enum ") && !trimmed.starts_with("//") {
                    if let Some(name) = extract_class_name(trimmed, "enum ") {
                        classes.push(format!("enum {}", name));
                    }
                }
            }
            "go" => {
                if trimmed.contains("type ") && trimmed.contains("struct") {
                    if let Some(name) = extract_class_name(trimmed, "type ") {
                        classes.push(format!("type {}", name));
                    }
                }
            }
            _ => {}
        }

        if classes.len() >= 50 {
            break;
        }
    }
    
    classes.sort();
    classes.dedup();
    classes
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
                stats.functions = detect_functions_in_file(&content, &ext);
                stats.classes = detect_classes_in_file(&content, &ext);
                stats.structs = vec![]; // Combined with classes for now
            }
        }
        Err(e) => {
            stats.error = Some(format!("Failed to read: {}", e));
        }
    }

    stats
}

fn walk_directory(dir: &Path, config: &Config) -> Result<DirectoryStats> {
    let mut stats = DirectoryStats::default();
    let mut entries: Vec<PathBuf> = Vec::new();

    if !dir.exists() {
        bail!("Directory does not exist: {}", dir.display());
    }

    if dir.is_file() {
        let file_stats = analyze_file(dir, config);
        stats.files = 1;
        stats.total_lines = file_stats.lines;
        stats.total_words = file_stats.words;
        stats.total_chars = file_stats.chars;
        stats.total_bytes = file_stats.bytes;
        stats.file_stats.push(file_stats);
        return Ok(stats);
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if should_skip_dir(&path, config) {
                eprintln!("  ⚠ Skipping directory: {}", path.display());
                continue;
            }
            let sub_stats = walk_directory(&path, config)?;
            stats.files += sub_stats.files;
            stats.total_lines += sub_stats.total_lines;
            stats.total_words += sub_stats.total_words;
            stats.total_chars += sub_stats.total_chars;
            stats.total_bytes += sub_stats.total_bytes;
            stats.file_stats.extend(sub_stats.file_stats);
        } else if path.is_file() {
            entries.push(path);
        }
    }

    // Sort entries for consistent output
    entries.sort();

    for path in entries {
        let file_stats = analyze_file(&path, config);
        if file_stats.lines > 0 || config.show_empty_files {
            stats.files += 1;
            stats.total_lines += file_stats.lines;
            stats.total_words += file_stats.words;
            stats.total_chars += file_stats.chars;
            stats.total_bytes += file_stats.bytes;
            stats.file_stats.push(file_stats);
        }
    }

    Ok(stats)
}

fn format_report(stats: &DirectoryStats, root: &Path, config: &Config) -> String {
    let mut report = String::new();
    
    report.push_str(&format!("📊 Directory Analysis: {}\n", root.display()));
    report.push_str(&format!("{}\n", "=".repeat(60)));
    
    report.push_str(&format!("\n📈 Summary:\n"));
    report.push_str(&format!("  Files analyzed: {}\n", stats.files));
    report.push_str(&format!("  Total lines: {}\n", stats.total_lines));
    report.push_str(&format!("  Total words: {}\n", stats.total_words));
    report.push_str(&format!("  Total chars: {}\n", stats.total_chars));
    report.push_str(&format!("  Total bytes: {}\n", stats.total_bytes));
    
    if config.show_stats_per_file && !stats.file_stats.is_empty() {
        report.push_str(&format!("\n📁 Per-file Statistics:\n"));
        report.push_str(&format!("{}\n", "-".repeat(60)));
        
        for (i, file_stat) in stats.file_stats.iter().enumerate().take(config.max_files_to_display) {
            let rel_path = file_stat.path.strip_prefix(root).unwrap_or(&file_stat.path);
            report.push_str(&format!("\n  📄 {}\n", rel_path.display()));
            
            if let Some(ref err) = file_stat.error {
                report.push_str(&format!("     ⚠ {}\n", err));
                continue;
            }
            
            report.push_str(&format!("     Lines: {} | Words: {} | Chars: {} | Bytes: {}\n",
                file_stat.lines,
                file_stat.words,
                file_stat.chars,
                file_stat.bytes
            ));
            
            if config.show_function_details {
                if !file_stat.functions.is_empty() {
                    report.push_str(&format!("     🎯 Functions ({})\n", file_stat.functions.len()));
                    for func in file_stat.functions.iter().take(10) {
                        report.push_str(&format!("        • {}\n", func));
                    }
                    if file_stat.functions.len() > 10 {
                        report.push_str(&format!("        ... and {} more\n", file_stat.functions.len() - 10));
                    }
                }
                
                if !file_stat.classes.is_empty() {
                    report.push_str(&format!("     📦 Classes/Structs ({})\n", file_stat.classes.len()));
                    for class in file_stat.classes.iter().take(10) {
                        report.push_str(&format!("        • {}\n", class));
                    }
                    if file_stat.classes.len() > 10 {
                        report.push_str(&format!("        ... and {} more\n", file_stat.classes.len() - 10));
                    }
                }
            }
        }
        
        if stats.file_stats.len() > config.max_files_to_display {
            report.push_str(&format!("\n  ... and {} more files not shown\n", 
                stats.file_stats.len() - config.max_files_to_display));
        }
    }
    
    report.push_str(&format!("\n{}\n", "=".repeat(60)));
    
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
    eprintln!("📋 Config: max_size={}KB, skip_patterns={}, skip_dirs={}", 
        config.max_file_size_kb, config.skip_patterns.len(), config.skip_dirs.len());
    
    let stats = walk_directory(&target_dir, &config)?;
    let report = format_report(&stats, &target_dir, &config);
    
    // Print to stdout for user
    println!("{}", report);
    
    // Copy to clipboard
    set_clipboard_content(&report)?;
    eprintln!("\n✓ Report copied to clipboard!");
    
    Ok(())
}