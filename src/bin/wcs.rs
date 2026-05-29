use anyhow::{Context, Result};
use clap::Parser;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use wcc::config::{load_unified_config, UnifiedConfig};

/// Recursively replace strings in source files within a directory
#[derive(Parser)]
#[command(name = "wcs")]
#[command(about = "Replace strings in source files recursively", version = "1.0")]
struct Args {
    /// String to replace
    old_string: String,
    
    /// New string to replace with
    new_string: String,
    
    /// Directory to search (default: current directory)
    #[arg(default_value = ".")]
    directory: PathBuf,
    
    /// File pattern to match (overrides config source_extensions)
    #[arg(short, long)]
    pattern: Option<String>,
    
    /// Create backup files (overrides config auto_backup)
    #[arg(short, long)]
    backup: Option<bool>,
    
    /// Dry run - show what would be changed without modifying files
    #[arg(short, long)]
    dry_run: Option<bool>,
    
    /// Case insensitive search (overrides config case_sensitive)
    #[arg(short, long)]
    ignore_case: Option<bool>,
    
    /// Show line details for each change
    #[arg(short, long)]
    verbose: Option<bool>,
}

#[derive(Debug, Clone)]
struct FileChange {
    path: PathBuf,
    relative_path: String,
    line_count: usize,
    replacement_count: usize,
    line_details: Vec<LineDetail>,
}

#[derive(Debug, Clone)]
struct LineDetail {
    line_num: usize,
    old_line: String,
    new_line: String,
    occurrences: usize,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_unified_config()?;
    
    // Check if wrs is enabled
    if !config.wrs.enabled {
        eprintln!("\x1b[33m⚠ wrs (string replacement) is disabled in config\x1b[0m");
        eprintln!("  Enable it by setting `enabled = true` in [wrs] section");
        return Ok(());
    }
    
    // Validate directory
    if !args.directory.exists() {
        eprintln!("\x1b[31mError: Directory '{}' does not exist\x1b[0m", args.directory.display());
        std::process::exit(1);
    }
    
    // Merge CLI args with config
    let auto_backup = args.backup.unwrap_or(config.wrs.auto_backup);
    let dry_run = args.dry_run.unwrap_or(config.wrs.dry_run_by_default);
    let case_sensitive = !args.ignore_case.unwrap_or(!config.wrs.case_sensitive);
    let show_line_details = args.verbose.unwrap_or(config.wrs.show_line_details);
    
    // Build walker
    let mut walker = if config.wrs.follow_symlinks {
        WalkDir::new(&args.directory).follow_links(true)
    } else {
        WalkDir::new(&args.directory).follow_links(false)
    };
    
    let walker_iter = walker.into_iter();
    
    let mut changes = Vec::new();
    let mut total_replacements = 0;
    let mut total_files = 0;
    let mut skipped_files = 0;
    
    println!("\x1b[36m🔍 Searching for '{}' → '{}'\x1b[0m", args.old_string, args.new_string);
    println!("📁 Directory: {}", args.directory.display());
    if dry_run {
        println!("\x1b[33m🔍 DRY RUN MODE - No files will be modified\x1b[0m");
    }
    println!();
    
    // Process each file
    for entry in walker_iter.filter_entry(|e| should_include(e.path(), &config, &args)) {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("\x1b[31mError reading entry: {}\x1b[0m", e);
                continue;
            }
        };
        
        let path = entry.path();
        
        // Skip directories
        if !path.is_file() {
            continue;
        }
        
        // Check if we should process this file based on extension
        if !should_process_file(path, &config, &args) {
            skipped_files += 1;
            continue;
        }
        
        // Check file size
        if let Ok(metadata) = fs::metadata(path) {
            let size_mb = metadata.len() as f64 / 1024.0 / 1024.0;
            if metadata.len() > config.wrs.max_file_size_mb * 1024 * 1024 {
                eprintln!("\x1b[33m⏭ Skipping large file ({}MB > {}MB): {}\x1b[0m", 
                         size_mb as u64, config.wrs.max_file_size_mb, path.display());
                skipped_files += 1;
                continue;
            }
        }
        
        // Process the file
        match process_file(path, &args, &config, case_sensitive, show_line_details) {
            Ok(Some(change)) => {
                if change.replacement_count > 0 {
                    total_replacements += change.replacement_count;
                    total_files += 1;
                    changes.push(change);
                }
            }
            Ok(None) => {} // No changes made
            Err(e) => {
                eprintln!("\x1b[31mError processing {}: {}\x1b[0m", path.display(), e);
                skipped_files += 1;
            }
        }
    }
    
    // Write changes if not dry run
    if !dry_run {
        for change in &changes {
            if let Err(e) = write_file(&change, auto_backup, &config) {
                eprintln!("\x1b[31mError writing {}: {}\x1b[0m", change.path.display(), e);
            }
        }
    }
    
    // Print summary
    print_summary(&changes, total_replacements, total_files, skipped_files, dry_run, auto_backup);
    
    Ok(())
}

fn should_include(path: &Path, config: &UnifiedConfig, args: &Args) -> bool {
    let path_str = path.to_string_lossy();
    
    // Check exclude directories
    for exclude_dir in &config.wrs.exclude_dirs {
        if path_str.contains(exclude_dir) {
            return false;
        }
    }
    
    // Check exclude patterns
    for pattern in &config.wrs.exclude_patterns {
        if let Some(ext) = path.extension() {
            let pattern_clean = pattern.trim_start_matches('*');
            if ext.to_string_lossy().ends_with(pattern_clean) {
                return false;
            }
        }
    }
    
    true
}

fn should_process_file(path: &Path, config: &UnifiedConfig, args: &Args) -> bool {
    // If pattern is provided via CLI, use that instead of config
    if let Some(pattern) = &args.pattern {
        if let Some(ext) = path.extension() {
            let pattern_clean = pattern.trim_start_matches('*');
            if ext.to_string_lossy().ends_with(pattern_clean) {
                return true;
            }
        }
        return false;
    }
    
    // Otherwise use configured source extensions
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy().to_lowercase();
        return config.wrs.source_extensions.iter().any(|e| e == &ext_str);
    }
    
    // Files without extension (like Dockerfile, Makefile)
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    config.wrs.source_extensions.iter().any(|e| {
        file_name.to_lowercase() == e.to_lowercase() || 
        format!(".{}", file_name.to_lowercase()) == e.to_lowercase()
    })
}

fn process_file(
    path: &Path, 
    args: &Args, 
    config: &UnifiedConfig,
    case_sensitive: bool,
    show_line_details: bool,
) -> Result<Option<FileChange>> {
    // Read file
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<io::Result<_>>()?;
    
    let mut modified_lines = Vec::new();
    let mut replacement_count = 0;
    let mut affected_lines = 0;
    let mut line_details = Vec::new();
    
    // Process each line
    for (line_num, line) in lines.iter().enumerate() {
        let new_line = if case_sensitive {
            line.replace(&args.old_string, &args.new_string)
        } else {
            replace_ignore_case(line, &args.old_string, &args.new_string)
        };
        
        if new_line != *line {
            affected_lines += 1;
            let occurrences = if case_sensitive {
                line.matches(&args.old_string).count()
            } else {
                count_occurrences_ignore_case(line, &args.old_string)
            };
            replacement_count += occurrences;
            
            if show_line_details {
                line_details.push(LineDetail {
                    line_num: line_num + 1,
                    old_line: line.clone(),
                    new_line: new_line.clone(),
                    occurrences,
                });
            }
            
            modified_lines.push(new_line);
        } else {
            modified_lines.push(line.clone());
        }
    }
    
    if replacement_count > 0 {
        println!("\x1b[32m📝 {}\x1b[0m", path.display());
        println!("   \x1b[36m{} replacements in {} lines\x1b[0m", replacement_count, affected_lines);
        
        if show_line_details {
            for detail in &line_details {
                println!("     \x1b[33mLine {}: {} replacement(s)\x1b[0m", detail.line_num, detail.occurrences);
                if detail.occurrences <= 3 { // Only show preview for few changes
                    println!("       \x1b[90m- {}\x1b[0m", detail.old_line.trim());
                    println!("       \x1b[90m+ {}\x1b[0m", detail.new_line.trim());
                }
            }
        }
        
        let relative_path = path.strip_prefix(".").unwrap_or(path);
        Ok(Some(FileChange {
            path: path.to_path_buf(),
            relative_path: relative_path.display().to_string(),
            line_count: affected_lines,
            replacement_count,
            line_details,
        }))
    } else {
        Ok(None)
    }
}

fn replace_ignore_case(text: &str, from: &str, to: &str) -> String {
    let mut result = String::new();
    let mut i = 0;
    let text_lower = text.to_lowercase();
    let from_lower = from.to_lowercase();
    
    while i <= text.len() {
        if i + from.len() <= text.len() && 
           &text_lower[i..i + from.len()] == from_lower {
            result.push_str(to);
            i += from.len();
        } else if i < text.len() {
            result.push(text.chars().nth(i).unwrap());
            i += 1;
        } else {
            break;
        }
    }
    
    result
}

fn count_occurrences_ignore_case(text: &str, pattern: &str) -> usize {
    let text_lower = text.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    text_lower.matches(&pattern_lower).count()
}

fn write_file(change: &FileChange, auto_backup: bool, config: &UnifiedConfig) -> Result<()> {
    // Create backup if requested
    if auto_backup {
        let backup_path = change.path.with_extension(format!(
            "{}.bak",
            change.path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
        ));
        fs::copy(&change.path, &backup_path)?;
        println!("   \x1b[90m📋 Backup created: {}\x1b[0m", backup_path.file_name().unwrap_or_default().to_string_lossy());
    }
    
    // Read original file to get lines again
    let file = fs::File::open(&change.path)?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().collect::<io::Result<_>>()?;
    
    // Apply replacements
    let case_sensitive = config.wrs.case_sensitive;
    let old_string = std::env::args().nth(1).unwrap_or_default();
    let new_string = std::env::args().nth(2).unwrap_or_default();
    
    let modified_lines: Vec<String> = lines
        .iter()
        .map(|line| {
            if case_sensitive {
                line.replace(&old_string, &new_string)
            } else {
                replace_ignore_case(line, &old_string, &new_string)
            }
        })
        .collect();
    
    // Write modified content
    let mut file = fs::File::create(&change.path)?;
    for line in modified_lines {
        writeln!(file, "{}", line)?;
    }
    
    // Preserve timestamps if configured
    if config.wrs.preserve_timestamps {
        if let Ok(metadata) = fs::metadata(&change.path) {
            if let Ok(mtime) = metadata.modified() {
                let _ = filetime::set_file_mtime(&change.path, filetime::FileTime::from_system_time(mtime));
            }
        }
    }
    
    Ok(())
}

fn print_summary(
    changes: &[FileChange], 
    total_replacements: usize, 
    total_files: usize,
    skipped_files: usize,
    dry_run: bool,
    auto_backup: bool,
) {
    println!();
    println!("\x1b[36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("\x1b[1m📊 Summary\x1b[0m");
    println!("\x1b[36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    println!("  \x1b[32mFiles modified:\x1b[0m {}", total_files);
    println!("  \x1b[32mTotal replacements:\x1b[0m {}", total_replacements);
    println!("  \x1b[33mSkipped files:\x1b[0m {}", skipped_files);
    
    if dry_run {
        println!("\n\x1b[33m⚠ DRY RUN - No files were actually modified\x1b[0m");
        if !changes.is_empty() {
            println!("\n\x1b[36mFiles that would be modified:\x1b[0m");
            for change in changes {
                println!("  📄 {} ({} replacements)", change.relative_path, change.replacement_count);
            }
        }
    } else if !changes.is_empty() {
        println!("\n\x1b[36mModified files:\x1b[0m");
        for change in changes {
            println!("  ✓ {} ({} replacements)", change.relative_path, change.replacement_count);
        }
        
        if auto_backup {
            println!("\n\x1b[90m💾 Backups created with .bak extension\x1b[0m");
        }
    } else {
        println!("\n\x1b[33m✅ No matches found\x1b[0m");
    }
    
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_replace_ignore_case() {
        assert_eq!(replace_ignore_case("Hello World", "hello", "Hi"), "Hi World");
        assert_eq!(replace_ignore_case("HELLO WORLD", "hello", "Hi"), "Hi WORLD");
        assert_eq!(replace_ignore_case("hello hello", "hello", "hi"), "hi hi");
    }

    #[test]
    fn test_count_occurrences_ignore_case() {
        assert_eq!(count_occurrences_ignore_case("Hello hello HELLO", "hello"), 3);
        assert_eq!(count_occurrences_ignore_case("No matches here", "foo"), 0);
    }

    #[test]
    fn test_should_process_file() {
        let config = UnifiedConfig::default();
        let args = Args {
            old_string: "foo".to_string(),
            new_string: "bar".to_string(),
            directory: PathBuf::from("."),
            pattern: None,
            backup: None,
            dry_run: None,
            ignore_case: None,
            verbose: None,
        };
        
        assert!(should_process_file(Path::new("test.rs"), &config, &args));
        assert!(should_process_file(Path::new("test.go"), &config, &args));
        assert!(should_process_file(Path::new("Dockerfile"), &config, &args));
        assert!(!should_process_file(Path::new("test.exe"), &config, &args));
        assert!(!should_process_file(Path::new("test.png"), &config, &args));
    }
}