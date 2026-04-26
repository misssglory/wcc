// src/bin/wcl.rs
use std::{
    collections::BTreeMap,
    env, fs,
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
use wcc::common::*;
use wcc::config::load_unified_config;

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
    content: Option<String>,
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

fn should_skip_file(path: &Path, config: &wcc::WclConfig) -> bool {
    if let Ok(metadata) = fs::metadata(path) {
        let size_kb = metadata.len() / 1024;
        if size_kb > config.max_file_size_kb as u64 {
            return true;
        }
    }

    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let ext_with_dot = format!(".{}", ext.to_lowercase());

    for pattern in &config.skip_patterns {
        if ext_with_dot == *pattern {
            return true;
        }
        if filename.ends_with(pattern) {
            return true;
        }
        if filename == *pattern {
            return true;
        }
    }

    if let Ok(content) = fs::read(path) {
        if content.iter().take(1024).any(|&b| b == 0) {
            return true;
        }
    }

    false
}

fn should_skip_dir(path: &Path, config: &wcc::WclConfig) -> bool {
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

fn detect_functions_in_file(
    content: &str,
    ext: &str,
    config: &wcc::WclConfig,
) -> Vec<FunctionInfo> {
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
                if trimmed.contains('(')
                    && trimmed.contains(')')
                    && !trimmed.starts_with("//")
                    && !trimmed.starts_with("/*")
                {
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

fn detect_classes_in_file(content: &str, ext: &str, config: &wcc::WclConfig) -> Vec<ClassInfo> {
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

fn detect_usage_in_file(
    content: &str,
    functions: &[FunctionInfo],
    classes: &[ClassInfo],
) -> BTreeMap<String, usize> {
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

fn analyze_file(path: &Path, config: &wcc::WclConfig) -> FileStats {
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
            // Clone content for storage, keep original for analysis
            stats.content = Some(content.clone());
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

            if config.show_usage_stats
                && (config.show_function_details || config.show_class_details)
            {
                stats.uses = detect_usage_in_file(&content, &stats.functions, &stats.classes);
            }
        }
        Err(e) => {
            stats.error = Some(format!("Failed to read: {}", e));
        }
    }

    stats
}

fn walk_directory(dir: &Path, config: &wcc::WclConfig) -> Result<DirectoryStats> {
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

    fn collect_paths(dir: &Path, paths: &mut Vec<PathBuf>, config: &wcc::WclConfig) -> Result<()> {
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

fn get_consistent_color(filename: &str) -> String {
    let hash = filename
        .chars()
        .fold(0u64, |acc, c| acc.wrapping_add(c as u64).wrapping_mul(31));
    let hue = (hash % 360) as f64;
    let saturation = 0.7;
    let lightness = 0.6;
    let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());

    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    let m = l - c / 2.0;
    let r = ((r1 + m) * 255.0) as u8;
    let g = ((g1 + m) * 255.0) as u8;
    let b = ((b1 + m) * 255.0) as u8;
    (r, g, b)
}

fn format_report(stats: &DirectoryStats, root: &Path, config: &wcc::WclConfig) -> String {
    let mut report = String::new();

    report.push_str(&format!("📊 Directory Analysis: {}\n", root.display()));
    report.push_str(&format!("{}\n", "=".repeat(80)));

    report.push_str(&format!("\n📈 Summary:\n"));
    report.push_str(&format!("  Files analyzed: {}\n", stats.files));
    report.push_str(&format!(
        "  Total lines: {}\n",
        heatmap_color_lines(stats.total_lines)
    ));
    report.push_str(&format!(
        "  Total words: {}\n",
        heatmap_color_words(stats.total_words)
    ));
    report.push_str(&format!(
        "  Total chars: {}\n",
        heatmap_color_chars(stats.total_chars)
    ));
    report.push_str(&format!(
        "  Total bytes: {}\n",
        heatmap_color_bytes(stats.total_bytes)
    ));

    if config.show_function_details {
        report.push_str(&format!("  Total functions: {}\n", stats.total_functions));
    }
    if config.show_class_details {
        report.push_str(&format!(
            "  Total classes/structs: {}\n",
            stats.total_classes
        ));
    }

    if config.show_stats_per_file && !stats.file_stats.is_empty() {
        report.push_str(&format!("\n📁 Per-file Statistics:\n"));
        report.push_str(&format!("{}\n", "-".repeat(80)));

        for file_stat in stats.file_stats.iter().take(config.max_files_to_display) {
            let rel_path = file_stat.path.strip_prefix(root).unwrap_or(&file_stat.path);
            let color = get_consistent_color(&rel_path.display().to_string());

            report.push_str(&format!("\n  📄 {}{}\x1b[0m\n", color, rel_path.display()));

            if let Some(ref err) = file_stat.error {
                report.push_str(&format!("     ⚠ {}\n", err));
                continue;
            }

            report.push_str(&format!(
                "     Lines: {} | Words: {} | Chars: {} | Bytes: {}\n",
                heatmap_color_lines(file_stat.lines),
                heatmap_color_words(file_stat.words),
                heatmap_color_chars(file_stat.chars),
                heatmap_color_bytes(file_stat.bytes)
            ));

            if config.show_function_details && !file_stat.functions.is_empty() {
                let max_func_display = config.max_functions_per_file.min(20);
                report.push_str(&format!(
                    "     🎯 Functions ({})\n",
                    file_stat.functions.len()
                ));
                for func in file_stat.functions.iter().take(max_func_display) {
                    report.push_str(&format!(
                        "        • {} ({} lines, {} words, {} chars) [L{}-L{}]\n",
                        func.name,
                        heatmap_color_lines(func.lines),
                        heatmap_color_words(func.words),
                        heatmap_color_chars(func.chars),
                        func.start_line,
                        func.end_line
                    ));
                }
                if file_stat.functions.len() > max_func_display {
                    report.push_str(&format!(
                        "        ... and {} more\n",
                        file_stat.functions.len() - max_func_display
                    ));
                }
            }

            if config.show_class_details && !file_stat.classes.is_empty() {
                let max_class_display = config.max_classes_per_file.min(20);
                report.push_str(&format!(
                    "     📦 Classes/Structs ({})\n",
                    file_stat.classes.len()
                ));
                for class in file_stat.classes.iter().take(max_class_display) {
                    report.push_str(&format!(
                        "        • {} ({} lines, {} words, {} chars) [L{}-L{}]\n",
                        class.name,
                        heatmap_color_lines(class.lines),
                        heatmap_color_words(class.words),
                        heatmap_color_chars(class.chars),
                        class.start_line,
                        class.end_line
                    ));

                    if !class.methods.is_empty() {
                        for method in class.methods.iter().take(5) {
                            report.push_str(&format!(
                                "          └─ {} ({} lines) [L{}-L{}]\n",
                                method.name,
                                heatmap_color_lines(method.lines),
                                method.start_line,
                                method.end_line
                            ));
                        }
                        if class.methods.len() > 5 {
                            report.push_str(&format!(
                                "          └─ ... and {} more methods\n",
                                class.methods.len() - 5
                            ));
                        }
                    }
                }
                if file_stat.classes.len() > max_class_display {
                    report.push_str(&format!(
                        "        ... and {} more\n",
                        file_stat.classes.len() - max_class_display
                    ));
                }
            }

            if config.show_usage_stats && !file_stat.uses.is_empty() {
                report.push_str(&format!("     📊 Usage statistics:\n"));
                for (name, count) in file_stat.uses.iter().take(10) {
                    report.push_str(&format!("        • {} used {} times\n", name, count));
                }
                if file_stat.uses.len() > 10 {
                    report.push_str(&format!(
                        "        ... and {} more\n",
                        file_stat.uses.len() - 10
                    ));
                }
            }
        }

        if stats.file_stats.len() > config.max_files_to_display {
            report.push_str(&format!(
                "\n  ... and {} more files not shown\n",
                stats.file_stats.len() - config.max_files_to_display
            ));
        }
    }

    report.push_str(&format!("\n{}\n", "=".repeat(80)));

    report
}

fn select_files_for_clipboard<'a>(
    stats: &'a DirectoryStats,
    _root: &Path,
    max_bytes: usize,
) -> Vec<&'a FileStats> {
    let mut files: Vec<&'a FileStats> = stats
        .file_stats
        .iter()
        .filter(|f| f.content.is_some() && f.error.is_none())
        .collect();

    files.sort_by_key(|f| f.bytes);

    let mut selected = Vec::new();
    let mut total_bytes = 0;

    for file in files {
        if total_bytes + file.bytes <= max_bytes {
            selected.push(file);
            total_bytes += file.bytes;
        } else {
            break;
        }
    }

    selected
}

fn format_file_contents_for_clipboard(
    stats: &DirectoryStats,
    root: &Path,
    config: &wcc::WclConfig,
) -> String {
    let mut clipboard_content = String::new();
    let max_clipboard_bytes = config.max_clipboard_bytes;
    let selected_files = select_files_for_clipboard(stats, root, max_clipboard_bytes);
    let mut total_copied_bytes = 0;

    clipboard_content.push_str(&format!("// wcl Analysis Report\n"));
    clipboard_content.push_str(&format!(
        "// ============================================================\n"
    ));
    clipboard_content.push_str(&format!(
        "// Total files: {} | Total size: {} bytes\n",
        stats.files, stats.total_bytes
    ));
    clipboard_content.push_str(&format!(
        "// Clipboard limit: {} bytes ({} KB)\n",
        max_clipboard_bytes,
        max_clipboard_bytes / 1024
    ));
    clipboard_content.push_str(&format!(
        "// ============================================================\n\n"
    ));

    // Files that were copied (full content)
    if !selected_files.is_empty() {
        clipboard_content.push_str(&format!("// 📁 COPIED FILES (full content):\n"));
        clipboard_content.push_str(&format!("// {}\n", "-".repeat(60)));

        for file_stat in &selected_files {
            if let Some(content) = &file_stat.content {
                let rel_path = file_stat.path.strip_prefix(root).unwrap_or(&file_stat.path);
                let ext = file_stat
                    .path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");

                let comment_prefix = match ext {
                    "rs" | "c" | "cc" | "cpp" | "h" | "hpp" | "js" | "ts" | "jsx" | "tsx"
                    | "java" | "go" => "//",
                    "py" | "sh" | "bash" | "rb" | "pl" => "#",
                    _ => "#",
                };

                clipboard_content.push_str(&format!("{} {}\n", comment_prefix, rel_path.display()));
                clipboard_content.push_str(&format!(
                    "{} Size: {} bytes | Lines: {} | Words: {} | Functions: {} | Classes: {}\n",
                    comment_prefix,
                    file_stat.bytes,
                    file_stat.lines,
                    file_stat.words,
                    file_stat.functions.len(),
                    file_stat.classes.len()
                ));
                clipboard_content.push_str(&format!("{}\n", "-".repeat(80)));
                clipboard_content.push_str(content);
                clipboard_content.push_str("\n\n");
                total_copied_bytes += file_stat.bytes;
            }
        }
    }

    // Files that were NOT copied (only statistics)
    let not_copied: Vec<&FileStats> = stats
        .file_stats
        .iter()
        .filter(|f| f.content.is_some() && f.error.is_none())
        .filter(|f| !selected_files.iter().any(|sf| std::ptr::eq(*sf, *f)))
        .collect();

    if !not_copied.is_empty() {
        clipboard_content.push_str(&format!(
            "// 📊 FILES NOT COPIED (exceeded byte limit) - Statistics Only:\n"
        ));
        clipboard_content.push_str(&format!("// {}\n", "-".repeat(60)));

        for file in not_copied.iter().take(100) {
            let rel_path = file.path.strip_prefix(root).unwrap_or(&file.path);
            clipboard_content.push_str(&format!(
                "//   {} ({} bytes, {} lines, {} words, {} functions, {} classes)\n",
                rel_path.display(),
                file.bytes,
                file.lines,
                file.words,
                file.functions.len(),
                file.classes.len()
            ));

            // Show function names for not-copied files (limited)
            if config.show_function_details && !file.functions.is_empty() {
                let func_names: Vec<String> = file
                    .functions
                    .iter()
                    .take(5)
                    .map(|f| f.name.clone())
                    .collect();
                clipboard_content
                    .push_str(&format!("//     Functions: {}\n", func_names.join(", ")));
                if file.functions.len() > 5 {
                    clipboard_content.push_str(&format!(
                        "//       ... and {} more\n",
                        file.functions.len() - 5
                    ));
                }
            }

            // Show class names for not-copied files (limited)
            if config.show_class_details && !file.classes.is_empty() {
                let class_names: Vec<String> = file
                    .classes
                    .iter()
                    .take(3)
                    .map(|c| c.name.clone())
                    .collect();
                clipboard_content
                    .push_str(&format!("//     Classes: {}\n", class_names.join(", ")));
                if file.classes.len() > 3 {
                    clipboard_content.push_str(&format!(
                        "//       ... and {} more\n",
                        file.classes.len() - 3
                    ));
                }
            }
        }

        if not_copied.len() > 100 {
            clipboard_content.push_str(&format!(
                "//   ... and {} more files (statistics only)\n",
                not_copied.len() - 100
            ));
        }
    }

    // Summary
    clipboard_content.push_str(&format!(
        "\n// ============================================================\n"
    ));
    clipboard_content.push_str(&format!("// SUMMARY:\n"));
    clipboard_content.push_str(&format!(
        "//   Files copied: {} ({} bytes / {} bytes limit)\n",
        selected_files.len(),
        total_copied_bytes,
        max_clipboard_bytes
    ));
    clipboard_content.push_str(&format!(
        "//   Files with statistics only: {}\n",
        not_copied.len()
    ));
    clipboard_content.push_str(&format!(
        "//   Total functions found: {}\n",
        stats.total_functions
    ));
    clipboard_content.push_str(&format!(
        "//   Total classes found: {}\n",
        stats.total_classes
    ));
    clipboard_content.push_str(&format!(
        "// ============================================================\n"
    ));

    clipboard_content
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

    let unified_config = load_unified_config()?;
    let config = &unified_config.wcl;

    eprintln!("🔍 Analyzing: {}", target_dir.display());
    eprintln!(
        "📋 Config: max_size={}KB, clipboard_limit={}KB, threads={}, parallel={}",
        config.max_file_size_kb,
        config.max_clipboard_bytes / 1024,
        config.max_threads,
        config.parallel_processing
    );
    eprintln!("📋 Skip patterns: {:?}", config.skip_patterns);
    eprintln!("📋 Skip dirs: {:?}", config.skip_dirs);

    if config.parallel_processing {
        rayon::ThreadPoolBuilder::new()
            .num_threads(config.max_threads)
            .build_global()
            .context("Failed to build thread pool")?;
    }

    let start = std::time::Instant::now();
    let stats = walk_directory(&target_dir, config)?;
    let duration = start.elapsed();

    let report = format_report(&stats, &target_dir, config);
    println!("{}", report);

    // Prepare file contents for clipboard
    let clipboard_content = format_file_contents_for_clipboard(&stats, &target_dir, config);

    set_clipboard_content(&clipboard_content)?;
    eprintln!(
        "\n✓ Report and file contents copied to clipboard! (took {:.2?})",
        duration
    );
    eprintln!(
        "  Files with total size <= {} KB were included",
        config.max_clipboard_bytes / 1024
    );

    Ok(())
}
