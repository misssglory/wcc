// src/bin/wce.rs
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use regex::Regex;
use syn::{
    parse_file, ImplItem, Item, Visibility,
};
use syn::spanned::Spanned;
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

fn main() -> Result<()> {
    let _config = load_unified_config()?;
    
    // Get clipboard content
    eprintln!("📋 Reading clipboard content...");
    let clipboard_content = get_clipboard_text()?;
    
    // Parse cargo build output
    eprintln!("🔍 Parsing cargo build errors...");
    let errors = parse_cargo_errors(&clipboard_content)?;
    
    if errors.is_empty() {
        eprintln!("✓ No errors found in clipboard");
        return Ok(());
    }
    
    eprintln!("⚠ Found {} error(s)", errors.len());
    
    // Show errors and prompt for confirmation
    for (idx, error) in errors.iter().enumerate() {
        eprintln!("\n\x1b[1;31m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        eprintln!("\x1b[1;31mError {}: {}\x1b[0m", idx + 1, error.error_code);
        eprintln!("\x1b[90m  {}:{}:{}\x1b[0m", error.file_path.display(), error.line, error.column);
        eprintln!("\x1b[33m  {}\x1b[0m", error.message);
        
        if let Some(ref func) = error.function_name {
            let visibility = error.function_visibility.as_deref().unwrap_or("");
            let asyncness = if error.function_asyncness { "async " } else { "" };
            let modifier = if !visibility.is_empty() && !asyncness.is_empty() {
                format!("{} {}", visibility, asyncness)
            } else if !visibility.is_empty() {
                visibility.to_string()
            } else {
                asyncness.to_string()
            };
            eprintln!("\x1b[36m  → Function: {}{} (lines {}-{})\x1b[0m", 
                if modifier.is_empty() { "" } else { &format!("{} ", modifier) },
                func, 
                error.function_start_line, 
                error.function_end_line);
        }
    }
    
    print!("\n❓ Process {} error(s) and copy to clipboard? (y/n): ", errors.len());
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    
    if input.trim().to_lowercase() != "y" {
        eprintln!("⚠ Aborted");
        return Ok(());
    }
    
    // Build output
    eprintln!("\n🔄 Building error report...");
    let output = build_error_report(&errors)?;
    
    // Copy to clipboard
    set_clipboard(&output)?;
    
    // Print statistics
    print_stats(&errors, &output)?;
    
    eprintln!("\n\x1b[1;32m✓ Error report copied to clipboard!\x1b[0m");
    
    Ok(())
}

fn parse_cargo_errors(content: &str) -> Result<Vec<ErrorInfo>> {
    let mut errors = Vec::new();
    
    // Regex for error patterns
    let error_re = Regex::new(r"error\[E(\d+)\]: (.+)")?;
    let location_re = Regex::new(r" --> (.+):(\d+):(\d+)")?;
    
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    
    while i < lines.len() {
        let line = lines[i];
        
        // Look for error pattern
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
            
            // Look for location in next few lines
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
            
            // Extract code snippet and function using syn if file exists
            if error.file_path.exists() && error.line > 0 {
                error.code_snippet = extract_code_snippet(&error.file_path, error.line);
                
                if let Some((func_name, func_body, start, end, visibility, asyncness)) = find_function_with_syn(&error.file_path, error.line) {
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

fn find_function_with_syn(file_path: &Path, line: usize) -> Option<(String, String, usize, usize, Option<String>, bool)> {
    let content = fs::read_to_string(file_path).ok()?;
    let file = parse_file(&content).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    
    // Search through all items in the file
    for item in file.items {
        match item {
            Item::Fn(item_fn) => {
                let start_line = item_fn.span().start().line;
                let end_line = find_function_end(&lines, start_line);
                
                // Check if error line is within this function
                if line >= start_line && line <= end_line {
                    let visibility = match &item_fn.vis {
                        Visibility::Public(_) => Some("pub".to_string()),
                        _ => None,
                    };
                    let asyncness = item_fn.sig.asyncness.is_some();
                    let func_name = item_fn.sig.ident.to_string();
                    let func_body = lines[start_line - 1..end_line].join("\n");
                    
                    return Some((func_name, func_body, start_line, end_line, visibility, asyncness));
                }
            }
            Item::Impl(item_impl) => {
                for method in item_impl.items {
                    if let ImplItem::Fn(method_fn) = method {
                        let start_line = method_fn.span().start().line;
                        let end_line = find_function_end(&lines, start_line);
                        
                        // Check if error line is within this method
                        if line >= start_line && line <= end_line {
                            let visibility = match &method_fn.vis {
                                Visibility::Public(_) => Some("pub".to_string()),
                                _ => None,
                            };
                            let asyncness = method_fn.sig.asyncness.is_some();
                            let func_name = method_fn.sig.ident.to_string();
                            let func_body = lines[start_line - 1..end_line].join("\n");
                            
                            return Some((func_name, func_body, start_line, end_line, visibility, asyncness));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    
    None
}

fn find_function_end(lines: &[&str], start_line: usize) -> usize {
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
    
    let start = line.saturating_sub(3);
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

fn build_error_report(errors: &[ErrorInfo]) -> Result<String> {
    let mut output = String::new();
    
    output.push_str(&format!(
        "// Cargo Build Error Report\n\
         // ============================================================\n\
         // Total errors: {}\n\
         // ============================================================\n\n",
        errors.len()
    ));
    
    for (idx, error) in errors.iter().enumerate() {
        output.push_str(&format!(
            "\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n"
        ));
        output.push_str(&format!(
            "Error {}: {}\n",
            idx + 1,
            error.error_code
        ));
        output.push_str(&format!(
            "  {}:{}:{}\n",
            error.file_path.display(),
            error.line,
            error.column
        ));
        output.push_str(&format!("  {}\n", error.message));
        
        if let Some(ref snippet) = error.code_snippet {
            output.push_str(&format!("\nCode snippet:\n{}\n", snippet));
        }
        
        if let Some(ref func_name) = error.function_name {
            let visibility = error.function_visibility.as_deref().unwrap_or("");
            let asyncness = if error.function_asyncness { "async " } else { "" };
            let modifier = if !visibility.is_empty() && !asyncness.is_empty() {
                format!("{} {}", visibility, asyncness)
            } else if !visibility.is_empty() {
                visibility.to_string()
            } else {
                asyncness.to_string()
            };
            output.push_str(&format!(
                "\nFunction: {}{}\n",
                if modifier.is_empty() { "" } else { &format!("{} ", modifier) },
                func_name
            ));
            output.push_str(&format!(
                "  Lines: {}-{}\n",
                error.function_start_line,
                error.function_end_line
            ));
            
            if let Some(ref body) = error.function_body {
                output.push_str(&format!("\n{}\n", body));
            }
        } else if error.file_path.exists() {
            output.push_str(&format!(
                "\n⚠ [FUNCTION WAS NOT FOUND]\n"
            ));
            output.push_str(&format!(
                "  Error at line {}, but no enclosing function found\n",
                error.line
            ));
        }
        
        output.push_str("\n");
    }
    
    Ok(output)
}

fn print_stats(errors: &[ErrorInfo], output: &str) -> Result<()> {
    let stats = calc_stats(output);
    
    let mut functions_found = 0;
    let mut functions_not_found = 0;
    
    for error in errors {
        if error.function_name.is_some() {
            functions_found += 1;
        } else {
            functions_not_found += 1;
        }
    }
    
    println!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1;32m✓ Error Report Statistics:\x1b[0m");
    println!("  \x1b[33mTotal errors:\x1b[0m {}", errors.len());
    println!("  \x1b[32mFunctions found:\x1b[0m {}", functions_found);
    println!("  \x1b[31mFunctions not found:\x1b[0m {}", functions_not_found);
    println!("  \x1b[33mLines:\x1b[0m {}", stats.lines);
    println!("  \x1b[33mWords:\x1b[0m {}", stats.words);
    println!("  \x1b[33mChars:\x1b[0m {}", stats.chars);
    println!("  \x1b[33mBytes:\x1b[0m {}", stats.bytes);
    
    Ok(())
}