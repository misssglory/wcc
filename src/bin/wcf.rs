use anyhow::{bail, Context, Result};
use quote::ToTokens;
use rayon::prelude::*;
use similar::{ChangeTag, TextDiff};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use syn::{
    parse_file, parse_str, Attribute, File, ImplItem, Item, ItemEnum, ItemFn, ItemStruct,
    Visibility,
};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use wcc::common::*;
use wcc::config::load_unified_config;
#[derive(Debug, Clone)]
enum CodeBlockType {
    Function,
    Struct,
    Enum,
}
#[derive(Debug, Clone)]
struct CodeBlockMatch {
    file_path: PathBuf,
    block_name: String,
    block_type: CodeBlockType,
    old_full_block: String,
    original_vis: Option<Visibility>,
    original_asyncness: Option<syn::token::Async>,
    original_unsafety: Option<syn::token::Unsafe>,
    original_attrs: Vec<Attribute>,
    line_range: Option<(usize, usize)>,
    context_label: Option<String>,
}
fn format_code_with_rustfmt(code: &str, file_path: Option<&Path>) -> Result<String> {
    let temp_dir = tempfile::TempDir::new()?;
    let temp_file = temp_dir.path().join("temp.rs");
    fs::write(&temp_file, code)?;
    let mut cmd = Command::new("rustfmt");
    cmd.arg("--edition").arg("2021");
    if let Some(path) = file_path {
        if let Some(parent) = path.parent() {
            cmd.current_dir(parent);
        }
    }
    cmd.arg(&temp_file);
    let output = cmd.output()?;
    if output.status.success() {
        Ok(fs::read_to_string(&temp_file)?)
    } else {
        Ok(code.to_string())
    }
}
fn format_code_with_lines(code: &str, file_path: Option<&Path>) -> Result<String> {
    let formatted = format_code_with_rustfmt(code, file_path)?;
    let lines: Vec<&str> = formatted.lines().collect();
    let numbered: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:4} {}", i + 1, line))
        .collect();
    Ok(numbered.join("\n"))
}
fn generate_formatted_diff(old: &str, new: &str, file_path: Option<&Path>) -> Result<String> {
    let old_formatted = format_code_with_rustfmt(old, file_path)?;
    let new_formatted = format_code_with_rustfmt(new, file_path)?;
    if old_formatted.trim() == new_formatted.trim() {
        return Ok(String::new());
    }
    let diff = TextDiff::from_lines(&old_formatted, &new_formatted);
    let mut result = String::new();
    for change in diff.iter_all_changes() {
        let line = change.as_str().unwrap_or("");
        match change.tag() {
            ChangeTag::Insert => {
                for line in line.lines() {
                    result.push_str(&format!("\x1b[32m+{}\x1b[0m\n", line));
                }
            }
            ChangeTag::Delete => {
                for line in line.lines() {
                    result.push_str(&format!("\x1b[31m-{}\x1b[0m\n", line));
                }
            }
            ChangeTag::Equal => {
                for line in line.lines() {
                    result.push_str(&format!(" {}\n", line));
                }
            }
        }
    }
    Ok(result)
}
fn print_colored_formatted_diff(old: &str, new: &str, file_path: Option<&Path>) -> Result<bool> {
    let diff = generate_formatted_diff(old, new, file_path)?;
    if diff.is_empty() {
        println!("  \x1b[33m⚠ No changes detected (code already matches)\x1b[0m");
        return Ok(false);
    }
    print!("{}", diff);
    Ok(true)
}
fn parse_clipboard_blocks(content: &str) -> Result<Vec<(String, CodeBlockType, String)>> {
    let content = content.trim();
    let mut blocks = Vec::new();
    match parse_file(content) {
        Ok(file) => {
            for item in file.items {
                match item {
                    Item::Fn(item_fn) => {
                        let name = item_fn.sig.ident.to_string();
                        let block_str = quote::quote!(# item_fn).to_string();
                        blocks.push((name, CodeBlockType::Function, block_str));
                    }
                    Item::Struct(item_struct) => {
                        let name = item_struct.ident.to_string();
                        let block_str = quote::quote!(# item_struct).to_string();
                        blocks.push((name, CodeBlockType::Struct, block_str));
                    }
                    Item::Enum(item_enum) => {
                        let name = item_enum.ident.to_string();
                        let block_str = quote::quote!(# item_enum).to_string();
                        blocks.push((name, CodeBlockType::Enum, block_str));
                    }
                    Item::Impl(item_impl) => {
                        for method in item_impl.items {
                            if let ImplItem::Fn(method_fn) = method {
                                let name = method_fn.sig.ident.to_string();
                                let block_str = quote::quote!(# method_fn).to_string();
                                blocks.push((name, CodeBlockType::Function, block_str));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(_) => {
            let mut pos = 0;
            while let Some(start_pos) = content[pos..].find("struct ") {
                let abs_start = pos + start_pos;
                let (end_pos, found) = find_block_end(content, abs_start);
                if !found {
                    break;
                }
                let full = content[abs_start..end_pos].to_string();
                if let Ok(item_struct) = parse_str::<ItemStruct>(&full) {
                    let name = item_struct.ident.to_string();
                    blocks.push((name, CodeBlockType::Struct, full));
                }
                pos = end_pos;
            }
            let mut pos = 0;
            while let Some(start_pos) = content[pos..].find("enum ") {
                let abs_start = pos + start_pos;
                let (end_pos, found) = find_block_end(content, abs_start);
                if !found {
                    break;
                }
                let full = content[abs_start..end_pos].to_string();
                if let Ok(item_enum) = parse_str::<ItemEnum>(&full) {
                    let name = item_enum.ident.to_string();
                    blocks.push((name, CodeBlockType::Enum, full));
                }
                pos = end_pos;
            }
            let mut pos = 0;
            while let Some(start_pos) = content[pos..].find("fn ") {
                let abs_start = pos + start_pos;
                let (end_pos, found) = find_block_end(content, abs_start);
                if !found {
                    break;
                }
                let full = content[abs_start..end_pos].to_string();
                if let Ok(item_fn) = parse_str::<ItemFn>(&full) {
                    let name = item_fn.sig.ident.to_string();
                    blocks.push((name, CodeBlockType::Function, full));
                }
                pos = end_pos;
            }
        }
    }
    if blocks.is_empty() {
        bail!("No valid functions, structs, or enums found in clipboard");
    }
    Ok(blocks)
}
fn find_block_end(content: &str, abs_start: usize) -> (usize, bool) {
    let mut brace_count = 0;
    let mut in_brace = false;
    let mut end_pos = abs_start;
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
                    return (end_pos, true);
                }
            }
            _ => {}
        }
    }
    (end_pos, false)
}
fn normalize_for_match(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
fn find_block_line_range(
    content: &str,
    block_str: &str,
    lines: &[&str],
) -> Result<Option<(usize, usize)>> {
    let normalized_target = block_str
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>();
    if normalized_target.is_empty() {
        return Ok(None);
    }
    let first_line = normalized_target[0];
    let second_line = normalized_target.get(1).copied();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != first_line {
            continue;
        }
        if let Some(expected_second) = second_line {
            let next_non_empty = lines
                .iter()
                .skip(i + 1)
                .map(|l| l.trim())
                .find(|l| !l.is_empty());
            if next_non_empty != Some(expected_second) {
                continue;
            }
        }
        let mut brace_count = 0;
        let mut saw_open = false;
        for (j, l) in lines.iter().enumerate().skip(i) {
            for ch in l.chars() {
                match ch {
                    '{' => {
                        brace_count += 1;
                        saw_open = true;
                    }
                    '}' => {
                        brace_count -= 1;
                        if saw_open && brace_count == 0 {
                            let candidate = lines[i..=j]
                                .iter()
                                .map(|s| s.trim())
                                .filter(|l| !l.is_empty())
                                .collect::<Vec<_>>();
                            if candidate == normalized_target {
                                return Ok(Some((i + 1, j + 1)));
                            }
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(byte_start) = content.find(block_str) {
        let start_line = content[..byte_start]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1;
        let line_count = block_str.lines().count().max(1);
        return Ok(Some((start_line, start_line + line_count - 1)));
    }
    Ok(None)
}
fn find_enum_in_file(file_path: &Path, enum_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let started = Instant::now();
    let content = fs::read_to_string(file_path)?;
    if !content.contains(enum_name) {
        return Ok(Vec::new());
    }
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    for item in file.items {
        if let Item::Enum(item_enum) = item {
            if item_enum.ident == enum_name {
                let enum_str = quote::quote!(# item_enum).to_string();
                let line_range = find_block_line_range(&content, &enum_str, &lines)?;
                matches.push(CodeBlockMatch {
                    file_path: file_path.to_path_buf(),
                    block_name: enum_name.to_string(),
                    block_type: CodeBlockType::Enum,
                    old_full_block: enum_str,
                    original_vis: Some(item_enum.vis),
                    original_asyncness: None,
                    original_unsafety: None,
                    original_attrs: item_enum.attrs,
                    line_range,
                    context_label: None,
                });
            }
        }
    }
    debug!(
        file = % file_path.display(), enum_name, elapsed_ms = started.elapsed()
        .as_millis(), match_count = matches.len(), "find_enum_in_file finished"
    );
    Ok(matches)
}
fn find_struct_in_file(file_path: &Path, struct_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let started = Instant::now();
    let content = fs::read_to_string(file_path)?;
    if !content.contains(struct_name) {
        return Ok(Vec::new());
    }
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    for item in file.items {
        if let Item::Struct(item_struct) = item {
            if item_struct.ident == struct_name {
                let struct_str = quote::quote!(# item_struct).to_string();
                let line_range = find_block_line_range(&content, &struct_str, &lines)?;
                matches.push(CodeBlockMatch {
                    file_path: file_path.to_path_buf(),
                    block_name: struct_name.to_string(),
                    block_type: CodeBlockType::Struct,
                    old_full_block: struct_str,
                    original_vis: Some(item_struct.vis),
                    original_asyncness: None,
                    original_unsafety: None,
                    original_attrs: item_struct.attrs,
                    line_range,
                    context_label: None,
                });
            }
        }
    }
    debug!(
        file = % file_path.display(), struct_name, elapsed_ms = started.elapsed()
        .as_millis(), match_count = matches.len(), "find_struct_in_file finished"
    );
    Ok(matches)
}
fn last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|seg| seg.ident.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
fn find_function_in_file(file_path: &Path, func_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let started = Instant::now();
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read {}", file_path.display()))?;
    if !content.contains(func_name) {
        debug!(
            file = % file_path.display(), func_name, elapsed_ms = started.elapsed()
            .as_millis(), "skip file early: function name not present in text"
        );
        return Ok(Vec::new());
    }
    let file =
        parse_file(&content).with_context(|| format!("Failed to parse {}", file_path.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let mut matches = Vec::new();
    for item in &file.items {
        match item {
            Item::Fn(item_fn) => {
                if item_fn.sig.ident == func_name {
                    let block_str = item_fn.to_token_stream().to_string();
                    let line_range = find_block_line_range(&content, &block_str, &lines)?;
                    matches.push(CodeBlockMatch {
                        file_path: file_path.to_path_buf(),
                        block_name: func_name.to_string(),
                        block_type: CodeBlockType::Function,
                        old_full_block: block_str,
                        original_vis: Some(item_fn.vis.clone()),
                        original_asyncness: item_fn.sig.asyncness,
                        original_unsafety: item_fn.sig.unsafety,
                        original_attrs: item_fn.attrs.clone(),
                        line_range,
                        context_label: None,
                    });
                }
            }
            Item::Impl(item_impl) => {
                let impl_type = if let syn::Type::Path(type_path) = &*item_impl.self_ty {
                    last_segment(&type_path.path)
                } else {
                    "unknown".to_string()
                };
                for impl_item in &item_impl.items {
                    if let ImplItem::Fn(method_fn) = impl_item {
                        if method_fn.sig.ident == func_name {
                            let block_str = method_fn.to_token_stream().to_string();
                            let line_range = find_block_line_range(&content, &block_str, &lines)?;
                            matches.push(CodeBlockMatch {
                                file_path: file_path.to_path_buf(),
                                block_name: func_name.to_string(),
                                block_type: CodeBlockType::Function,
                                old_full_block: block_str,
                                original_vis: Some(method_fn.vis.clone()),
                                original_asyncness: method_fn.sig.asyncness,
                                original_unsafety: method_fn.sig.unsafety,
                                original_attrs: method_fn.attrs.clone(),
                                line_range,
                                context_label: Some(format!("impl {}", impl_type)),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    debug!(
        file = % file_path.display(), func_name, total_ms = started.elapsed()
        .as_millis(), match_count = matches.len(), "find_function_in_file finished"
    );
    Ok(matches)
}
fn replace_block_by_line_range(
    content: &str,
    line_range: (usize, usize),
    new_block_str: &str,
    file_path: &Path,
) -> Result<String> {
    let (start_line, end_line) = line_range;
    let start = start_line.saturating_sub(1);
    let end = end_line.saturating_sub(1);
    let lines: Vec<&str> = content.lines().collect();
    if start >= lines.len() || end >= lines.len() || start > end {
        bail!(
            "Invalid line range {}-{} for {}",
            start_line,
            end_line,
            file_path.display()
        );
    }
    let formatted_new = format_code_with_rustfmt(new_block_str, Some(file_path))?;
    let formatted_new = formatted_new.trim_end_matches('\n');
    let mut out = Vec::new();
    out.extend_from_slice(&lines[..start]);
    out.push(formatted_new);
    out.extend_from_slice(&lines[end + 1..]);
    let mut result = out.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}
fn replace_function_in_file(
    content: &str,
    func_match: &CodeBlockMatch,
    new_block_str: &str,
) -> Result<String> {
    let formatted_new = format_code_with_rustfmt(new_block_str, Some(&func_match.file_path))?;
    let formatted_new = formatted_new.trim_end_matches('\n').to_string();
    if let Some((start_line, end_line)) = func_match.line_range {
        let start = start_line - 1;
        let end = end_line - 1;
        let lines: Vec<&str> = content.lines().collect();
        if start < lines.len() && end < lines.len() && start <= end {
            let mut new_lines = Vec::new();
            new_lines.extend_from_slice(&lines[..start]);
            new_lines.push(formatted_new.as_str());
            new_lines.extend_from_slice(&lines[end + 1..]);
            let mut result = new_lines.join("\n");
            if content.ends_with('\n') {
                result.push('\n');
            }
            return Ok(result);
        }
    }
    let old_block = func_match.old_full_block.trim();
    if !old_block.is_empty() {
        let exact_hits: Vec<usize> = content
            .match_indices(old_block)
            .map(|(idx, _)| idx)
            .collect();
        if exact_hits.len() == 1 {
            let start = exact_hits[0];
            let end = start + old_block.len();
            let mut result = String::with_capacity(content.len() + formatted_new.len());
            result.push_str(&content[..start]);
            result.push_str(&formatted_new);
            result.push_str(&content[end..]);
            return Ok(result);
        }
        let old_block_fmt =
            format_code_with_rustfmt(&func_match.old_full_block, Some(&func_match.file_path))?;
        let old_block_fmt = old_block_fmt.trim();
        if !old_block_fmt.is_empty() {
            let formatted_content = format_code_with_rustfmt(content, Some(&func_match.file_path))?;
            let fmt_hits: Vec<usize> = formatted_content
                .match_indices(old_block_fmt)
                .map(|(idx, _)| idx)
                .collect();
            if fmt_hits.len() == 1 {
                let start = fmt_hits[0];
                let end = start + old_block_fmt.len();
                let mut replaced =
                    String::with_capacity(formatted_content.len() + formatted_new.len());
                replaced.push_str(&formatted_content[..start]);
                replaced.push_str(&formatted_new);
                replaced.push_str(&formatted_content[end..]);
                return Ok(replaced);
            }
        }
    }
    if !func_match.block_name.contains(" (in impl ") {
        let mut file: File = parse_file(content)?;
        let new_item_fn: ItemFn = parse_str(new_block_str)?;
        let target_name = &func_match.block_name;
        let mut replaced = false;
        for item in &mut file.items {
            if let Item::Fn(item_fn) = item {
                if item_fn.sig.ident == target_name {
                    let mut final_attrs = func_match.original_attrs.clone();
                    for attr in &new_item_fn.attrs {
                        if !final_attrs.contains(attr) {
                            final_attrs.push(attr.clone());
                        }
                    }
                    let final_vis = func_match
                        .original_vis
                        .clone()
                        .unwrap_or_else(|| new_item_fn.vis.clone());
                    let final_asyncness =
                        func_match.original_asyncness.or(new_item_fn.sig.asyncness);
                    let final_unsafety = func_match.original_unsafety.or(new_item_fn.sig.unsafety);
                    let mut preserved_sig = new_item_fn.sig.clone();
                    preserved_sig.asyncness = final_asyncness;
                    preserved_sig.unsafety = final_unsafety;
                    *item_fn = ItemFn {
                        attrs: final_attrs,
                        vis: final_vis,
                        sig: preserved_sig,
                        block: new_item_fn.block.clone(),
                    };
                    replaced = true;
                    break;
                }
            }
        }
        if replaced {
            return Ok(prettyplease::unparse(&file));
        }
    }
    bail!(
        "Could not determine exact location for selected function '{}' in {}. line_range={:?}",
        func_match.block_name,
        func_match.file_path.display(),
        func_match.line_range
    )
}
fn replace_struct_in_file(
    content: &str,
    struct_match: &CodeBlockMatch,
    new_block_str: &str,
) -> Result<String> {
    let line_range = struct_match.line_range.with_context(|| {
        format!(
            "No exact line range found for struct '{}' in {}",
            struct_match.block_name,
            struct_match.file_path.display()
        )
    })?;
    replace_block_by_line_range(content, line_range, new_block_str, &struct_match.file_path)
}
fn replace_enum_in_file(
    content: &str,
    enum_match: &CodeBlockMatch,
    new_block_str: &str,
) -> Result<String> {
    let line_range = enum_match.line_range.with_context(|| {
        format!(
            "No exact line range found for enum '{}' in {}",
            enum_match.block_name,
            enum_match.file_path.display()
        )
    })?;
    replace_block_by_line_range(content, line_range, new_block_str, &enum_match.file_path)
}
fn run_rustfmt_in_dir(file_path: &Path) -> Result<()> {
    let output = Command::new("rustfmt").arg(file_path).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("  Warning: rustfmt had issues: {}", stderr);
        }
    }
    Ok(())
}
fn scan_directory_for_block(
    dir: &Path,
    block_name: &str,
    block_type: &CodeBlockType,
) -> Result<Vec<CodeBlockMatch>> {
    fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in
            fs::read_dir(dir).with_context(|| format!("Failed to read dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let skip_dirs = [
                    "target",
                    "node_modules",
                    ".git",
                    ".cargo",
                    ".idea",
                    ".vscode",
                    ".direnv",
                ];
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if skip_dirs.contains(&name_str.as_ref()) {
                        continue;
                    }
                }
                collect_rust_files(&path, out)?;
            } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
        Ok(())
    }
    let total_started = Instant::now();
    let mut files = Vec::new();
    collect_rust_files(dir, &mut files)?;
    debug!(
        dir = % dir.display(), block_name, block_type = ? block_type, file_count = files
        .len(), "collected rust files"
    );
    let scanned_files = AtomicUsize::new(0);
    let candidate_files: Vec<PathBuf> = files
        .par_iter()
        .filter_map(|path| {
            scanned_files.fetch_add(1, Ordering::Relaxed);
            let content = match fs::read_to_string(path) {
                Ok(content) => content,
                Err(err) => {
                    debug!(
                        file = % path.display(), error = % err,
                        "failed to read file during prefilter"
                    );
                    return None;
                }
            };
            let hit = match block_type {
                CodeBlockType::Function | CodeBlockType::Struct | CodeBlockType::Enum => {
                    content.contains(block_name)
                }
            };
            if hit {
                Some(path.clone())
            } else {
                None
            }
        })
        .collect();
    debug!(
        dir = % dir.display(), block_name, block_type = ? block_type, scanned_files =
        scanned_files.load(Ordering::Relaxed), candidate_files = candidate_files.len(),
        prefilter_ms = total_started.elapsed().as_millis(), "parallel prefilter finished"
    );
    let mut all_matches = Vec::new();
    for path in candidate_files {
        let started = Instant::now();
        let mut matches = match block_type {
            CodeBlockType::Function => find_function_in_file(&path, block_name)?,
            CodeBlockType::Struct => find_struct_in_file(&path, block_name)?,
            CodeBlockType::Enum => find_enum_in_file(&path, block_name)?,
        };
        if !matches.is_empty() {
            debug!(
                file = % path.display(), block_name, block_type = ? block_type,
                elapsed_ms = started.elapsed().as_millis(), match_count = matches.len(),
                "resolved matches in candidate file"
            );
        }
        all_matches.append(&mut matches);
    }
    info!(
        dir = % dir.display(), block_name, block_type = ? block_type, total_matches =
        all_matches.len(), elapsed_ms = total_started.elapsed().as_millis(),
        "scan_directory_for_block finished"
    );
    Ok(all_matches)
}
fn select_match_with_fzf(matches: &[CodeBlockMatch]) -> Result<Option<CodeBlockMatch>> {
    let fzf_check = Command::new("fzf").arg("--version").output();
    if fzf_check.is_err() {
        bail!("fzf not found");
    }
    if matches.is_empty() {
        return Ok(None);
    }
    fn compact_body_preview(block: &str) -> String {
        let body = block.split_once('{').map(|(_, rest)| rest).unwrap_or(block);
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed == "}" {
                continue;
            }
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }
            return trimmed.chars().take(120).collect();
        }
        String::new()
    }
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                if matches!(chars.peek(), Some('[')) {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }
    let block_type = match matches[0].block_type {
        CodeBlockType::Function => "fn",
        CodeBlockType::Struct => "struct",
        CodeBlockType::Enum => "enum",
    };
    let header_name = &matches[0].block_name;
    eprintln!(
        "\n📁 Multiple matches found for \x1b[1;36m{}\x1b[0m \x1b[1;33m{}\x1b[0m. Select one to modify:",
        block_type, header_name
    );
    let rendered_rows: Vec<(usize, String, String, String, String)> = matches
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let location = if let Some((start, end)) = m.line_range {
                format!("{}:{}-{}", relative_display_path(&m.file_path), start, end)
            } else {
                relative_display_path(&m.file_path)
            };
            let line_count = heatmap_lines(m.line_range);
            let outer = m
                .context_label
                .as_ref()
                .map(|s| ansi_cyan(s))
                .unwrap_or_default();
            let preview = compact_body_preview(&m.old_full_block);
            (idx, location, line_count, outer, preview)
        })
        .collect();
    let location_width = rendered_rows
        .iter()
        .map(|(_, location, _, _, _)| strip_ansi(location).chars().count())
        .max()
        .unwrap_or(0);
    let lines_width = rendered_rows
        .iter()
        .map(|(_, lines, _, _, _)| strip_ansi(lines).chars().count())
        .max()
        .unwrap_or(0);
    let outer_width = rendered_rows
        .iter()
        .map(|(_, _, _, outer, _)| strip_ansi(outer).chars().count())
        .max()
        .unwrap_or(0);
    let item_list = rendered_rows
        .into_iter()
        .map(|(idx, location, line_count, outer, preview)| {
            let location_pad = location_width.saturating_sub(strip_ansi(&location).chars().count());
            let lines_pad = lines_width.saturating_sub(strip_ansi(&line_count).chars().count());
            let outer_pad = outer_width.saturating_sub(strip_ansi(&outer).chars().count());
            format!(
                "{}\t{}{}  {}{}  {}{}  {}",
                idx,
                location,
                " ".repeat(location_pad),
                line_count,
                " ".repeat(lines_pad),
                outer,
                " ".repeat(outer_pad),
                preview
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut fzf_child = Command::new("fzf")
        .arg("--height")
        .arg("40%")
        .arg("--border")
        .arg("--layout=reverse")
        .arg("--no-multi")
        .arg("--ansi")
        .arg("--delimiter")
        .arg("\t")
        .arg("--with-nth")
        .arg("2..")
        .arg("--prompt")
        .arg("> ")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to spawn fzf")?;
    {
        let mut stdin = fzf_child.stdin.take().context("Failed to open fzf stdin")?;
        stdin.write_all(item_list.as_bytes())?;
    }
    let output = fzf_child
        .wait_with_output()
        .context("Failed to read fzf output")?;
    if !output.status.success() {
        return Ok(None);
    }
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        return Ok(None);
    }
    if let Some(idx_str) = selected.split('\t').next() {
        if let Ok(idx) = idx_str.trim().parse::<usize>() {
            if idx < matches.len() {
                return Ok(Some(matches[idx].clone()));
            }
        }
    }
    Ok(None)
}
fn main() -> Result<()> {
    init_tracing();
    let config = load_unified_config()?;
    let args: Vec<String> = env::args().skip(1).collect();
    let target_dir = if args.is_empty() {
        env::current_dir()?
    } else {
        PathBuf::from(&args[0])
    };
    let mut last_selected_match: Option<CodeBlockMatch> = None;
    eprintln!("📋 Reading clipboard content...");
    let clipboard_content = get_clipboard_text()?;
    eprintln!("🔍 Parsing code blocks from clipboard...");
    let mut blocks = match parse_clipboard_blocks(&clipboard_content) {
        Ok(blocks) => blocks,
        Err(e) => {
            eprintln!("⚠ Failed to parse clipboard as Rust code: {}", e);
            eprintln!(
                "Make sure the clipboard contains valid Rust function, struct, or enum definitions"
            );
            return Ok(());
        }
    };
    eprintln!("✓ Found {} code block(s):", blocks.len());
    for (name, block_type, _) in &blocks {
        let type_str = match block_type {
            CodeBlockType::Function => "function",
            CodeBlockType::Struct => "struct",
            CodeBlockType::Enum => "enum",
        };
        eprintln!("  • {} ({})", name, type_str);
    }
    let mut processed_count = 0;
    let mut skipped_count = 0;
    let mut processed_blocks: Vec<String> = Vec::new();
    let mut skipped_no_changes_blocks: Vec<String> = Vec::new();
    let mut not_found_blocks: Vec<String> = Vec::new();
    let mut manually_skipped_blocks: Vec<String> = Vec::new();
    while !blocks.is_empty() {
        let (block_name, block_type, new_block_str) = blocks.remove(0);
        let type_str = match block_type {
            CodeBlockType::Function => "function",
            CodeBlockType::Struct => "struct",
            CodeBlockType::Enum => "enum",
        };
        eprintln!("\n\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        eprintln!(
            "📝 Processing {}: \x1b[1;33m{}\x1b[0m",
            type_str, block_name
        );
        eprintln!("\x1b[1;36m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
        if config.wcf.show_buffer_preview {
            let formatted = format_code_with_lines(&new_block_str, None)?;
            println!("\n\x1b[1;36m📋 Code block from clipboard:\x1b[0m");
            println!("\x1b[90m{}\x1b[0m", "-".repeat(60));
            println!("{}", formatted);
            println!("\x1b[90m{}\x1b[0m", "-".repeat(60));
            print!("\n❓ Continue with this code block? (y/n/skip all): ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            match input.trim().to_lowercase().as_str() {
                "skip all" => {
                    eprintln!("⚠ Skipping all remaining blocks");
                    break;
                }
                "n" | "no" => {
                    eprintln!("⚠ Skipping {}: {}", type_str, block_name);
                    skipped_count += 1;
                    manually_skipped_blocks.push(format!("{} {}", type_str, block_name));
                    continue;
                }
                _ => {}
            }
        }
        eprintln!("\n🔎 Scanning for {} '{}'...", type_str, block_name);
        let scan_started = Instant::now();
        let mut matches = scan_directory_for_block(&target_dir, &block_name, &block_type)?;
        enrich_match_metadata_parallel(&mut matches)?;
        let scan_elapsed = scan_started.elapsed();
        let with_lines = matches.iter().filter(|m| m.line_range.is_some()).count();
        let with_context = matches.iter().filter(|m| m.context_label.is_some()).count();
        eprintln!(
            "✓ Scan complete: found {} matching block(s) in {:?} ({} with lines, {} with context)",
            matches.len(),
            scan_elapsed,
            with_lines,
            with_context
        );
        info!(
            block_name, block_type = ? block_type, match_count = matches.len(),
            with_lines, with_context, elapsed_ms = scan_elapsed.as_millis(),
            "block scan complete"
        );
        if let Some(ref last_match) = last_selected_match {
            if last_match.block_name == block_name {
                let file_matches: Vec<CodeBlockMatch> = matches
                    .iter()
                    .filter(|m| {
                        m.file_path == last_match.file_path && m.line_range == last_match.line_range
                    })
                    .cloned()
                    .collect();
                if !file_matches.is_empty() {
                    let range_str = last_match
                        .line_range
                        .map(|(s, e)| format!("{}-{}", s, e))
                        .unwrap_or_else(|| "unknown".to_string());
                    eprintln!(
                        "✓ Using previously selected match: {}:{}",
                        relative_display_path(&last_match.file_path),
                        range_str
                    );
                    matches = file_matches;
                } else {
                    eprintln!(
                        "ℹ Last selected match no longer matches '{}', prompting again",
                        block_name
                    );
                    last_selected_match = None;
                }
            } else {
                last_selected_match = None;
            }
        }
        if matches.is_empty() {
            eprintln!("⚠ No matches found for '{}', skipping", block_name);
            skipped_count += 1;
            not_found_blocks.push(format!("{} {}", type_str, block_name));
            last_selected_match = None;
            continue;
        }
        let selected_matches = if matches.len() > 1 {
            match select_match_with_fzf(&matches)? {
                Some(selected_match) => {
                    last_selected_match = Some(selected_match.clone());
                    vec![selected_match]
                }
                None => {
                    eprintln!("⚠ No match selected for '{}', skipping", block_name);
                    skipped_count += 1;
                    manually_skipped_blocks.push(format!("{} {}", type_str, block_name));
                    continue;
                }
            }
        } else {
            last_selected_match = Some(matches[0].clone());
            matches
        };
        let sample_file_path = selected_matches.first().map(|m| m.file_path.as_path());
        eprintln!("\n📝 Preview of changes:");
        let mut has_changes = false;
        for m in &selected_matches {
            let range_str = m
                .line_range
                .map(|(s, e)| format!("{}-{}", s, e))
                .unwrap_or_else(|| "unknown".to_string());
            let context_suffix = m
                .context_label
                .as_ref()
                .map(|ctx| format!(" {}", ansi_cyan(ctx)))
                .unwrap_or_default();
            eprintln!(
                "\n  File: {} (lines {}){}",
                relative_display_path(&m.file_path),
                range_str,
                context_suffix
            );
            eprintln!("\n  \x1b[1;33mDiff (formatted with rustfmt):\x1b[0m");
            println!("\x1b[90m{}\x1b[0m", "-".repeat(60));
            let changed =
                print_colored_formatted_diff(&m.old_full_block, &new_block_str, sample_file_path)?;
            println!("\x1b[90m{}\x1b[0m", "-".repeat(60));
            if changed {
                has_changes = true;
            }
        }
        if !has_changes {
            eprintln!(
                "\n\x1b[33m⚠ No changes detected for '{}', auto-skipping\x1b[0m",
                block_name
            );
            skipped_count += 1;
            skipped_no_changes_blocks.push(format!("{} {}", type_str, block_name));
            continue;
        }
        print!("\n❓ Apply these changes? (y/n): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            eprintln!("⚠ Skipping {}: {}", type_str, block_name);
            skipped_count += 1;
            manually_skipped_blocks.push(format!("{} {}", type_str, block_name));
            last_selected_match = None;
            continue;
        }
        eprintln!("\n🔄 Applying changes...");
        for block_match in &selected_matches {
            let file_path = &block_match.file_path;
            let old_content = fs::read_to_string(file_path)?;
            let backup_path = PathBuf::from(format!("{}.bkp", file_path.display()));
            if !backup_path.exists() {
                fs::write(&backup_path, &old_content)?;
                eprintln!("  ✓ Created backup: {}", backup_path.display());
            }
            let new_content = match block_type {
                CodeBlockType::Function => {
                    replace_function_in_file(&old_content, block_match, &new_block_str)?
                }
                CodeBlockType::Struct => {
                    replace_struct_in_file(&old_content, block_match, &new_block_str)?
                }
                CodeBlockType::Enum => {
                    replace_enum_in_file(&old_content, block_match, &new_block_str)?
                }
            };
            fs::write(file_path, &new_content)?;
            if let Err(e) = run_rustfmt_in_dir(file_path) {
                eprintln!("  Warning: rustfmt failed: {}", e);
            }
            let range_str = block_match
                .line_range
                .map(|(s, e)| format!("{}-{}", s, e))
                .unwrap_or_else(|| "unknown".to_string());
            eprintln!(
                "  ✓ Updated: {} (lines {})",
                relative_display_path(file_path),
                range_str
            );
            processed_count += 1;
        }
        eprintln!("\n✅ {} '{}' processed successfully!", type_str, block_name);
        processed_blocks.push(format!("{} {}", type_str, block_name));
    }
    eprintln!("\n\x1b[1;32m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    eprintln!("\x1b[1;32m✅ All code blocks processed!\x1b[0m");
    eprintln!("  Blocks processed: {}", processed_count);
    eprintln!("  Blocks skipped: {}", skipped_count);
    eprintln!("\n\x1b[1;36mProcessed blocks:\x1b[0m");
    if processed_blocks.is_empty() {
        eprintln!("  • none");
    } else {
        for block in &processed_blocks {
            eprintln!("  • {}", block);
        }
    }
    eprintln!("\n\x1b[1;33mSkipped (no changes detected):\x1b[0m");
    if skipped_no_changes_blocks.is_empty() {
        eprintln!("  • none");
    } else {
        for block in &skipped_no_changes_blocks {
            eprintln!("  • {}", block);
        }
    }
    eprintln!("\n\x1b[1;33mSkipped (user decision / no file selected):\x1b[0m");
    if manually_skipped_blocks.is_empty() {
        eprintln!("  • none");
    } else {
        for block in &manually_skipped_blocks {
            eprintln!("  • {}", block);
        }
    }
    eprintln!("\n\x1b[1;31mNot found:\x1b[0m");
    if not_found_blocks.is_empty() {
        eprintln!("  • none");
    } else {
        for block in &not_found_blocks {
            eprintln!("  • {}", block);
        }
    }
    Ok(())
}
fn relative_display_path(path: &Path) -> String {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let rel = path.strip_prefix(&cwd).unwrap_or(path);
    let parent = rel.parent().filter(|p| !p.as_os_str().is_empty());
    let filename = rel
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.display().to_string());
    match parent {
        Some(parent) => format!("{}/{}", parent.display(), color_filename(&filename)),
        None => color_filename(&filename),
    }
}
fn ansi_dim(s: impl AsRef<str>) -> String {
    format!("\x1b[90m{}\x1b[0m", s.as_ref())
}
fn ansi_cyan(s: impl AsRef<str>) -> String {
    format!("\x1b[36m{}\x1b[0m", s.as_ref())
}
fn ansi_green(s: impl AsRef<str>) -> String {
    format!("\x1b[32m{}\x1b[0m", s.as_ref())
}
fn ansi_yellow(s: impl AsRef<str>) -> String {
    format!("\x1b[33m{}\x1b[0m", s.as_ref())
}
fn ansi_red(s: impl AsRef<str>) -> String {
    format!("\x1b[31m{}\x1b[0m", s.as_ref())
}
fn block_line_count(range: Option<(usize, usize)>) -> Option<usize> {
    range.map(|(start, end)| end.saturating_sub(start) + 1)
}
fn heatmap_lines(range: Option<(usize, usize)>) -> String {
    match block_line_count(range) {
        Some(n) => format!("{}L", heatmap_color(n, 0, 80)),
        None => ansi_dim("?L"),
    }
}
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,wcf=debug")),
        )
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true)
        .try_init();
}
fn find_block_metadata_in_content(
    content: &str,
    block_name: &str,
    block_type: &CodeBlockType,
    old_full_block: &str,
    existing_context: Option<&str>,
) -> (Option<(usize, usize)>, Option<String>) {
    let lines: Vec<&str> = content.lines().collect();
    let mut candidate_headers = Vec::new();
    match block_type {
        CodeBlockType::Function => {
            candidate_headers.push(format!("fn {}", block_name));
            candidate_headers.push(format!("async fn {}", block_name));
            candidate_headers.push(format!("pub fn {}", block_name));
            candidate_headers.push(format!("pub async fn {}", block_name));
            candidate_headers.push(format!("pub(crate) fn {}", block_name));
            candidate_headers.push(format!("pub(crate) async fn {}", block_name));
        }
        CodeBlockType::Struct => {
            candidate_headers.push(format!("struct {}", block_name));
            candidate_headers.push(format!("pub struct {}", block_name));
            candidate_headers.push(format!("pub(crate) struct {}", block_name));
        }
        CodeBlockType::Enum => {
            candidate_headers.push(format!("enum {}", block_name));
            candidate_headers.push(format!("pub enum {}", block_name));
            candidate_headers.push(format!("pub(crate) enum {}", block_name));
        }
    }
    let mut hits: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !candidate_headers.iter().any(|h| trimmed.contains(h)) {
            continue;
        }
        let mut start = i;
        while start > 0 {
            let prev = lines[start - 1].trim();
            if prev.starts_with("#[")
                || prev.starts_with("///")
                || prev.starts_with("//!")
                || prev.is_empty()
            {
                start -= 1;
            } else {
                break;
            }
        }
        let mut brace_depth = 0i32;
        let mut saw_open = false;
        let mut end = None;
        for (j, l) in lines.iter().enumerate().skip(i) {
            for ch in l.chars() {
                match ch {
                    '{' => {
                        brace_depth += 1;
                        saw_open = true;
                    }
                    '}' => {
                        brace_depth -= 1;
                        if saw_open && brace_depth == 0 {
                            end = Some(j);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if end.is_some() {
                break;
            }
        }
        if let Some(end_idx) = end {
            hits.push((start + 1, end_idx + 1));
        }
    }
    let selected_range = if hits.len() == 1 {
        Some(hits[0])
    } else if !old_full_block.trim().is_empty() {
        find_block_line_range(content, old_full_block, &lines)
            .ok()
            .flatten()
    } else {
        None
    };
    let context_label = if existing_context.is_some() {
        existing_context.map(|s| s.to_string())
    } else if let Some((start_line, _)) = selected_range {
        infer_outer_context_label(content, start_line)
    } else {
        None
    };
    (selected_range, context_label)
}
fn infer_outer_context_label(content: &str, start_line: usize) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    if start_line == 0 || start_line > lines.len() {
        return None;
    }
    for i in (0..start_line.saturating_sub(1)).rev() {
        let trimmed = lines[i].trim();
        if let Some(rest) = trimmed.strip_prefix("impl ") {
            let name = rest
                .split('{')
                .next()
                .unwrap_or(rest)
                .trim()
                .replace(" where ", " ");
            return Some(format!("impl {}", name));
        }
        if let Some(rest) = trimmed.strip_prefix("trait ") {
            let name = rest
                .split('{')
                .next()
                .unwrap_or(rest)
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("unknown");
            return Some(format!("trait {}", name));
        }
        if let Some(rest) = trimmed.strip_prefix("mod ") {
            let name = rest
                .split('{')
                .next()
                .unwrap_or(rest)
                .trim()
                .trim_end_matches(';')
                .trim();
            return Some(format!("mod {}", name));
        }
    }
    None
}
fn enrich_match_metadata_parallel(matches: &mut [CodeBlockMatch]) -> Result<()> {
    if matches.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let mut grouped: HashMap<PathBuf, Vec<(usize, String, CodeBlockType, String, Option<String>)>> =
        HashMap::new();
    for (idx, m) in matches.iter().enumerate() {
        grouped.entry(m.file_path.clone()).or_default().push((
            idx,
            m.block_name.clone(),
            m.block_type.clone(),
            m.old_full_block.clone(),
            m.context_label.clone(),
        ));
    }
    let total_files = grouped.len();
    let total_matches = matches.len();
    debug!(
        total_files,
        total_matches, "starting parallel metadata enrichment"
    );
    let processed = AtomicUsize::new(0);
    let grouped_vec: Vec<(
        PathBuf,
        Vec<(usize, String, CodeBlockType, String, Option<String>)>,
    )> = grouped.into_iter().collect();
    let results: Vec<Result<Vec<(usize, Option<(usize, usize)>, Option<String>)>>> = grouped_vec
        .par_iter()
        .map(|(path, entries)| {
            let file_started = Instant::now();
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let mut out = Vec::with_capacity(entries.len());
            for (idx, block_name, block_type, old_full_block, existing_context) in entries {
                let (line_range, context_label) = find_block_metadata_in_content(
                    &content,
                    block_name,
                    block_type,
                    old_full_block,
                    existing_context.as_deref(),
                );
                let now = processed.fetch_add(1, Ordering::Relaxed) + 1;
                if now % 100 == 0 || now == total_matches {
                    debug!(
                        processed = now,
                        total = total_matches,
                        "metadata enrichment progress"
                    );
                }
                out.push((*idx, line_range, context_label));
            }
            debug!(
                file = % path.display(), entry_count = entries.len(), elapsed_ms =
                file_started.elapsed().as_millis(),
                "metadata enrichment finished for file"
            );
            Ok(out)
        })
        .collect();
    let mut missing_lines = 0usize;
    let mut missing_context = 0usize;
    for result in results {
        for (idx, line_range, context_label) in result? {
            matches[idx].line_range = line_range;
            if matches[idx].context_label.is_none() {
                matches[idx].context_label = context_label;
            }
            if matches[idx].line_range.is_none() {
                missing_lines += 1;
                warn!(
                    file = % matches[idx].file_path.display(), block_name = %
                    matches[idx].block_name, block_type = ? matches[idx].block_type,
                    "line_range still missing after enrichment"
                );
            }
            if matches[idx].context_label.is_none() {
                missing_context += 1;
            }
        }
    }
    info!(
        total_matches,
        missing_lines,
        missing_context,
        elapsed_ms = started.elapsed().as_millis(),
        "metadata enrichment complete"
    );
    Ok(())
}
