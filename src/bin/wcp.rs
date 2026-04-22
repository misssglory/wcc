// wcp.rs
use std::{
    collections::BTreeMap,
    env,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
};
use anyhow::{bail, Context, Result};
use arboard::Clipboard;
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Default)]
struct TextStats {
    lines: usize,
    words: usize,
    chars: usize,
    bytes: usize,
}

#[derive(Debug, Clone, Default)]
struct DiffStats {
    added_lines: usize,
    removed_lines: usize,
    changed: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    
    // Determine target path
    let path = if args.len() == 1 {
        // Path provided as argument
        PathBuf::from(args[0].clone())
    } else {
        // Try to deduce filename from clipboard content
        let clipboard_content = get_clipboard_text()?;
        match deduce_filename_from_content(&clipboard_content) {
            Some(filename) => {
                println!("\x1b[36minfo\x1b[0m deduced filename from content: {}", filename);
                PathBuf::from(filename)
            }
            None => {
                // Interactive filename selection
                println!("\x1b[33mwarning\x1b[0m could not deduce filename, please enter a filename:");
                let filename = get_user_filename()?;
                PathBuf::from(filename)
            }
        }
    };
    
    let new_content = get_clipboard_text()?;
    
    let old_content = if path.exists() {
        let old = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let backup = backup_path(&path);
        fs::write(&backup, &old)
            .with_context(|| format!("failed to write backup {}", backup.display()))?;
        println!("\x1b[36minfo\x1b[0m backup written to {}", backup.display());
        Some(old)
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            println!("\x1b[36minfo\x1b[0m created directory: {}", parent.display());
        }
        None
    };
    
    fs::write(&path, &new_content)
        .with_context(|| format!("failed to write {}", path.display()))?;
    
    println!("\x1b[36minfo\x1b[0m output written to: {}", path.display());
    
    let new_stats = calc_stats(&new_content);
    println!(
        "\x1b[36mfile\x1b[0m {}  \x1b[33mlines\x1b[0m {}  \x1b[33mwords\x1b[0m {}  \x1b[33mchars\x1b[0m {}  \x1b[33mbytes\x1b[0m {}",
        path.display(),
        new_stats.lines,
        new_stats.words,
        new_stats.chars,
        new_stats.bytes
    );
    
    if let Some(old) = old_content {
        let diff_stats = diff_stats(&old, &new_content);
        println!(
            "\x1b[35mdiff\x1b[0m changed={}  \x1b[32m+{}\x1b[0m  \x1b[31m-{}\x1b[0m",
            diff_stats.changed, diff_stats.added_lines, diff_stats.removed_lines
        );
        let per_fn = function_diff_stats(&old, &new_content);
        if !per_fn.is_empty() {
            println!("\x1b[34mper-function diff\x1b[0m");
            for (name, stats) in per_fn {
                println!(
                    "  {}  \x1b[32m+{}\x1b[0m \x1b[31m-{}\x1b[0m changed={}",
                    name, stats.added_lines, stats.removed_lines, stats.changed
                );
            }
        }
    }
    
    Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bkp");
    PathBuf::from(s)
}

fn calc_stats(s: &str) -> TextStats {
    TextStats {
        lines: s.bytes().filter(|b| *b == b'\n').count(),
        words: s.split_whitespace().count(),
        chars: s.chars().count(),
        bytes: s.as_bytes().len(),
    }
}

fn diff_stats(old: &str, new: &str) -> DiffStats {
    let diff = TextDiff::from_lines(old, new);
    let mut stats = DiffStats::default();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => {
                stats.removed_lines += 1;
                stats.changed = true;
            }
            ChangeTag::Insert => {
                stats.added_lines += 1;
                stats.changed = true;
            }
            ChangeTag::Equal => {}
        }
    }
    stats
}

fn function_diff_stats(old: &str, new: &str) -> BTreeMap<String, DiffStats> {
    let old_map = map_lines_to_function(old);
    let new_map = map_lines_to_function(new);
    let diff = TextDiff::from_lines(old, new);
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let mut stats: BTreeMap<String, DiffStats> = BTreeMap::new();
    
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
            }
            ChangeTag::Delete => {
                let name = old_map.get(&old_line).cloned().unwrap_or_else(|| "<global>".to_string());
                let entry = stats.entry(name).or_default();
                entry.removed_lines += 1;
                entry.changed = true;
                old_line += 1;
            }
            ChangeTag::Insert => {
                let name = new_map.get(&new_line).cloned().unwrap_or_else(|| "<global>".to_string());
                let entry = stats.entry(name).or_default();
                entry.added_lines += 1;
                entry.changed = true;
                new_line += 1;
            }
        }
    }
    
    stats.retain(|_, v| v.changed);
    stats
}

fn map_lines_to_function(src: &str) -> BTreeMap<usize, String> {
    let mut map = BTreeMap::new();
    let mut current = "<global>".to_string();
    for (idx, line) in src.lines().enumerate() {
        if let Some(name) = detect_function_name(line) {
            current = name;
        }
        map.insert(idx, current.clone());
    }
    map
}

fn detect_function_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let prefixes = [
        "fn ", "pub fn ", "pub(crate) fn ", "def ", "function ", "class ", "impl ",
    ];
    for p in prefixes {
        if let Some(rest) = trimmed.strip_prefix(p) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':' || *c == '<' || *c == '>')
                .collect();
            if !name.is_empty() {
                return Some(format!("{}{}", p.trim(), name));
            }
        }
    }
    if trimmed.contains('(') && trimmed.ends_with('{') {
        let before = trimmed.split('(').next().unwrap_or(trimmed).trim();
        let name = before.split_whitespace().last().unwrap_or(before);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

fn deduce_filename_from_content(content: &str) -> Option<String> {
    // Look at first 2 lines for comment with filename
    for line in content.lines().take(2) {
        let line = line.trim();
        
        // Check for common comment patterns
        let patterns = [
            ("//", "// "),
            ("#", "# "),
            ("/*", "/* "),
            ("<!--", "<!-- "),
            (";", "; "),
            ("--", "-- "),
        ];
        
        for (comment_start, comment_prefix) in patterns {
            if line.starts_with(comment_start) {
                // Remove comment prefix and look for filename-like pattern
                let after_comment = line.strip_prefix(comment_start).unwrap_or(line).trim();
                let after_prefix = after_comment.strip_prefix(comment_prefix.trim()).unwrap_or(after_comment);
                
                // Check if it looks like a filename (contains dot, no spaces)
                if after_prefix.contains('.') && !after_prefix.contains(char::is_whitespace) {
                    return Some(after_prefix.to_string());
                }
                
                // Also check for any word that might be a filename
                let words: Vec<&str> = after_prefix.split_whitespace().collect();
                for word in words {
                    if word.contains('.') && !word.contains('/') && word.len() > 2 {
                        return Some(word.to_string());
                    }
                }
            }
        }
    }
    
    None
}

fn get_user_filename() -> Result<String> {
    use std::io::{self, Write};
    
    print!("filename: ");
    io::stdout().flush()?;
    
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    
    let filename = input.trim();
    if filename.is_empty() {
        bail!("no filename provided");
    }
    
    // Suggest adding extension if missing
    if !filename.contains('.') {
        print!("no extension detected. add .rs? (y/n): ");
        io::stdout().flush()?;
        let mut response = String::new();
        io::stdin().read_line(&mut response)?;
        if response.trim().to_lowercase() == "y" {
            return Ok(format!("{}.rs", filename));
        }
    }
    
    Ok(filename.to_string())
}

fn get_clipboard_text() -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let out = Command::new("wl-paste")
                .arg("--no-newline")
                .output()
                .context("failed to spawn wl-paste")?;
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).to_string());
            }
        }
    }
    let mut cb = Clipboard::new().context("clipboard init failed")?;
    cb.get_text().context("failed to read clipboard text")
}