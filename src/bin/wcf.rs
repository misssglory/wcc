// src/bin/wcf.rs
use std::{
    env,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};
use regex::Regex;
use similar::{ChangeTag, TextDiff};
use wcc::common::*;
use wcc::load_unified_config;

#[derive(Debug, Clone)]
struct FunctionMatch {
    file_path: PathBuf,
    func_name: String,
    old_full_function: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug, Clone)]
struct FileChange {
    file_path: PathBuf,
    old_content: String,
    new_content: String,
    diff: String,
}

fn parse_clipboard_functions(content: &str) -> Result<Vec<(String, String)>> {
    let content = content.trim();
    let mut functions = Vec::new();
    let mut pos = 0;
    
    while let Some(start_pos) = content[pos..].find("fn ") {
        let abs_start = pos + start_pos;
        
        let before_fn = &content[..abs_start];
        let has_pub = before_fn.trim_end().ends_with("pub");
        
        let mut brace_count = 0;
        let mut end_pos = abs_start;
        let mut in_brace = false;
        
        for (i, ch) in content[abs_start..].char_indices() {
            let abs_pos = abs_start + i;
            match ch {
                '{' => {
                    brace_count += 1;
                    in_brace = true;
                }
                '}' => {
                    brace_count -= 1;
                    if in_brace && brace_count == 0 {
                        end_pos = abs_pos + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        
        let mut full_function = content[abs_start..end_pos].to_string();
        
        if has_pub && !full_function.trim_start().starts_with("pub") {
            full_function = format!("pub {}", full_function);
        }
        
        if !full_function.trim_end().ends_with('}') {
            full_function.push_str("\n}");
        }
        
        let name_re = Regex::new(r"fn\s+(\w+)")?;
        if let Some(func_name) = name_re.captures(&full_function)
            .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string())) {
            functions.push((func_name, full_function));
        }
        
        pos = end_pos;
    }
    
    if functions.is_empty() {
        bail!("No valid functions found in clipboard");
    }
    
    Ok(functions)
}

fn find_function_in_file(file_path: &Path, func_name: &str) -> Result<Option<FunctionMatch>> {
    let content = fs::read_to_string(file_path)?;
    let lines: Vec<&str> = content.lines().collect();
    
    let pattern = format!(r"^\s*(?:pub\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+{}\s*\(", func_name);
    let re = Regex::new(&pattern)?;
    
    let mut start_line = 0;
    let mut end_line = 0;
    let mut found_start = false;
    let mut brace_count = 0;
    let mut in_function = false;
    
    for (idx, line) in lines.iter().enumerate() {
        if !found_start && re.is_match(line) {
            start_line = idx;
            found_start = true;
            in_function = true;
        }
        
        if in_function {
            for ch in line.chars() {
                if ch == '{' {
                    brace_count += 1;
                } else if ch == '}' {
                    brace_count -= 1;
                    if brace_count == 0 {
                        end_line = idx;
                        in_function = false;
                        break;
                    }
                }
            }
            if !in_function && brace_count == 0 {
                break;
            }
        }
    }
    
    if found_start && end_line > start_line {
        let full_function = lines[start_line..=end_line].join("\n");
        return Ok(Some(FunctionMatch {
            file_path: file_path.to_path_buf(),
            func_name: func_name.to_string(),
            old_full_function: full_function,
            start_line: start_line + 1,
            end_line: end_line + 1,
        }));
    }
    
    Ok(None)
}

fn replace_function_in_file(content: &str, func_match: &FunctionMatch, new_function: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut result: Vec<String> = Vec::new();
    
    let original_line = lines[func_match.start_line - 1];
    let indent_len = original_line.len() - original_line.trim_start().len();
    let indent = original_line[..indent_len].to_string();
    
    for i in 0..func_match.start_line - 1 {
        result.push(lines[i].to_string());
    }
    
    for line in new_function.lines() {
        if line.trim().is_empty() {
            result.push(String::new());
        } else {
            result.push(format!("{}{}", indent, line));
        }
    }
    
    for i in func_match.end_line..lines.len() {
        result.push(lines[i].to_string());
    }
    
    Ok(result.join("\n"))
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
    let mut matches = Vec::new();
    
    fn walk_dir(dir: &Path, func_name: &str, matches: &mut Vec<FunctionMatch>) -> Result<()> {
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
                walk_dir(&path, func_name, matches)?;
            } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(Some(func_match)) = find_function_in_file(&path, func_name) {
                    matches.push(func_match);
                }
            }
        }
        Ok(())
    }
    
    walk_dir(dir, func_name, &mut matches)?;
    Ok(matches)
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
    
    let mut all_diffs = String::new();
    let mut processed_count = 0;
    let mut skipped_count = 0;
    
    // Process each function one by one
    while !functions.is_empty() {
        let (func_name, new_function) = functions.remove(0);
        
        eprintln!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        eprintln!("📝 Processing function: \x1b[1;33m{}\x1b[0m", func_name);
        eprintln!("\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        
        // Show current buffer content if configured
        if config.wcf.show_buffer_preview {
            eprintln!("\n\x1b[36m📋 Function preview:\x1b[0m");
            eprintln!("\x1b[90m{}\x1b[0m", "-".repeat(60));
            for (i, line) in new_function.lines().enumerate().take(20) {
                eprintln!("{:4} {}", i + 1, line);
            }
            if new_function.lines().count() > 20 {
                eprintln!("     ... {} more lines", new_function.lines().count() - 20);
            }
            eprintln!("\x1b[90m{}\x1b[0m", "-".repeat(60));
            
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
            eprintln!("  • {} (lines {}-{})", m.file_path.display(), m.start_line, m.end_line);
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
            eprintln!("  Old function (lines {}-{}):", m.start_line, m.end_line);
            print_colored_diff(&m.old_full_function, &new_function);
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
            let new_content = replace_function_in_file(&old_content, func_match, &new_function)?;
            fs::write(file_path, &new_content)?;
            
            // Run rustfmt on the file
            if let Err(e) = run_rustfmt_in_dir(file_path) {
                eprintln!("  Warning: rustfmt failed: {}", e);
            }
            
            // Generate diff for this file
            let file_diff = generate_clean_diff(&func_match.old_full_function, &new_function);
            let rel_path = file_path.strip_prefix(&target_dir).unwrap_or(file_path);
            
            all_diffs.push_str(&format!("// {} - Function: {}\n", rel_path.display(), func_name));
            all_diffs.push_str(&file_diff);
            all_diffs.push_str("\n");
            
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
    
    // Copy final diff to clipboard
    if !all_diffs.is_empty() {
        let final_output = format!(
            "// wcf Function Replacement Summary\n\
             // ============================================================\n\
             // Total functions processed: {}\n\
             // Total functions skipped: {}\n\
             // ============================================================\n\n\
             {}",
            processed_count,
            skipped_count,
            all_diffs
        );
        set_clipboard(&final_output)?;
        eprintln!("\n✓ Final diff copied to clipboard!");
    }
    
    Ok(())
}