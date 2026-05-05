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
        let formatted = fs::read_to_string(&temp_file)?;
        Ok(formatted)
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
                let (end_pos, found) = find_block_end(&content, abs_start);
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
                let (end_pos, found) = find_block_end(&content, abs_start);
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
                let (end_pos, found) = find_block_end(&content, abs_start);
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
fn find_enum_in_file(file_path: &Path, enum_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let content = fs::read_to_string(file_path)?;
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    for item in file.items {
        if let Item::Enum(item_enum) = item {
            if item_enum.ident == enum_name {
                let enum_str = quote::quote!(# item_enum).to_string();
                matches.push(CodeBlockMatch {
                    file_path: file_path.to_path_buf(),
                    block_name: enum_name.to_string(),
                    block_type: CodeBlockType::Enum,
                    old_full_block: enum_str,
                    original_vis: Some(item_enum.vis),
                    original_asyncness: None,
                    original_unsafety: None,
                    original_attrs: item_enum.attrs,
                });
            }
        }
    }
    Ok(matches)
}
fn find_function_in_file(file_path: &Path, func_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let content = fs::read_to_string(file_path)?;
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    for item in file.items {
        match item {
            Item::Fn(item_fn) => {
                if item_fn.sig.ident == func_name {
                    let fn_str = quote::quote!(# item_fn).to_string();
                    matches.push(CodeBlockMatch {
                        file_path: file_path.to_path_buf(),
                        block_name: func_name.to_string(),
                        block_type: CodeBlockType::Function,
                        old_full_block: fn_str,
                        original_vis: Some(item_fn.vis),
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
                            let fn_str = quote::quote!(# method_fn).to_string();
                            matches.push(CodeBlockMatch {
                                file_path: file_path.to_path_buf(),
                                block_name: func_name.to_string(),
                                block_type: CodeBlockType::Function,
                                old_full_block: fn_str,
                                original_vis: Some(method_fn.vis.clone()),
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
fn find_struct_in_file(file_path: &Path, struct_name: &str) -> Result<Vec<CodeBlockMatch>> {
    let content = fs::read_to_string(file_path)?;
    let file = parse_file(&content)?;
    let mut matches = Vec::new();
    for item in file.items {
        if let Item::Struct(item_struct) = item {
            if item_struct.ident == struct_name {
                let struct_str = quote::quote!(# item_struct).to_string();
                matches.push(CodeBlockMatch {
                    file_path: file_path.to_path_buf(),
                    block_name: struct_name.to_string(),
                    block_type: CodeBlockType::Struct,
                    old_full_block: struct_str,
                    original_vis: Some(item_struct.vis),
                    original_asyncness: None,
                    original_unsafety: None,
                    original_attrs: item_struct.attrs,
                });
            }
        }
    }
    Ok(matches)
}
fn replace_function_in_file(
    content: &str,
    func_match: &CodeBlockMatch,
    new_block_str: &str,
) -> Result<String> {
    let mut file: File = parse_file(content)?;
    let new_item_fn: ItemFn = parse_str(new_block_str)?;
    for item in &mut file.items {
        match item {
            Item::Fn(item_fn) => {
                if item_fn.sig.ident == func_match.block_name {
                    let mut final_attrs = func_match.original_attrs.clone();
                    for attr in &new_item_fn.attrs {
                        if !final_attrs.contains(attr) {
                            final_attrs.push(attr.clone());
                        }
                    }
                    let final_vis = if func_match.original_vis.is_some() {
                        func_match.original_vis.clone().unwrap()
                    } else {
                        new_item_fn.vis.clone()
                    };
                    let final_asyncness = if func_match.original_asyncness.is_some() {
                        func_match.original_asyncness
                    } else {
                        new_item_fn.sig.asyncness
                    };
                    let final_unsafety = if func_match.original_unsafety.is_some() {
                        func_match.original_unsafety
                    } else {
                        new_item_fn.sig.unsafety
                    };
                    let mut preserved_sig = new_item_fn.sig.clone();
                    preserved_sig.asyncness = final_asyncness;
                    preserved_sig.unsafety = final_unsafety;
                    *item_fn = ItemFn {
                        attrs: final_attrs,
                        vis: final_vis,
                        sig: preserved_sig,
                        block: new_item_fn.block.clone(),
                    };
                    break;
                }
            }
            Item::Impl(item_impl) => {
                for method in &mut item_impl.items {
                    if let ImplItem::Fn(method_fn) = method {
                        if method_fn.sig.ident == func_match.block_name {
                            let mut final_attrs = func_match.original_attrs.clone();
                            for attr in &new_item_fn.attrs {
                                if !final_attrs.contains(attr) {
                                    final_attrs.push(attr.clone());
                                }
                            }
                            let final_vis = if func_match.original_vis.is_some() {
                                func_match.original_vis.clone().unwrap()
                            } else {
                                new_item_fn.vis.clone()
                            };
                            let final_asyncness = if func_match.original_asyncness.is_some() {
                                func_match.original_asyncness
                            } else {
                                new_item_fn.sig.asyncness
                            };
                            let final_unsafety = if func_match.original_unsafety.is_some() {
                                func_match.original_unsafety
                            } else {
                                new_item_fn.sig.unsafety
                            };
                            let mut preserved_sig = new_item_fn.sig.clone();
                            preserved_sig.asyncness = final_asyncness;
                            preserved_sig.unsafety = final_unsafety;
                            method_fn.attrs = final_attrs;
                            method_fn.vis = final_vis;
                            method_fn.sig = preserved_sig;
                            method_fn.block = (*new_item_fn.block).clone();
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let new_content = prettyplease::unparse(&file);
    Ok(new_content)
}
fn replace_struct_in_file(
    content: &str,
    struct_match: &CodeBlockMatch,
    new_block_str: &str,
) -> Result<String> {
    let mut file: File = parse_file(content)?;
    let new_item_struct: ItemStruct = parse_str(new_block_str)?;
    for item in &mut file.items {
        if let Item::Struct(item_struct) = item {
            if item_struct.ident == struct_match.block_name {
                let mut final_attrs = struct_match.original_attrs.clone();
                for attr in &new_item_struct.attrs {
                    if !final_attrs.contains(attr) {
                        final_attrs.push(attr.clone());
                    }
                }
                let final_vis = if struct_match.original_vis.is_some() {
                    struct_match.original_vis.clone().unwrap()
                } else {
                    new_item_struct.vis.clone()
                };
                *item_struct = ItemStruct {
                    attrs: final_attrs,
                    vis: final_vis,
                    struct_token: new_item_struct.struct_token,
                    ident: new_item_struct.ident.clone(),
                    generics: new_item_struct.generics.clone(),
                    fields: new_item_struct.fields.clone(),
                    semi_token: new_item_struct.semi_token,
                };
                break;
            }
        }
    }
    let new_content = prettyplease::unparse(&file);
    Ok(new_content)
}
fn replace_enum_in_file(
    content: &str,
    enum_match: &CodeBlockMatch,
    new_block_str: &str,
) -> Result<String> {
    let mut file: File = parse_file(content)?;
    let new_item_enum: ItemEnum = parse_str(new_block_str)?;
    for item in &mut file.items {
        if let Item::Enum(item_enum) = item {
            if item_enum.ident == enum_match.block_name {
                let mut final_attrs = enum_match.original_attrs.clone();
                for attr in &new_item_enum.attrs {
                    if !final_attrs.contains(attr) {
                        final_attrs.push(attr.clone());
                    }
                }
                let final_vis = if let Some(ref vis) = enum_match.original_vis {
                    vis.clone()
                } else {
                    new_item_enum.vis.clone()
                };
                *item_enum = ItemEnum {
                    attrs: final_attrs,
                    vis: final_vis,
                    enum_token: new_item_enum.enum_token,
                    ident: new_item_enum.ident.clone(),
                    generics: new_item_enum.generics.clone(),
                    brace_token: new_item_enum.brace_token,
                    variants: new_item_enum.variants.clone(),
                };
                break;
            }
        }
    }
    let new_content = prettyplease::unparse(&file);
    Ok(new_content)
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
fn select_file_with_fzf(files: &[PathBuf]) -> Result<Option<PathBuf>> {
    let fzf_check = Command::new("fzf").arg("--version").output();
    if fzf_check.is_err() {
        bail!("fzf not found");
    }
    let file_list: String = files
        .iter()
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
    Ok(Some(PathBuf::from(selected)))
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
fn main() -> Result<()> {
    let config = load_unified_config()?;
    let args: Vec<String> = env::args().skip(1).collect();
    let target_dir = if args.is_empty() {
        env::current_dir()?
    } else {
        PathBuf::from(&args[0])
    };
    let mut last_selected_file: Option<PathBuf> = None;
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
                    continue;
                }
                _ => {}
            }
        }
        eprintln!("\n🔎 Scanning for {} '{}'...", type_str, block_name);
        let mut matches = scan_directory_for_block(&target_dir, &block_name, &block_type)?;
        if let Some(ref last_file) = last_selected_file {
            let file_matches: Vec<CodeBlockMatch> = matches
                .iter()
                .filter(|m| m.file_path == *last_file)
                .cloned()
                .collect();
            if !file_matches.is_empty() {
                eprintln!("✓ Using previously selected file: {}", last_file.display());
                matches = file_matches;
            } else {
                eprintln!(
                    "ℹ Last selected file ({}) does not contain '{}', will prompt for new selection",
                    last_file.display(), block_name
                );
                last_selected_file = None;
            }
        }
        if matches.is_empty() {
            eprintln!("⚠ No matches found for '{}', skipping", block_name);
            skipped_count += 1;
            last_selected_file = None;
            continue;
        }
        eprintln!("✓ Found {} matching {} block(s):", matches.len(), type_str);
        for m in &matches {
            if let Some(vis) = &m.original_vis {
                let vis_str = match vis {
                    Visibility::Public(_) => "pub",
                    _ => "",
                };
                eprintln!("  • {} (visibility: {})", m.file_path.display(), vis_str);
            } else {
                eprintln!("  • {}", m.file_path.display());
            }
        }
        let selected_matches = if matches.len() > 1 {
            let remembered_match = if let Some(ref last_file) = last_selected_file {
                matches.iter().find(|m| m.file_path == *last_file).cloned()
            } else {
                None
            };
            if let Some(matched) = remembered_match {
                eprintln!(
                    "\n📁 Using previously selected file: {}",
                    matched.file_path.display()
                );
                vec![matched]
            } else {
                let files: Vec<PathBuf> = matches.iter().map(|m| m.file_path.clone()).collect();
                eprintln!("\n📁 Multiple files found. Select one to modify:");
                match select_file_with_fzf(&files)? {
                    Some(selected_file) => {
                        last_selected_file = Some(selected_file.clone());
                        matches
                            .into_iter()
                            .filter(|m| m.file_path == selected_file)
                            .collect()
                    }
                    None => {
                        eprintln!("⚠ No file selected for '{}', skipping", block_name);
                        skipped_count += 1;
                        continue;
                    }
                }
            }
        } else {
            if matches.len() == 1 {
                last_selected_file = Some(matches[0].file_path.clone());
            }
            matches
        };
        let sample_file_path = selected_matches.first().map(|m| m.file_path.as_path());
        eprintln!("\n📝 Preview of changes:");
        let mut has_changes = false;
        for m in &selected_matches {
            eprintln!("\n  File: {}", m.file_path.display());
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
            continue;
        }
        print!("\n❓ Apply these changes? (y/n): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            eprintln!("⚠ Skipping {}: {}", type_str, block_name);
            skipped_count += 1;
            last_selected_file = None;
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
            eprintln!("  ✓ Updated: {}", file_path.display());
            processed_count += 1;
        }
        eprintln!("\n✅ {} '{}' processed successfully!", type_str, block_name);
    }
    eprintln!("\n\x1b[1;32m━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\x1b[0m");
    eprintln!("\x1b[1;32m✅ All code blocks processed!\x1b[0m");
    eprintln!("  Blocks processed: {}", processed_count);
    eprintln!("  Blocks skipped: {}", skipped_count);
    Ok(())
}
