// src/bin/wcn.rs
use std::{
    env,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use wcc::common::*;

#[derive(Debug, Default)]
struct Args {
    file: Option<PathBuf>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    function: Option<String>,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    
    let path = args.file.clone().context("file argument required")?;
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    // Extract content based on flags
    let extracted_content = extract_content(&content, &args, &path)?;

    // Get relative path for display
    let current_dir = env::current_dir()?;
    let relative_path = path
        .strip_prefix(&current_dir)
        .unwrap_or(&path)
        .to_path_buf();
    let filename = relative_path.display().to_string();

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let prefix = comment_prefix(&path, &ext);

    let payload = if header_present(&extracted_content, &filename) {
        extracted_content
    } else {
        format!("{prefix} {filename}\n{extracted_content}")
    };

    set_clipboard(&payload)?;
    let stats = calc_stats(&payload);

    print_stats(&relative_path, &stats, &args);

    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let raw_args = env::args().skip(1).collect::<Vec<_>>();
    let mut i = 0;
    
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "-h" | "--head" => {
                if i + 1 < raw_args.len() {
                    args.head_lines = Some(raw_args[i + 1].parse().context("invalid number for -h")?);
                    i += 2;
                } else {
                    bail!("-h requires a number argument");
                }
            }
            "-t" | "--tail" => {
                if i + 1 < raw_args.len() {
                    args.tail_lines = Some(raw_args[i + 1].parse().context("invalid number for -t")?);
                    i += 2;
                } else {
                    bail!("-t requires a number argument");
                }
            }
            "-f" | "--function" => {
                if i + 1 < raw_args.len() {
                    args.function = Some(raw_args[i + 1].clone());
                    i += 2;
                } else {
                    bail!("-f requires a function name argument");
                }
            }
            arg if !arg.starts_with('-') => {
                if args.file.is_none() {
                    args.file = Some(PathBuf::from(arg));
                } else {
                    bail!("multiple file arguments provided");
                }
                i += 1;
            }
            _ => {
                bail!("unknown flag: {}", raw_args[i]);
            }
        }
    }
    
    Ok(args)
}

fn extract_content(content: &str, args: &Args, path: &Path) -> Result<String> {
    // Priority: function extraction > head/tail combination > full content
    
    if let Some(func_name) = &args.function {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        return extract_function(content, func_name, &ext);
    }
    
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    
    match (args.head_lines, args.tail_lines) {
        (Some(head), Some(tail)) => {
            let start = 0;
            let end = head.min(total_lines);
            let tail_start = total_lines.saturating_sub(tail);
            
            let mut result = lines[start..end].join("\n");
            if end < tail_start {
                result.push_str(&format!("\n... ({}) lines omitted ...\n", tail_start - end));
            }
            if tail_start < total_lines {
                if end < tail_start {
                    result.push('\n');
                }
                result.push_str(&lines[tail_start..].join("\n"));
            }
            Ok(result)
        }
        (Some(head), None) => {
            let end = head.min(total_lines);
            Ok(lines[0..end].join("\n"))
        }
        (None, Some(tail)) => {
            let start = total_lines.saturating_sub(tail);
            Ok(lines[start..].join("\n"))
        }
        (None, None) => Ok(content.to_string()),
    }
}

fn extract_function(content: &str, func_name: &str, ext: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let escaped_name = regex_escape(func_name);
    
    // Language-specific function patterns
    let patterns: Vec<String> = match ext {
        "rs" => vec![
            format!(r"fn\s+{}", escaped_name),
            format!(r"pub\s+fn\s+{}", escaped_name),
            format!(r"async\s+fn\s+{}", escaped_name),
        ],
        "py" => vec![
            format!(r"def\s+{}", escaped_name),
            format!(r"async\s+def\s+{}", escaped_name),
            format!(r"class\s+{}", escaped_name),
        ],
        "js" | "ts" | "jsx" | "tsx" => vec![
            format!(r"function\s+{}", escaped_name),
            format!(r"const\s+{}\s*=", escaped_name),
            format!(r"let\s+{}\s*=", escaped_name),
            format!(r"class\s+{}", escaped_name),
        ],
        "c" | "cc" | "cpp" | "h" | "hpp" => vec![
            format!(r"{}", escaped_name),
            format!(r"class\s+{}", escaped_name),
        ],
        "go" => vec![
            format!(r"func\s+{}", escaped_name),
            format!(r"func\s+\([^)]+\)\s+{}", escaped_name),
        ],
        _ => vec![escaped_name],
    };
    
    let pattern = patterns.join("|");
    let re = regex::Regex::new(&format!(r"(?m)^{}$", pattern))
        .context("failed to create function regex")?;
    
    let mut func_start = None;
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            func_start = Some(i);
            break;
        }
    }
    
    let start = func_start.context(format!("function '{}' not found", func_name))?;
    let end = find_function_end(&lines, start, ext);
    
    Ok(lines[start..=end].join("\n"))
}

fn find_function_end(lines: &[&str], start: usize, ext: &str) -> usize {
    let mut brace_count = 0;
    let mut paren_count = 0;
    let mut found_opening_brace = false;
    
    for i in start..lines.len() {
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
            if i > start {
                let current_indent = line.len() - line.trim_start().len();
                let start_indent = lines[start].len() - lines[start].trim_start().len();
                
                if line.trim().is_empty() {
                    continue;
                }
                if current_indent <= start_indent && !line.trim().is_empty() {
                    return i - 1;
                }
            }
            continue;
        }
        
        if found_opening_brace && brace_count == 0 && paren_count == 0 {
            return i;
        }
        
        if !found_opening_brace && i > start {
            if line.contains(';') && !line.contains('{') {
                return i;
            }
        }
    }
    
    lines.len() - 1
}

fn header_present(content: &str, filename: &str) -> bool {
    content
        .lines()
        .take(2)
        .any(|line| line.contains(filename))
}

fn print_stats(path: &Path, stats: &TextStats, args: &Args) {
    let file = path.display();
    let mut flags = Vec::new();
    
    if args.head_lines.is_some() {
        flags.push(format!("-h {}", args.head_lines.unwrap()));
    }
    if args.tail_lines.is_some() {
        flags.push(format!("-t {}", args.tail_lines.unwrap()));
    }
    if args.function.is_some() {
        flags.push(format!("-f {}", args.function.as_ref().unwrap()));
    }
    
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    };
    
    println!(
        "\x1b[36mfile\x1b[0m {}{}  \x1b[33mlines\x1b[0m {}  \x1b[33mwords\x1b[0m {}  \x1b[33mchars\x1b[0m {}  \x1b[33mbytes\x1b[0m {}",
        file, flag_str, stats.lines, stats.words, stats.chars, stats.bytes
    );
}