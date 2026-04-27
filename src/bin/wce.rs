use anyhow::{Context, Result};
use regex::Regex;
use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use syn::spanned::Spanned;
use syn::{parse_file, ImplItem, Item, ItemFn, Visibility};
use wcc::common::*;
use wcc::config::load_unified_config;
#[derive(Debug, Clone)]
struct ErrorInfo {
    file_path: PathBuf,
    line: usize,
    column: usize,
    error_code: String,
    message: String,
    code_snippet: Option<String>,
    function_name: Option<String>,
    function_body: Option<String>,
    function_start_line: usize,
    function_end_line: usize,
    function_visibility: Option<String>,
    function_asyncness: bool,
}
#[derive(Debug, Clone)]
struct FunctionErrorGroup {
    function_name: String,
    function_body: String,
    function_start_line: usize,
    function_end_line: usize,
    function_visibility: Option<String>,
    function_asyncness: bool,
    file_path: PathBuf,
    errors: Vec<ErrorInfo>,
}
fn main() -> Result<()> {
    let _config = load_unified_config()?;
    eprintln!("📋 Reading clipboard content...");
    let clipboard_content = get_clipboard_text()?;
    eprintln!("🔍 Parsing cargo build errors...");
    let errors = parse_cargo_errors(&clipboard_content)?;
    if errors.is_empty() {
        eprintln!("✓ No errors found in clipboard");
        return Ok(());
    }
    let grouped_errors = group_errors_by_function(&errors);
    eprintln!(
        "⚠ Found {} error(s) in {} function(s)",
        errors.len(),
        grouped_errors.len()
    );
    for (idx, group) in grouped_errors.iter().enumerate() {
        eprintln!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        let visibility = group.function_visibility.as_deref().unwrap_or("");
        let asyncness = if group.function_asyncness {
            "async "
        } else {
            ""
        };
        let modifier = format!(
            "{}{}",
            visibility,
            if !visibility.is_empty() && !asyncness.is_empty() {
                " "
            } else {
                ""
            }
        );
        eprintln!(
            "\x1b[1;36mFunction {}: {}{}{}\x1b[0m",
            idx + 1,
            modifier,
            asyncness,
            group.function_name
        );
        eprintln!(
            "\x1b[90m  {}: lines {}-{}\x1b[0m",
            group.file_path.display(),
            group.function_start_line,
            group.function_end_line
        );
        eprintln!("\x1b[33m  {} error(s):\x1b[0m", group.errors.len());
        for error in &group.errors {
            eprintln!(
                "\x1b[31m    • [{}] at line {}: {}\x1b[0m",
                error.error_code, error.line, error.message
            );
        }
    }
    print!(
        "\n❓ Process {} error(s) in {} function(s) and copy to clipboard? (y/n): ",
        errors.len(),
        grouped_errors.len()
    );
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if input.trim().to_lowercase() != "y" {
        eprintln!("⚠ Aborted");
        return Ok(());
    }
    eprintln!("\n🔄 Building error report...");
    let output = build_grouped_error_report(&grouped_errors)?;
    set_clipboard(&output)?;
    print_stats(&errors, &grouped_errors, &output)?;
    eprintln!("\n\x1b[1;32m✓ Error report copied to clipboard!\x1b[0m");
    Ok(())
}
fn parse_cargo_errors(content: &str) -> Result<Vec<ErrorInfo>> {
    let mut errors = Vec::new();
    let error_re = Regex::new(r"error\[E(\d+)\]: (.+)")?;
    let location_re = Regex::new(r" --> (.+):(\d+):(\d+)")?;
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(caps) = error_re.captures(line) {
            let error_code = format!("E{}", caps.get(1).unwrap().as_str());
            let error_msg = caps.get(2).unwrap().as_str().to_string();
            let mut error = ErrorInfo {
                file_path: PathBuf::new(),
                line: 0,
                column: 0,
                error_code,
                message: error_msg,
                code_snippet: None,
                function_name: None,
                function_body: None,
                function_start_line: 0,
                function_end_line: 0,
                function_visibility: None,
                function_asyncness: false,
            };
            let mut j = i + 1;
            while j < lines.len() && j < i + 10 {
                let next_line = lines[j];
                if let Some(loc_caps) = location_re.captures(next_line) {
                    let file = loc_caps.get(1).unwrap().as_str();
                    error.file_path = PathBuf::from(file);
                    error.line = loc_caps.get(2).unwrap().as_str().parse().unwrap_or(0);
                    error.column = loc_caps.get(3).unwrap().as_str().parse().unwrap_or(0);
                    break;
                }
                j += 1;
            }
            if error.file_path.exists() && error.line > 0 {
                error.code_snippet = extract_code_snippet(&error.file_path, error.line);
                if let Some((func_name, func_body, start, end, visibility, asyncness)) =
                    find_function_with_syn(&error.file_path, error.line)
                {
                    error.function_name = Some(func_name);
                    error.function_body = Some(func_body);
                    error.function_start_line = start;
                    error.function_end_line = end;
                    error.function_visibility = visibility;
                    error.function_asyncness = asyncness;
                }
            }
            errors.push(error);
        }
        i += 1;
    }
    Ok(errors)
}
fn find_function_with_syn(
    file_path: &Path,
    line: usize,
) -> Option<(String, String, usize, usize, Option<String>, bool)> {
    let content = fs::read_to_string(file_path).ok()?;
    let file = parse_file(&content).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    for item in file.items {
        match item {
            Item::Fn(item_fn) => {
                let span = item_fn.span();
                let start_line = span.start().line;
                let end_line = find_function_end_syn(&lines, start_line);
                if line >= start_line && line <= end_line {
                    let visibility = match &item_fn.vis {
                        Visibility::Public(_) => Some("pub".to_string()),
                        _ => None,
                    };
                    let asyncness = item_fn.sig.asyncness.is_some();
                    let func_name = item_fn.sig.ident.to_string();
                    let func_body = lines[start_line - 1..end_line].join("\n");
                    return Some((
                        func_name, func_body, start_line, end_line, visibility, asyncness,
                    ));
                }
            }
            Item::Impl(item_impl) => {
                for method in item_impl.items {
                    if let ImplItem::Fn(method_fn) = method {
                        let span = method_fn.span();
                        let start_line = span.start().line;
                        let end_line = find_function_end_syn(&lines, start_line);
                        if line >= start_line && line <= end_line {
                            let visibility = match &method_fn.vis {
                                Visibility::Public(_) => Some("pub".to_string()),
                                _ => None,
                            };
                            let asyncness = method_fn.sig.asyncness.is_some();
                            let func_name = method_fn.sig.ident.to_string();
                            let func_body = lines[start_line - 1..end_line].join("\n");
                            return Some((
                                func_name, func_body, start_line, end_line, visibility, asyncness,
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}
fn find_function_end_syn(lines: &[&str], start_line: usize) -> usize {
    let mut brace_count = 0;
    let mut found_brace = false;
    let mut end_line = start_line;
    for i in (start_line - 1)..lines.len() {
        let line = lines[i];
        for ch in line.chars() {
            if ch == '{' {
                brace_count += 1;
                found_brace = true;
            } else if ch == '}' {
                brace_count -= 1;
                if found_brace && brace_count == 0 {
                    end_line = i + 1;
                    return end_line;
                }
            }
        }
    }
    end_line
}
fn extract_code_snippet(file_path: &Path, line: usize) -> Option<String> {
    let content = fs::read_to_string(file_path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = line.saturating_sub(2);
    let end = (line + 2).min(lines.len());
    let snippet: Vec<String> = (start..end)
        .map(|idx| {
            let line_num = idx + 1;
            let prefix = if line_num == line { "→ " } else { "  " };
            format!("{}{:4} {}", prefix, line_num, lines[idx])
        })
        .collect();
    Some(snippet.join("\n"))
}
fn group_errors_by_function(errors: &[ErrorInfo]) -> Vec<FunctionErrorGroup> {
    let mut groups: HashMap<String, FunctionErrorGroup> = HashMap::new();
    for error in errors {
        let key = if let Some(ref func_name) = error.function_name {
            format!("{}:{}", error.file_path.display(), func_name)
        } else {
            format!("{}:no-function", error.file_path.display())
        };
        if let Some(group) = groups.get_mut(&key) {
            group.errors.push(error.clone());
        } else {
            groups.insert(
                key,
                FunctionErrorGroup {
                    function_name: error
                        .function_name
                        .clone()
                        .unwrap_or_else(|| "[NO FUNCTION]".to_string()),
                    function_body: error.function_body.clone().unwrap_or_default(),
                    function_start_line: error.function_start_line,
                    function_end_line: error.function_end_line,
                    function_visibility: error.function_visibility.clone(),
                    function_asyncness: error.function_asyncness,
                    file_path: error.file_path.clone(),
                    errors: vec![error.clone()],
                },
            );
        }
    }
    let mut grouped: Vec<FunctionErrorGroup> = groups.into_values().collect();
    grouped.sort_by_key(|g| (g.file_path.clone(), g.function_start_line));
    grouped
}
fn build_grouped_error_report(groups: &[FunctionErrorGroup]) -> Result<String> {
    let mut output = String::new();
    let total_errors: usize = groups.iter().map(|g| g.errors.len()).sum();
    output.push_str(&format!(
        "// Cargo Build Error Report\n\
         // ============================================================\n\
         // Total errors: {} in {} functions\n\
         // ============================================================\n\n",
        total_errors,
        groups.len()
    ));
    for (idx, group) in groups.iter().enumerate() {
        let visibility = group.function_visibility.as_deref().unwrap_or("");
        let asyncness = if group.function_asyncness {
            "async "
        } else {
            ""
        };
        let modifier = if !visibility.is_empty() && !asyncness.is_empty() {
            format!("{} ", visibility)
        } else if !visibility.is_empty() {
            visibility.to_string()
        } else if !asyncness.is_empty() {
            asyncness.to_string()
        } else {
            String::new()
        };
        output.push_str(&format!("\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"));
        output.push_str(&format!(
            "Function {}: {}{}\n",
            idx + 1,
            if modifier.is_empty() {
                ""
            } else {
                &format!("{} ", modifier)
            },
            group.function_name
        ));
        output.push_str(&format!(
            "  {}: lines {}-{}\n",
            group.file_path.display(),
            group.function_start_line,
            group.function_end_line
        ));
        output.push_str(&format!("  {} error(s):\n", group.errors.len()));
        for error in &group.errors {
            output.push_str(&format!(
                "    • [{}] at line {}: {}\n",
                error.error_code, error.line, error.message
            ));
            if let Some(ref snippet) = error.code_snippet {
                output.push_str(&format!(
                    "\n  Code snippet (around line {}):\n{}\n",
                    error.line, snippet
                ));
            }
        }
        if !group.function_body.is_empty() {
            output.push_str(&format!(
                "\n  Full function body (lines {}-{}):\n",
                group.function_start_line, group.function_end_line
            ));
            let body_lines: Vec<&str> = group.function_body.lines().collect();
            for (line_num, line) in body_lines.iter().enumerate() {
                let actual_line = group.function_start_line + line_num;
                let has_error = group.errors.iter().any(|e| e.line == actual_line);
                if has_error {
                    output.push_str(&format!("  \x1b[31m{:4} {}\x1b[0m\n", actual_line, line));
                } else {
                    output.push_str(&format!("  {:4} {}\n", actual_line, line));
                }
            }
        } else if group.function_name == "[NO FUNCTION]" {
            output.push_str(&format!("\n⚠ [FUNCTION BODY NOT FOUND]\n"));
        }
        output.push_str("\n");
    }
    Ok(output)
}
fn print_stats(errors: &[ErrorInfo], groups: &[FunctionErrorGroup], output: &str) -> Result<()> {
    let stats = calc_stats(output);
    let functions_with_errors = groups.len();
    let functions_found = groups
        .iter()
        .filter(|g| g.function_name != "[NO FUNCTION]")
        .count();
    let functions_not_found = functions_with_errors - functions_found;
    println!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1;32m✓ Error Report Statistics:\x1b[0m");
    println!("  \x1b[33mTotal errors:\x1b[0m {}", errors.len());
    println!(
        "  \x1b[33mFunctions with errors:\x1b[0m {}",
        functions_with_errors
    );
    println!("  \x1b[32mFunctions found:\x1b[0m {}", functions_found);
    println!(
        "  \x1b[31mFunctions not found:\x1b[0m {}",
        functions_not_found
    );
    println!("  \x1b[33mTotal lines in report:\x1b[0m {}", stats.lines);
    println!("  \x1b[33mTotal words:\x1b[0m {}", stats.words);
    println!("  \x1b[33mTotal chars:\x1b[0m {}", stats.chars);
    println!("  \x1b[33mTotal bytes:\x1b[0m {}", stats.bytes);
    Ok(())
}
