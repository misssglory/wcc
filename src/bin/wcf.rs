use anyhow::{bail, Context, Result};
use similar::{ChangeTag, TextDiff};
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
    let content = fs::read_to_string(file_path)?;
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
    Ok(matches)
}
fn find_struct_in_file(file_path: &Path, struct_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let content = fs::read_to_string(file_path)?;
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
    Ok(matches)
}
fn last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|seg| seg.ident.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
fn find_function_in_file(file_path: &Path, func_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let content = fs::read_to_string(file_path)?;
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    fn byte_offset_to_line(content: &str, byte_offset: usize) -> usize {
        content[..byte_offset]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1
    }
    fn enclosing_impl_type(file: &File, line_no: usize) -> Option<String> {
        for item in &file.items {
            if let Item::Impl(item_impl) = item {
                for method in &item_impl.items {
                    if let ImplItem::Fn(method_fn) = method {
                        let method_str = quote::quote!(# method_fn).to_string();
                        let method_line_count = method_str.lines().count().max(1);
                        for start_guess in 1..=line_no {
                            let end_guess = start_guess + method_line_count - 1;
                            if line_no >= start_guess && line_no <= end_guess {
                                let impl_type =
                                    if let syn::Type::Path(type_path) = &*item_impl.self_ty {
                                        last_segment(&type_path.path)
                                    } else {
                                        "unknown".to_string()
                                    };
                                return Some(impl_type);
                            }
                        }
                    }
                }
            }
        }
        None
    }
    let needle = format!("fn {}", func_name);
    let mut search_from = 0usize;
    let bytes = content.as_bytes();
    while search_from < content.len() {
        let Some(rel_pos) = content[search_from..].find(&needle) else {
            break;
        };
        let fn_pos = search_from + rel_pos;
        let ident_ok_before = fn_pos == 0
            || !content[..fn_pos]
                .chars()
                .last()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
        if !ident_ok_before {
            search_from = fn_pos + needle.len();
            continue;
        }
        let mut sig_start = fn_pos;
        while sig_start > 0 {
            let prev_nl = content[..sig_start].rfind('\n').map(|p| p + 1).unwrap_or(0);
            let line = &content[prev_nl..sig_start];
            let trimmed = line.trim();
            if trimmed.starts_with("#[")
                || trimmed.starts_with("///")
                || trimmed.starts_with("//!")
                || trimmed.is_empty()
            {
                sig_start = prev_nl;
                if prev_nl == 0 {
                    break;
                }
                continue;
            }
            break;
        }
        let mut brace_start = None;
        let mut i = fn_pos;
        let mut paren_depth = 0i32;
        let mut bracket_depth = 0i32;
        let mut angle_depth = 0i32;
        while i < content.len() {
            let ch = bytes[i] as char;
            match ch {
                '(' => paren_depth += 1,
                ')' => paren_depth -= 1,
                '[' => bracket_depth += 1,
                ']' => bracket_depth -= 1,
                '<' => angle_depth += 1,
                '>' => {
                    if angle_depth > 0 {
                        angle_depth -= 1;
                    }
                }
                '{' if paren_depth == 0 && bracket_depth == 0 => {
                    brace_start = Some(i);
                    break;
                }
                ';' if paren_depth == 0 && bracket_depth == 0 => {
                    brace_start = None;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let Some(open_brace) = brace_start else {
            search_from = fn_pos + needle.len();
            continue;
        };
        let mut depth = 0i32;
        let mut block_end = None;
        let mut j = open_brace;
        while j < content.len() {
            let ch = bytes[j] as char;
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = Some(j + 1);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let Some(end_pos) = block_end else {
            search_from = fn_pos + needle.len();
            continue;
        };
        let block_text = content[sig_start..end_pos].to_string();
        let start_line = byte_offset_to_line(&content, sig_start);
        let end_line = byte_offset_to_line(&content, end_pos.saturating_sub(1));
        let parsed_top = parse_str::<ItemFn>(&block_text).ok();
        let parsed_impl = parse_str::<syn::ImplItemFn>(&block_text).ok();
        if let Some(item_fn) = parsed_top {
            if item_fn.sig.ident == func_name {
                matches.push(CodeBlockMatch {
                    file_path: file_path.to_path_buf(),
                    block_name: func_name.to_string(),
                    block_type: CodeBlockType::Function,
                    old_full_block: block_text,
                    original_vis: Some(item_fn.vis),
                    original_asyncness: item_fn.sig.asyncness,
                    original_unsafety: item_fn.sig.unsafety,
                    original_attrs: item_fn.attrs,
                    line_range: Some((start_line, end_line)),
                    context_label: None,
                });
                search_from = end_pos;
                continue;
            }
        }
        if let Some(method_fn) = parsed_impl {
            if method_fn.sig.ident == func_name {
                let impl_type =
                    enclosing_impl_type(&file, start_line).unwrap_or_else(|| "unknown".to_string());
                matches.push(CodeBlockMatch {
                    file_path: file_path.to_path_buf(),
                    block_name: func_name.to_string(),
                    block_type: CodeBlockType::Function,
                    old_full_block: block_text,
                    original_vis: Some(method_fn.vis),
                    original_asyncness: method_fn.sig.asyncness,
                    original_unsafety: method_fn.sig.unsafety,
                    original_attrs: method_fn.attrs,
                    line_range: Some((start_line, end_line)),
                    context_label: Some(format!("impl {}", impl_type)),
                });
            }
        }
        search_from = end_pos;
    }
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
    let mut all_matches = Vec::new();
    fn walk_dir(
        dir: &Path,
        block_name: &str,
        block_type: &CodeBlockType,
        all_matches: &mut Vec<CodeBlockMatch>,
    ) -> Result<()> {
        for entry in fs::read_dir(dir)? {
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
                ];
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if skip_dirs.contains(&name_str.as_ref()) {
                        continue;
                    }
                }
                walk_dir(&path, block_name, block_type, all_matches)?;
            } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                match block_type {
                    CodeBlockType::Function => {
                        if let Ok(matches) = find_function_in_file(&path, block_name) {
                            all_matches.extend(matches);
                        }
                    }
                    CodeBlockType::Struct => {
                        if let Ok(matches) = find_struct_in_file(&path, block_name) {
                            all_matches.extend(matches);
                        }
                    }
                    CodeBlockType::Enum => {
                        if let Ok(matches) = find_enum_in_file(&path, block_name) {
                            all_matches.extend(matches);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    walk_dir(dir, block_name, block_type, &mut all_matches)?;
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
    let rendered_rows: Vec<(usize, String)> = matches
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let rel_path = relative_display_path(&m.file_path);
            let location = if let Some((start, end)) = m.line_range {
                format!("{}:{}-{}", rel_path, start, end)
            } else {
                rel_path
            };
            let preview = compact_body_preview(&m.old_full_block);
            let row = if let Some(ctx) = &m.context_label {
                format!("{}\t{}\t{}\t{}", idx, location, ctx, preview)
            } else {
                format!("{}\t{}\t\t{}", idx, location, preview)
            };
            (idx, row)
        })
        .collect();
    let location_width = rendered_rows
        .iter()
        .filter_map(|(_, row)| row.split('\t').nth(1))
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0);
    let context_width = rendered_rows
        .iter()
        .filter_map(|(_, row)| row.split('\t').nth(2))
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0);
    let item_list = rendered_rows
        .into_iter()
        .map(|(idx, row)| {
            let mut parts = row.splitn(4, '\t');
            let idx_part = parts.next().unwrap_or_default();
            let loc_part = parts.next().unwrap_or_default();
            let ctx_part = parts.next().unwrap_or_default();
            let preview_part = parts.next().unwrap_or_default();
            format!(
                "{}\t{:<location_width$}  {:<context_width$}  {}",
                idx,
                loc_part,
                ctx_part,
                preview_part,
                location_width = location_width,
                context_width = context_width
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
        let mut matches = scan_directory_for_block(&target_dir, &block_name, &block_type)?;
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
                        "ℹ Last selected match ({}) does not contain '{}', will prompt for new selection",
                        last_match.file_path.display(), block_name
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
        eprintln!("✓ Found {} matching {} block(s):", matches.len(), type_str);
        for m in &matches {
            let rel_path = relative_display_path(&m.file_path);
            if let Some((start, end)) = m.line_range {
                eprintln!("  • {}:{}-{}", rel_path, start, end);
            } else {
                eprintln!("  • {}", rel_path);
            }
        }
        let selected_matches = if matches.len() > 1 {
            let remembered_match = if let Some(ref last_match) = last_selected_match {
                matches
                    .iter()
                    .find(|m| {
                        m.file_path == last_match.file_path && m.line_range == last_match.line_range
                    })
                    .cloned()
            } else {
                None
            };
            if let Some(matched) = remembered_match {
                let range_str = matched
                    .line_range
                    .map(|(s, e)| format!("{}-{}", s, e))
                    .unwrap_or_else(|| "unknown".to_string());
                eprintln!(
                    "\n📁 Using previously selected match: {}:{}",
                    relative_display_path(&matched.file_path),
                    range_str
                );
                vec![matched]
            } else {
                eprintln!("\n📁 Multiple matches found. Select one to modify:");
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
            eprintln!("\n  File: {} (lines {})", m.file_path.display(), range_str);
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
            eprintln!("  ✓ Updated: {} (lines {})", file_path.display(), range_str);
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
    path.strip_prefix(&cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}
