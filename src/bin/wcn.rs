// src/bin/wcn.rs
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};
use chrono::{Local, DateTime};
use wcc::common::*;
use wcc::{config::load_unified_config, };

#[derive(Debug, Default)]
struct Args {
    file: Option<PathBuf>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    function: Option<String>,
}

fn main() -> Result<()> {
    let config = load_unified_config()?;
    let mut args = parse_args()?;

    // If no file provided, use fzf to select one
    if args.file.is_none() {
        let selected_file = select_file_with_fzf()?;
        args.file = Some(selected_file);
    }

    let path = args.file.clone().context("file argument required")?;
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

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

    // Add timestamp to header if configured
    let timestamp = if config.wcn.show_time_in_header {
        let time_str = if config.wcn.use_file_modification_time {
            // Get file modification time
            let metadata = fs::metadata(&path)?;
            let modified = metadata.modified()?;
            let datetime: DateTime<Local> = modified.into();
            datetime.format(&config.wcc.time_format).to_string()
        } else {
            // Use current time
            let now = Local::now();
            now.format(&config.wcc.time_format).to_string()
        };
        format!(" # {}", time_str)
    } else {
        String::new()
    };

    let payload = if header_present(&extracted_content, &filename) {
        extracted_content
    } else {
        format!("{prefix} {filename}{timestamp}\n{extracted_content}")
    };

    set_clipboard(&payload)?;
    let stats = calc_stats(&payload);

    print_stats(&relative_path, &stats, &args);

    Ok(())
}

fn select_file_with_fzf() -> Result<PathBuf> {
    // Check if fzf is available
    let fzf_check = Command::new("fzf").arg("--version").output();

    if fzf_check.is_err() {
        bail!("fzf not found. Please install fzf or provide a file argument");
    }

    // Use fd if available for better file listing, otherwise use find
    let files_output = if Command::new("fd").arg("--version").output().is_ok() {
        // Use fd for faster, gitignore-aware file listing
        let output = Command::new("fd")
            .arg("--type")
            .arg("f")
            .arg("--hidden")
            .arg("--exclude")
            .arg(".git")
            .arg("--exclude")
            .arg("target")
            .arg("--exclude")
            .arg("node_modules")
            .output()
            .context("Failed to run fd")?;

        String::from_utf8_lossy(&output.stdout).to_string()
    } else {
        // Fallback to find
        let output = Command::new("find")
            .arg(".")
            .arg("-type")
            .arg("f")
            .arg("-not")
            .arg("-path")
            .arg("*/.*")
            .arg("-not")
            .arg("-path")
            .arg("*/target/*")
            .arg("-not")
            .arg("-path")
            .arg("*/node_modules/*")
            .output()
            .context("Failed to run find")?;

        String::from_utf8_lossy(&output.stdout).to_string()
    };

    // Pipe files to fzf
    let mut fzf_child = Command::new("fzf")
        .arg("--height")
        .arg("40%")
        .arg("--border")
        .arg("--preview")
        .arg("bat --style=numbers --color=always --line-range=:500 {} 2>/dev/null || head -500 {}")
        .arg("--preview-window=right:60%")
        .arg("--ansi")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn fzf")?;

    {
        let mut stdin = fzf_child.stdin.take().context("Failed to open fzf stdin")?;
        use std::io::Write;
        stdin.write_all(files_output.as_bytes())?;
    }

    let output = fzf_child
        .wait_with_output()
        .context("Failed to read fzf output")?;

    if !output.status.success() {
        bail!("No file selected");
    }

    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        bail!("No file selected");
    }

    Ok(PathBuf::from(selected))
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let raw_args = env::args().skip(1).collect::<Vec<_>>();
    let mut i = 0;

    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "-h" | "--head" => {
                if i + 1 < raw_args.len() {
                    args.head_lines =
                        Some(raw_args[i + 1].parse().context("invalid number for -h")?);
                    i += 2;
                } else {
                    bail!("-h requires a number argument");
                }
            }
            "-t" | "--tail" => {
                if i + 1 < raw_args.len() {
                    args.tail_lines =
                        Some(raw_args[i + 1].parse().context("invalid number for -t")?);
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
    content.lines().take(2).any(|line| line.contains(filename))
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
