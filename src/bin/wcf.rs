// src/bin/wcf.rs
use std::{
    env,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};
use similar::{ChangeTag, TextDiff};
use syn::{
    parse_file, parse_str, File, ImplItem, Item, ItemFn, Visibility,
};
use wcc::common::*;
use wcc::config::load_unified_config;

#[derive(Debug, Clone)]
struct FunctionMatch {
    file_path: PathBuf,
    func_name: String,
    old_full_function: String,
    original_vis: Visibility,
    original_asyncness: Option<syn::token::Async>,
    original_unsafety: Option<syn::token::Unsafe>,
    original_attrs: Vec<syn::Attribute>,
}

fn parse_clipboard_functions(content: &str) -> Result<Vec<(String, ItemFn)>> {
    let content = content.trim();
    let mut functions = Vec::new();
    
    // Try to parse as a full file first
    match parse_file(content) {
        Ok(file) => {
            for item in file.items {
                if let Item::Fn(item_fn) = item {
                    let func_name = item_fn.sig.ident.to_string();
                    functions.push((func_name, item_fn));
                }
            }
        }
        Err(_) => {
            // Try to parse as individual function items
            let mut pos = 0;
            while let Some(start_pos) = content[pos..].find("fn ") {
                let abs_start = pos + start_pos;
                let mut brace_count = 0;
                let mut end_pos = abs_start;
                let mut in_brace = false;
                
                for (i, ch) in content[abs_start..].char_indices() {
                    match ch {
                        '{' => {
                            brace_count += 1;
                            in_brace = true;
                        }
                        '}' => {
                            brace_count -= 1;
                            if in_brace && brace_count == 0 {
                                end_pos = abs_start + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                
                let full_function = content[abs_start..end_pos].to_string();
                if let Ok(item_fn) = parse_str::<ItemFn>(&full_function) {
                    let func_name = item_fn.sig.ident.to_string();
                    functions.push((func_name, item_fn));
                }
                pos = end_pos;
            }
        }
    }
    
    if functions.is_empty() {
        bail!("No valid functions found in clipboard");
    }
    
    Ok(functions)
}

fn find_function_in_file(file_path: &Path, func_name: &str) -> Result<Vec<FunctionMatch>> {
    let content = fs::read_to_string(file_path)?;
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    
    // Search through all items in the file
    for item in file.items {
        match item {
            Item::Fn(item_fn) => {
                if item_fn.sig.ident == func_name {
                    let fn_str = quote::quote!(#item_fn).to_string();
                    matches.push(FunctionMatch {
                        file_path: file_path.to_path_buf(),
                        func_name: func_name.to_string(),
                        old_full_function: fn_str,
                        original_vis: item_fn.vis,
                        original_asyncness: item_fn.sig.asyncness,
                        original_unsafety: item_fn.sig.unsafety,
                        original_attrs: item_fn.attrs,
                    });
                }
            }
            Item::Impl(item_impl) => {
                for method in item_impl.items {
                    if let ImplItem::Fn(method_fn) = &method {
                        if method_fn.sig.ident == func_name {
                            let fn_str = quote::quote!(#method_fn).to_string();
                            matches.push(FunctionMatch {
                                file_path: file_path.to_path_buf(),
                                func_name: func_name.to_string(),
                                old_full_function: fn_str,
                                original_vis: method_fn.vis.clone(),
                                original_asyncness: method_fn.sig.asyncness,
                                original_unsafety: method_fn.sig.unsafety,
                                original_attrs: method_fn.attrs.clone(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    
    Ok(matches)
}

fn replace_function_in_file(content: &str, func_match: &FunctionMatch, new_item_fn: &ItemFn) -> Result<String> {
    let mut file: File = parse_file(content)?;
    
    // Create preserved signature - keep original modifiers
    let mut preserved_sig = new_item_fn.sig.clone();
    preserved_sig.asyncness = func_match.original_asyncness;
    preserved_sig.unsafety = func_match.original_unsafety;
    
    // Find and replace the function in the file
    for item in &mut file.items {
        match item {
            Item::Fn(item_fn) => {
                if item_fn.sig.ident == func_match.func_name {
                    // Replace free function
                    *item_fn = ItemFn {
                        attrs: func_match.original_attrs.clone(),
                        vis: func_match.original_vis.clone(),
                        sig: preserved_sig.clone(),
                        block: new_item_fn.block.clone(),
                    };
                    break;
                }
            }
            Item::Impl(item_impl) => {
                for method in &mut item_impl.items {
                    match method {
                        ImplItem::Fn(method_fn) => {
                            if method_fn.sig.ident == func_match.func_name {
                                // Replace method in impl block
                                method_fn.attrs = func_match.original_attrs.clone();
                                method_fn.vis = func_match.original_vis.clone();
                                method_fn.sig = preserved_sig.clone();
                                method_fn.block = (*new_item_fn.block).clone();
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    
    // Convert back to string
    let new_content = prettyplease::unparse(&file);
    Ok(new_content)
}

fn run_rustfmt_in_dir(file_path: &Path) -> Result<()> {
    let output = Command::new("rustfmt")
        .arg(file_path)
        .output()?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("  Warning: rustfmt had issues: {}", stderr);
        }
    }
    
    Ok(())
}

fn generate_clean_diff(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut result = String::new();
    
    for change in diff.iter_all_changes() {
        let line = change.as_str().unwrap_or("");
        match change.tag() {
            ChangeTag::Insert => {
                for line in line.lines() {
                    result.push_str(&format!("+{}\n", line));
                }
            }
            ChangeTag::Delete => {
                for line in line.lines() {
                    result.push_str(&format!("-{}\n", line));
                }
            }
            ChangeTag::Equal => {
                for line in line.lines() {
                    result.push_str(&format!(" {}\n", line));
                }
            }
        }
    }
    
    result
}

fn print_colored_diff(old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new);
    
    for change in diff.iter_all_changes() {
        let line = change.as_str().unwrap_or("");
        match change.tag() {
            ChangeTag::Insert => {
                for line in line.lines() {
                    println!("\x1b[32m+{}\x1b[0m", line);
                }
            }
            ChangeTag::Delete => {
                for line in line.lines() {
                    println!("\x1b[31m-{}\x1b[0m", line);
                }
            }
            ChangeTag::Equal => {
                for line in line.lines() {
                    println!(" {}\x1b[0m", line);
                }
            }
        }
    }
}

// Format code with rustfmt and optionally syntax highlight
fn format_code_with_rustfmt(code: &str) -> Result<String> {
    // Write code to temp file
    let temp_file = tempfile::NamedTempFile::new()?;
    let temp_path = temp_file.path();
    fs::write(temp_path, code)?;
    
    // Run rustfmt on temp file
    let output = Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .arg(temp_path)
        .output()?;
    
    if output.status.success() {
        let formatted = fs::read_to_string(temp_path)?;
        Ok(formatted)
    } else {
        Ok(code.to_string())
    }
}

// Show code with a pager for scrolling
fn show_with_pager(content: &str, title: &str) -> Result<()> {
    use std::io::Write;
    
    // Try to use bat for syntax highlighting (preferred)
    let bat_check = Command::new("bat")
        .arg("--version")
        .output();
    
    if bat_check.is_ok() {
        // Use bat with Rust syntax highlighting
        let mut child = Command::new("bat")
            .arg("--language=rust")
            .arg("--style=numbers,changes")
            .arg("--color=always")
            .arg("--paging=always")
            .arg("--wrap=never")
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()?;
        
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(content.as_bytes())?;
        }
        
        child.wait()?;
    } else {
        // Fallback to less with syntax highlighting disabled
        let mut child = Command::new("less")
            .arg("-R")
            .arg("-X")
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .spawn()?;
        
        if let Some(mut stdin) = child.stdin.take() {
            // Add colored header
            let header = format!("\x1b[1;36m{}\n\x1b[90m{}\x1b[0m\n\n", 
                title, 
                "=".repeat(80));
            stdin.write_all(header.as_bytes())?;
            stdin.write_all(content.as_bytes())?;
        }
        
        child.wait()?;
    }
    
    Ok(())
}

// Pretty print function with formatting
fn pretty_print_function(item_fn: &ItemFn, title: &str) -> Result<String> {
    let code = quote::quote!(#item_fn).to_string();
    let formatted = format_code_with_rustfmt(&code)?;
    
    // Add line numbers
    let lines: Vec<&str> = formatted.lines().collect();
    let numbered: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:4} {}", i + 1, line))
        .collect();
    
    let separator = "\x1b[90m".to_string() + &"-".repeat(60) + "\x1b[0m\n";
    let header = format!("\x1b[1;36m{}\x1b[0m\n{}\n", title, separator);
    
    Ok(format!("{}{}", header, numbered.join("\n")))
}

fn select_file_with_fzf(files: &[PathBuf]) -> Result<Option<PathBuf>> {
    let fzf_check = Command::new("fzf").arg("--version").output();
    if fzf_check.is_err() {
        bail!("fzf not found");
    }
    
    let file_list: String = files.iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    
    let mut fzf_child = Command::new("fzf")
        .arg("--height")
        .arg("40%")
        .arg("--border")
        .arg("--ansi")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn fzf")?;
    
    {
        let mut stdin = fzf_child.stdin.take().context("Failed to open fzf stdin")?;
        stdin.write_all(file_list.as_bytes())?;
    }
    
    let output = fzf_child.wait_with_output().context("Failed to read fzf output")?;
    
    if !output.status.success() {
        return Ok(None);
    }
    
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        return Ok(None);
    }
    
    Ok(Some(PathBuf::from(selected)))
}

fn scan_directory_for_function(dir: &Path, func_name: &str) -> Result<Vec<FunctionMatch>> {
    let mut all_matches = Vec::new();
    
    fn walk_dir(dir: &Path, func_name: &str, all_matches: &mut Vec<FunctionMatch>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            
            if path.is_dir() {
                let skip_dirs = ["target", "node_modules", ".git", ".cargo", ".idea", ".vscode"];
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if skip_dirs.contains(&name_str.as_ref()) {
                        continue;
                    }
                }
                walk_dir(&path, func_name, all_matches)?;
            } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(matches) = find_function_in_file(&path, func_name) {
                    all_matches.extend(matches);
                }
            }
        }
        Ok(())
    }
    
    walk_dir(dir, func_name, &mut all_matches)?;
    Ok(all_matches)
}

fn main() -> Result<()> {
    let config = load_unified_config()?;
    let args: Vec<String> = env::args().skip(1).collect();
    let target_dir = if args.is_empty() {
        env::current_dir()?
    } else {
        PathBuf::from(&args[0])
    };
    
    // Get clipboard content
    eprintln!("📋 Reading clipboard content...");
    let clipboard_content = get_clipboard_text()?;
    
    // Parse functions from clipboard
    eprintln!("🔍 Parsing functions from clipboard...");
    let mut functions = match parse_clipboard_functions(&clipboard_content) {
        Ok(funcs) => funcs,
        Err(e) => {
            eprintln!("⚠ Failed to parse clipboard as Rust functions: {}", e);
            eprintln!("Make sure the clipboard contains valid Rust function definitions");
            return Ok(());
        }
    };
    
    eprintln!("✓ Found {} function(s):", functions.len());
    for (name, _) in &functions {
        eprintln!("  • {}", name);
    }
    
    let mut processed_count = 0;
    let mut skipped_count = 0;
    
    // Process each function one by one
    while !functions.is_empty() {
        let (func_name, new_item_fn) = functions.remove(0);
        
        eprintln!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        eprintln!("📝 Processing function: \x1b[1;33m{}\x1b[0m", func_name);
        eprintln!("\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        
        // Show current buffer content if configured
        if config.wcf.show_buffer_preview {
            // Pretty print the function
            let pretty = pretty_print_function(&new_item_fn, "📋 Function from clipboard:")?;
            println!("{}", pretty);
            
            print!("\n❓ Continue with this function? (y/n/skip all): ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            
            match input.trim().to_lowercase().as_str() {
                "skip all" => {
                    eprintln!("⚠ Skipping all remaining functions");
                    break;
                }
                "n" | "no" => {
                    eprintln!("⚠ Skipping function: {}", func_name);
                    skipped_count += 1;
                    continue;
                }
                _ => {}
            }
        }
        
        // Scan directory for matching functions
        eprintln!("\n🔎 Scanning for function '{}'...", func_name);
        let matches = scan_directory_for_function(&target_dir, &func_name)?;
        
        if matches.is_empty() {
            eprintln!("⚠ No matches found for '{}', skipping", func_name);
            skipped_count += 1;
            continue;
        }
        
        eprintln!("✓ Found {} matching function(s):", matches.len());
        for m in &matches {
            let visibility = match &m.original_vis {
                Visibility::Public(_) => "pub",
                _ => "",
            };
            let asyncness = if m.original_asyncness.is_some() { "async" } else { "" };
            let unsafety = if m.original_unsafety.is_some() { "unsafe" } else { "" };
            
            eprintln!("  • {}", m.file_path.display());
            eprintln!("    Modifiers: {} {} {}", visibility, asyncness, unsafety);
        }
        
        // If multiple files, let user select which one to modify
        let selected_matches = if matches.len() > 1 {
            let files: Vec<PathBuf> = matches.iter().map(|m| m.file_path.clone()).collect();
            eprintln!("\n📁 Multiple files found. Select one to modify:");
            
            match select_file_with_fzf(&files)? {
                Some(selected_file) => {
                    matches.into_iter().filter(|m| m.file_path == selected_file).collect()
                }
                None => {
                    eprintln!("⚠ No file selected for '{}', skipping", func_name);
                    skipped_count += 1;
                    continue;
                }
            }
        } else {
            matches
        };
        
        // Preview changes with colored diff
        eprintln!("\n📝 Preview of changes:");
        for m in &selected_matches {
            eprintln!("\n  File: {}", m.file_path.display());
            eprintln!("  Original function:");
            println!("{}", m.old_full_function);
            
            eprintln!("\n  New function (will preserve modifiers):");
            let new_function_str = quote::quote!(#new_item_fn).to_string();
            let formatted = format_code_with_rustfmt(&new_function_str)?;
            println!("{}", formatted);
            
            eprintln!("\n  Diff:");
            print_colored_diff(&m.old_full_function, &new_function_str);
        }
        
        // Ask for confirmation
        print!("\n❓ Apply these changes? (y/n): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        
        if input.trim().to_lowercase() != "y" {
            eprintln!("⚠ Skipping function: {}", func_name);
            skipped_count += 1;
            continue;
        }
        
        // Apply changes
        eprintln!("\n🔄 Applying changes...");
        
        for func_match in &selected_matches {
            let file_path = &func_match.file_path;
            let old_content = fs::read_to_string(file_path)?;
            
            // Create backup only once per file
            let backup_path = PathBuf::from(format!("{}.bkp", file_path.display()));
            if !backup_path.exists() {
                fs::write(&backup_path, &old_content)?;
                eprintln!("  ✓ Created backup: {}", backup_path.display());
            }
            
            // Replace function
            let new_content = replace_function_in_file(&old_content, func_match, &new_item_fn)?;
            fs::write(file_path, &new_content)?;
            
            // Run rustfmt on the file
            if let Err(e) = run_rustfmt_in_dir(file_path) {
                eprintln!("  Warning: rustfmt failed: {}", e);
            }
            
            eprintln!("  ✓ Updated: {}", file_path.display());
            processed_count += 1;
        }
        
        eprintln!("\n✅ Function '{}' processed successfully!", func_name);
    }
    
    // Print final summary
    eprintln!("\n\x1b[1;32m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    eprintln!("\x1b[1;32m✅ All functions processed!\x1b[0m");
    eprintln!("  Functions processed: {}", processed_count);
    eprintln!("  Functions skipped: {}", skipped_count);
    
    Ok(())
}