// src/bin/wff.rs
use std::{
    env,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use syn::{
    parse_file, ImplItem, Item, ItemFn, Visibility,
};
use wcc::common::*;
use wcc::config::{load_unified_config, UnifiedConfig};

#[derive(Debug, Clone)]
struct FunctionInfo {
    file_path: PathBuf,
    relative_path: PathBuf,
    func_name: String,
    func_body: String,
    start_line: usize,
    end_line: usize,
    file_lines: usize,
    file_modified: Option<DateTime<Local>>,
    visibility: Visibility,
    asyncness: Option<syn::token::Async>,
    unsafety: Option<syn::token::Unsafe>,
    attrs: Vec<syn::Attribute>,
}

fn main() -> Result<()> {
    let config = load_unified_config()?;
    let args: Vec<String> = env::args().skip(1).collect();
    let target_dir = if args.is_empty() {
        env::current_dir()?
    } else {
        PathBuf::from(&args[0])
    };

    if !target_dir.exists() {
        bail!("Directory does not exist: {}", target_dir.display());
    }

    eprintln!("🔍 Scanning directory: {}", target_dir.display());
    let functions = scan_directory_for_functions(&target_dir)?;

    if functions.is_empty() {
        bail!("No functions found in directory");
    }

    eprintln!("✓ Found {} function(s)", functions.len());
    for func in &functions {
        let visibility = match &func.visibility {
            Visibility::Public(_) => "pub",
            _ => "",
        };
        let asyncness = if func.asyncness.is_some() { "async" } else { "" };
        let modifiers = format!("{} {}", visibility, asyncness).trim().to_string();
        let modifier_str = if modifiers.is_empty() {
            String::new()
        } else {
            format!("{} ", modifiers)
        };
        
        eprintln!(
            "  • {}{} ({}: lines {}-{})",
            modifier_str,
            func.func_name,
            func.relative_path.display(),
            func.start_line,
            func.end_line
        );
    }

    // Select function to copy
    let selected = select_function_with_fzf(&functions)?;
    
    // Prepare clipboard content with header
    let header = format_header(&selected, &config)?;
    let payload = format!("{}\n{}", header, selected.func_body);
    
    set_clipboard(&payload)?;
    
    // Print statistics
    print_stats(&selected)?;
    
    eprintln!("\n\x1b[1;32m✓ Function copied to clipboard!\x1b[0m");
    
    Ok(())
}

fn scan_directory_for_functions(dir: &Path) -> Result<Vec<FunctionInfo>> {
    let mut functions = Vec::new();
    
    fn walk_dir(dir: &Path, functions: &mut Vec<FunctionInfo>) -> Result<()> {
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
                walk_dir(&path, functions)?;
            } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(funcs) = find_functions_in_file(&path) {
                    functions.extend(funcs);
                }
            }
        }
        Ok(())
    }
    
    walk_dir(dir, &mut functions)?;
    Ok(functions)
}

fn find_functions_in_file(file_path: &Path) -> Result<Vec<FunctionInfo>> {
    let content = fs::read_to_string(file_path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    
    // Get file modification time
    let metadata = fs::metadata(file_path)?;
    let modified = metadata.modified()?;
    let modified_datetime: DateTime<Local> = modified.into();
    
    // Get relative path
    let current_dir = env::current_dir()?;
    let relative_path = file_path
        .strip_prefix(&current_dir)
        .unwrap_or(file_path)
        .to_path_buf();
    
    let mut functions = Vec::new();
    
    // Search for function definitions using line-based parsing
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        
        // Check for function definition patterns
        let is_fn = line.contains("fn ") && (line.contains("fn ") && !line.trim_start().starts_with("//"));
        let is_pub_fn = line.contains("pub fn");
        let is_async_fn = line.contains("async fn");
        let is_pub_async_fn = line.contains("pub async fn");
        
        if is_fn || is_pub_fn || is_async_fn || is_pub_async_fn {
            // Extract function name
            let fn_name = extract_function_name(line);
            
            if let Some(name) = fn_name {
                // Find the function body
                let start_line = i + 1;
                let mut brace_count = 0;
                let mut found_brace = false;
                let mut end_line = start_line;
                
                for j in i..lines.len() {
                    let current_line = lines[j];
                    for ch in current_line.chars() {
                        if ch == '{' {
                            brace_count += 1;
                            found_brace = true;
                        } else if ch == '}' {
                            brace_count -= 1;
                            if found_brace && brace_count == 0 {
                                end_line = j + 1;
                                break;
                            }
                        }
                    }
                    if found_brace && brace_count == 0 {
                        break;
                    }
                }
                
                // Extract function body
                let func_body = lines[start_line - 1..end_line].join("\n");
                
                // Parse to get modifiers
                let (visibility, asyncness, unsafety, attrs) = if let Ok(item_fn) = syn::parse_str::<ItemFn>(&func_body) {
                    (item_fn.vis, item_fn.sig.asyncness, item_fn.sig.unsafety, item_fn.attrs)
                } else {
                    (Visibility::Inherited, None, None, Vec::new())
                };
                
                functions.push(FunctionInfo {
                    file_path: file_path.to_path_buf(),
                    relative_path: relative_path.clone(),
                    func_name: name,
                    func_body,
                    start_line,
                    end_line,
                    file_lines: total_lines,
                    file_modified: Some(modified_datetime),
                    visibility,
                    asyncness,
                    unsafety,
                    attrs,
                });
                
                i = end_line;
                continue;
            }
        }
        i += 1;
    }
    
    Ok(functions)
}

fn extract_function_name(line: &str) -> Option<String> {
    // Find "fn " pattern
    if let Some(fn_pos) = line.find("fn ") {
        let after_fn = &line[fn_pos + 3..];
        // Find the function name (alphanumeric + underscore)
        let name_end = after_fn
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(after_fn.len());
        let name = &after_fn[..name_end];
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

fn select_function_with_fzf(functions: &[FunctionInfo]) -> Result<FunctionInfo> {
    // Check if fzf is available
    let fzf_check = Command::new("fzf").arg("--version").output();
    if fzf_check.is_err() {
        bail!("fzf not found. Please install fzf to select functions");
    }
    
    // Create preview list
    let mut preview_lines = Vec::new();
    for (idx, func) in functions.iter().enumerate() {
        let visibility = match &func.visibility {
            Visibility::Public(_) => "pub",
            _ => "",
        };
        let asyncness = if func.asyncness.is_some() { "async" } else { "" };
        let modifiers = format!("{} {}", visibility, asyncness).trim().to_string();
        let modifier_str = if modifiers.is_empty() {
            String::new()
        } else {
            format!("{} ", modifiers)
        };
        
        preview_lines.push(format!(
            "{:3} │ {}{} │ {} │ {}-{}",
            idx + 1,
            modifier_str,
            func.func_name,
            func.relative_path.display(),
            func.start_line,
            func.end_line
        ));
    }
    
    let preview_text = preview_lines.join("\n");
    
    // Use fzf with preview showing function body
    let mut fzf_child = Command::new("fzf")
        .arg("--height")
        .arg("40%")
        .arg("--border")
        .arg("--ansi")
        .arg("--with-nth=2..")
        .arg("--delimiter=│")
        .arg("--preview")
        .arg("echo {4} && bat --style=numbers --color=always --language=rust --line-range={5}:{6} {4} 2>/dev/null || (echo '--- Function Body ---' && sed -n '{5},{6}p' {4})")
        .arg("--preview-window=right:60%")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn fzf")?;
    
    {
        let mut stdin = fzf_child.stdin.take().context("Failed to open fzf stdin")?;
        stdin.write_all(preview_text.as_bytes())?;
    }
    
    let output = fzf_child.wait_with_output().context("Failed to read fzf output")?;
    
    if !output.status.success() {
        bail!("No function selected");
    }
    
    let selected_line = String::from_utf8_lossy(&output.stdout);
    let first_line = selected_line.lines().next().unwrap_or("");
    
    // Extract index from first column
    let idx_str = first_line.split('│').next().unwrap_or("").trim();
    let idx: usize = idx_str.parse().unwrap_or(0);
    
    if idx == 0 || idx > functions.len() {
        bail!("Invalid selection");
    }
    
    Ok(functions[idx - 1].clone())
}

fn format_header(func: &FunctionInfo, config: &UnifiedConfig) -> Result<String> {
    let timestamp = if let Some(modified) = func.file_modified {
        modified.format(&config.wcc.time_format).to_string()
    } else {
        let now = Local::now();
        now.format(&config.wcc.time_format).to_string()
    };
    
    let comment_prefix = match func.file_path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "//",
        Some("py") => "#",
        Some("js") | Some("ts") | Some("jsx") | Some("tsx") => "//",
        Some("c") | Some("cc") | Some("cpp") | Some("h") | Some("hpp") => "//",
        Some("go") => "//",
        _ => "//",
    };
    
    Ok(format!(
        "{} {} # lines {}-{} # {}\n",
        comment_prefix,
        func.relative_path.display(),
        func.start_line,
        func.end_line,
        timestamp,
    ))
}

fn print_stats(func: &FunctionInfo) -> Result<()> {
    let stats = calc_stats(&func.func_body);
    let file_percentage = (func.func_body.lines().count() as f64 / func.file_lines as f64) * 100.0;
    
    let visibility = match &func.visibility {
        Visibility::Public(_) => "pub",
        _ => "",
    };
    let asyncness = if func.asyncness.is_some() { "async" } else { "" };
    let modifiers = format!("{} {}", visibility, asyncness).trim().to_string();
    let modifier_str = if modifiers.is_empty() {
        String::new()
    } else {
        format!("{} ", modifiers)
    };
    
    println!(
        "\n\x1b[36mfile\x1b[0m {}  \x1b[36mfunction\x1b[0m {}{}",
        func.relative_path.display(),
        modifier_str,
        func.func_name
    );
    println!(
        "  \x1b[33mlines\x1b[0m {} ({}% of file)  \x1b[33mwords\x1b[0m {}  \x1b[33mchars\x1b[0m {}  \x1b[33mbytes\x1b[0m {}",
        stats.lines,
        format!("{:.1}", file_percentage),
        stats.words,
        stats.chars,
        stats.bytes
    );
    println!(
        "  \x1b[33mline range\x1b[0m {}-{}",
        func.start_line, func.end_line
    );
    
    Ok(())
}