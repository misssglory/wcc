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

fn parse_clipboard_function(content: &str) -> Result<(String, String)> {
    let content = content.trim();
    
    let start_pos = content.find("fn ").context("No 'fn ' found in clipboard")?;
    
    let before_fn = &content[..start_pos];
    let has_pub = before_fn.trim_end().ends_with("pub");
    
    let mut brace_count = 0;
    let mut end_pos = start_pos;
    let mut in_brace = false;
    
    for (i, ch) in content[start_pos..].char_indices() {
        let abs_pos = start_pos + i;
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
    
    let mut full_function = content[start_pos..end_pos].to_string();
    
    if has_pub && !full_function.trim_start().starts_with("pub") {
        full_function = format!("pub {}", full_function);
    }
    
    if !full_function.trim_end().ends_with('}') {
        full_function.push_str("\n}");
    }
    
    let name_re = Regex::new(r"fn\s+(\w+)")?;
    let func_name = name_re.captures(&full_function)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .context("Could not find function name")?;
    
    Ok((func_name, full_function))
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
    
    // Parse function from clipboard
    eprintln!("🔍 Parsing function from clipboard...");
    let (func_name, new_function) = match parse_clipboard_function(&clipboard_content) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("⚠ Failed to parse clipboard as Rust function: {}", e);
            eprintln!("Make sure the clipboard contains a valid Rust function definition");
            return Ok(());
        }
    };
    
    eprintln!("✓ Found function: {}", func_name);
    eprintln!("  Function preview: {}", new_function.lines().next().unwrap_or(""));
    
    // Show current buffer content if configured
    if config.wcf.show_buffer_preview {
        eprintln!("\n\x1b[36m📋 Current buffer content:\x1b[0m");
        eprintln!("\x1b[90m{}\x1b[0m", "-".repeat(60));
        for (i, line) in new_function.lines().enumerate().take(30) {
            eprintln!("{:4} {}", i + 1, line);
        }
        if new_function.lines().count() > 30 {
            eprintln!("     ... {} more lines", new_function.lines().count() - 30);
        }
        eprintln!("\x1b[90m{}\x1b[0m", "-".repeat(60));
        
        print!("\n❓ Continue with this function? (y/n): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            eprintln!("❌ Aborted.");
            return Ok(());
        }
    }
    
    // Scan directory for matching functions
    eprintln!("\n🔎 Scanning for function '{}'...", func_name);
    let matches = scan_directory_for_function(&target_dir, &func_name)?;
    
    if matches.is_empty() {
        eprintln!("⚠ No matching functions found");
        return Ok(());
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
                eprintln!("⚠ No file selected");
                return Ok(());
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
        eprintln!("❌ Aborted.");
        return Ok(());
    }
    
    // Apply changes and collect diffs
    eprintln!("\n🔄 Applying changes...");
    let mut changes = Vec::new();
    let mut all_diffs = String::new();
    
    for func_match in &selected_matches {
        let file_path = &func_match.file_path;
        let old_content = fs::read_to_string(file_path)?;
        
        // Create backup
        let backup_path = PathBuf::from(format!("{}.bkp", file_path.display()));
        fs::write(&backup_path, &old_content)?;
        eprintln!("  ✓ Created backup: {}", backup_path.display());
        
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
        
        all_diffs.push_str(&format!("// {}\n", rel_path.display()));
        all_diffs.push_str(&file_diff);
        all_diffs.push_str("\n");
        
        changes.push(FileChange {
            file_path: file_path.clone(),
            old_content,
            new_content: fs::read_to_string(file_path)?,
            diff: file_diff,
        });
        
        eprintln!("  ✓ Updated: {}", file_path.display());
    }
    
    // Copy diff to clipboard
    if !all_diffs.is_empty() {
        set_clipboard(&all_diffs)?;
        eprintln!("\n✓ Diff copied to clipboard!");
    }
    
    // Print summary
    eprintln!("\n✅ Changes applied successfully!");
    eprintln!("  Files changed: {}", changes.len());
    eprintln!("  Functions replaced: {}", selected_matches.len());
    
    Ok(())
}