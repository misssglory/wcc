use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use wcc::common::*;
use wcc::config::{load_unified_config, UnifiedConfig};
#[derive(Debug, Clone)]
enum CodeBlockType {
    Function,
    Impl,
    Struct,
}
#[derive(Debug, Clone)]
struct CodeBlockInfo {
    file_path: PathBuf,
    relative_path: PathBuf,
    block_name: String,
    block_body: String,
    start_line: usize,
    end_line: usize,
    file_lines: usize,
    file_modified: Option<DateTime<Local>>,
    block_type: CodeBlockType,
    visibility: String,
    asyncness: bool,
}
impl CodeBlockInfo {
    fn new(
        file_path: PathBuf,
        relative_path: PathBuf,
        block_name: String,
        block_body: String,
        start_line: usize,
        end_line: usize,
        file_lines: usize,
        file_modified: Option<DateTime<Local>>,
        block_type: CodeBlockType,
        visibility: String,
        asyncness: bool,
    ) -> Self {
        Self {
            file_path,
            relative_path,
            block_name,
            block_body,
            start_line,
            end_line,
            file_lines,
            file_modified,
            block_type,
            visibility,
            asyncness,
        }
    }
    fn get_modifier_string(&self) -> String {
        match self.block_type {
            CodeBlockType::Function => {
                let mut parts = Vec::new();
                if !self.visibility.is_empty() {
                    parts.push(self.visibility.clone());
                }
                if self.asyncness {
                    parts.push("async".to_string());
                }
                let modifiers = parts.join(" ");
                if modifiers.is_empty() {
                    String::new()
                } else {
                    format!("{} ", modifiers)
                }
            }
            CodeBlockType::Impl => "impl ".to_string(),
            CodeBlockType::Struct => {
                if self.visibility == "pub" {
                    "pub struct ".to_string()
                } else {
                    "struct ".to_string()
                }
            }
        }
    }
    fn get_type_icon(&self) -> &'static str {
        match self.block_type {
            CodeBlockType::Function => "ƒ",
            CodeBlockType::Impl => "ℑ",
            CodeBlockType::Struct => "Ⓢ",
        }
    }
    fn get_type_name(&self) -> &'static str {
        match self.block_type {
            CodeBlockType::Function => "function",
            CodeBlockType::Impl => "impl block",
            CodeBlockType::Struct => "struct",
        }
    }
    fn get_telescope_entry(&self) -> String {
        format!(
            "{}│{}│{}│{}│{}",
            self.get_type_icon(),
            self.block_name,
            self.relative_path.display(),
            self.start_line,
            self.end_line
        )
    }
    fn get_display_string(&self) -> String {
        format!(
            "{} {} {} ({}: lines {}-{})",
            self.get_type_icon(),
            self.get_modifier_string(),
            self.block_name,
            self.relative_path.display(),
            self.start_line,
            self.end_line
        )
    }
    fn format_header(&self, config: &UnifiedConfig, show_line_numbers: bool) -> Result<String> {
        let comment_prefix = match self.file_path.extension().and_then(|e| e.to_str()) {
            Some("rs") => "//",
            Some("py") => "#",
            Some("js") | Some("ts") | Some("jsx") | Some("tsx") => "//",
            Some("c") | Some("cc") | Some("cpp") | Some("h") | Some("hpp") => "//",
            Some("go") => "//",
            _ => "//",
        };
        let mut header_parts = Vec::new();
        header_parts.push(format!("{}", self.relative_path.display()));
        header_parts.push(format!("# {}", self.block_name));
        if !show_line_numbers {
            header_parts.push(format!("# lines {}-{}", self.start_line, self.end_line));
        }
        if config.wff.show_time_in_header {
            let timestamp = if config.wcn.use_file_modification_time {
                if let Some(modified) = self.file_modified {
                    modified.format(&config.wcc.time_format).to_string()
                } else {
                    let now = Local::now();
                    now.format(&config.wcc.time_format).to_string()
                }
            } else {
                let now = Local::now();
                now.format(&config.wcc.time_format).to_string()
            };
            header_parts.push(format!("# {}", timestamp));
        }
        Ok(format!("{} {}\n", comment_prefix, header_parts.join(" ")))
    }
    fn add_line_numbers_to_body(&self) -> String {
        let lines: Vec<&str> = self.block_body.lines().collect();
        let line_number_width = (self.end_line as f64).log10().floor() as usize + 1;
        lines
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                let line_num = self.start_line + idx;
                format!("{:>width$}  {}", line_num, line, width = line_number_width)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn print_stats(&self) -> Result<()> {
        let stats = calc_stats(&self.block_body);
        let file_percentage =
            (self.block_body.lines().count() as f64 / self.file_lines as f64) * 100.0;
        println!(
            "\n\x1b[36mfile\x1b[0m {}  \x1b[36m{}\x1b[0m {}{}",
            self.relative_path.display(),
            self.get_type_name(),
            self.get_modifier_string(),
            self.block_name
        );
        println!(
            "  \x1b[33mlines\x1b[0m {} ({}% of file)  \x1b[33mwords\x1b[0m {}  \x1b[33mchars\x1b[0m {}  \x1b[33mbytes\x1b[0m {}",
            stats.lines, format!("{:.1}", file_percentage), stats.words, stats.chars,
            stats.bytes
        );
        println!(
            "  \x1b[33mline range\x1b[0m {}-{}",
            self.start_line, self.end_line
        );
        Ok(())
    }
    fn to_clipboard_payload(
        &self,
        config: &UnifiedConfig,
        show_line_numbers: bool,
    ) -> Result<String> {
        let header = self.format_header(config, show_line_numbers)?;
        let body = if show_line_numbers {
            self.add_line_numbers_to_body()
        } else {
            self.block_body.clone()
        };
        Ok(format!("{}{}", header, body))
    }
    fn to_json(&self) -> Result<String> {
        Ok(format!(
            r#"{{"type":"{:?}","name":"{}","path":"{}","line":{},"end_line":{}}}"#,
            self.block_type,
            self.block_name,
            self.relative_path.display(),
            self.start_line,
            self.end_line
        ))
    }
}
struct CodeBlockScanner {
    skip_dirs: Vec<String>,
}
impl CodeBlockScanner {
    fn new() -> Self {
        Self {
            skip_dirs: vec![
                "target".to_string(),
                "node_modules".to_string(),
                ".git".to_string(),
                ".cargo".to_string(),
                ".idea".to_string(),
                ".vscode".to_string(),
            ],
        }
    }
    fn scan_directory(&self, dir: &Path) -> Result<Vec<CodeBlockInfo>> {
        let mut blocks = Vec::new();
        self.walk_directory(dir, &mut blocks)?;
        Ok(blocks)
    }
    fn walk_directory(&self, dir: &Path, blocks: &mut Vec<CodeBlockInfo>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name() {
                    let name_str = name.to_string_lossy();
                    if self.skip_dirs.iter().any(|d| d == name_str.as_ref()) {
                        continue;
                    }
                }
                self.walk_directory(&path, blocks)?;
            } else if self.is_rust_file(&path) {
                if let Ok(blocks_in_file) = self.find_blocks_in_file(&path) {
                    blocks.extend(blocks_in_file);
                }
            }
        }
        Ok(())
    }
    fn is_rust_file(&self, path: &Path) -> bool {
        path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs")
    }
    fn find_blocks_in_file(&self, file_path: &Path) -> Result<Vec<CodeBlockInfo>> {
        let content = fs::read_to_string(file_path)?;
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let metadata = fs::metadata(file_path)?;
        let modified = metadata.modified()?;
        let modified_datetime: DateTime<Local> = modified.into();
        let current_dir = env::current_dir()?;
        let relative_path = file_path
            .strip_prefix(&current_dir)
            .unwrap_or(file_path)
            .to_path_buf();
        let mut blocks = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                i += 1;
                continue;
            }
            if trimmed.contains("struct ") && !trimmed.contains("fn ") {
                let mut start_idx = i;
                while start_idx > 0 && lines[start_idx - 1].trim().starts_with("#[") {
                    start_idx -= 1;
                }
                if let Some(struct_info) = self.extract_struct_block(
                    &lines,
                    start_idx,
                    file_path,
                    &relative_path,
                    total_lines,
                    modified_datetime,
                )? {
                    let end_line = struct_info.end_line;
                    blocks.push(struct_info);
                    i = end_line;
                    continue;
                }
            }
            if trimmed.starts_with("impl ") {
                if let Some(impl_info) = self.extract_impl_block(
                    &lines,
                    i,
                    file_path,
                    &relative_path,
                    total_lines,
                    modified_datetime,
                )? {
                    let impl_end_line = impl_info.end_line;
                    blocks.push(impl_info.clone());
                    let mut j = i + 1;
                    while j < impl_end_line {
                        let inner_line = lines[j];
                        let inner_trimmed = inner_line.trim();
                        if inner_trimmed.starts_with("//") {
                            j += 1;
                            continue;
                        }
                        if inner_trimmed.contains("fn ") && !inner_trimmed.starts_with("//") {
                            if let Some(func_info) = self.extract_function_block(
                                &lines,
                                j,
                                file_path,
                                &relative_path,
                                total_lines,
                                modified_datetime,
                            )? {
                                if func_info.end_line <= impl_end_line {
                                    let func_end_line = func_info.end_line;
                                    blocks.push(func_info);
                                    j = func_end_line;
                                    continue;
                                }
                            }
                        }
                        j += 1;
                    }
                    i = impl_end_line;
                    continue;
                }
            }
            if line.contains("fn ") && !line.trim_start().starts_with("//") {
                let mut is_inside_impl = false;
                for block in &blocks {
                    if let CodeBlockType::Impl = block.block_type {
                        if block.start_line <= i + 1 && i + 1 <= block.end_line {
                            is_inside_impl = true;
                            break;
                        }
                    }
                }
                if !is_inside_impl {
                    if let Some(func_info) = self.extract_function_block(
                        &lines,
                        i,
                        file_path,
                        &relative_path,
                        total_lines,
                        modified_datetime,
                    )? {
                        let end_line = func_info.end_line;
                        blocks.push(func_info);
                        i = end_line;
                        continue;
                    }
                }
            }
            i += 1;
        }
        Ok(blocks)
    }
    fn extract_struct_block(
        &self,
        lines: &[&str],
        start_idx: usize,
        file_path: &Path,
        relative_path: &PathBuf,
        total_lines: usize,
        modified_datetime: DateTime<Local>,
    ) -> Result<Option<CodeBlockInfo>> {
        let mut struct_idx = start_idx;
        while struct_idx < lines.len() && lines[struct_idx].trim().starts_with("#[") {
            struct_idx += 1;
        }
        if struct_idx >= lines.len() {
            return Ok(None);
        }
        let line = lines[struct_idx];
        let trimmed = line.trim();
        let (visibility, name) = if trimmed.starts_with("pub struct ") {
            ("pub", trimmed[11..].trim())
        } else if trimmed.starts_with("struct ") {
            ("", trimmed[7..].trim())
        } else {
            return Ok(None);
        };
        let struct_name = name
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()
            .unwrap_or("")
            .to_string();
        if struct_name.is_empty() {
            return Ok(None);
        }
        let mut brace_count = 0;
        let mut found_brace = false;
        let mut end_line = struct_idx + 1;
        for j in struct_idx..lines.len() {
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
        let block_body = lines[start_idx..end_line].join("\n");
        Ok(Some(CodeBlockInfo::new(
            file_path.to_path_buf(),
            relative_path.clone(),
            struct_name,
            block_body,
            start_idx + 1,
            end_line,
            total_lines,
            Some(modified_datetime),
            CodeBlockType::Struct,
            visibility.to_string(),
            false,
        )))
    }
    fn extract_impl_block(
        &self,
        lines: &[&str],
        start_idx: usize,
        file_path: &Path,
        relative_path: &PathBuf,
        total_lines: usize,
        modified_datetime: DateTime<Local>,
    ) -> Result<Option<CodeBlockInfo>> {
        let line = lines[start_idx];
        let trimmed = line.trim();
        if !trimmed.starts_with("impl ") {
            return Ok(None);
        }
        let after_impl = &trimmed[5..];
        let impl_name = after_impl
            .split(|c: char| c == '{' || c == ' ' || c == '<')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let impl_name = if impl_name.is_empty() {
            "impl block".to_string()
        } else {
            impl_name
        };
        let mut brace_count = 0;
        let mut found_brace = false;
        let mut end_line = start_idx + 1;
        for j in start_idx..lines.len() {
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
        if !found_brace {
            return Ok(None);
        }
        let block_body = lines[start_idx..end_line].join("\n");
        Ok(Some(CodeBlockInfo::new(
            file_path.to_path_buf(),
            relative_path.clone(),
            impl_name,
            block_body,
            start_idx + 1,
            end_line,
            total_lines,
            Some(modified_datetime),
            CodeBlockType::Impl,
            String::new(),
            false,
        )))
    }
    fn extract_function_block(
        &self,
        lines: &[&str],
        start_idx: usize,
        file_path: &Path,
        relative_path: &PathBuf,
        total_lines: usize,
        modified_datetime: DateTime<Local>,
    ) -> Result<Option<CodeBlockInfo>> {
        let line = lines[start_idx];
        let trimmed = line.trim();
        let mut visibility = "";
        let mut asyncness = false;
        let mut fn_name = String::new();
        if trimmed.contains("pub async fn") {
            let after = trimmed.split("pub async fn").nth(1).unwrap_or("").trim();
            fn_name = after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("")
                .to_string();
            visibility = "pub";
            asyncness = true;
        } else if trimmed.contains("pub fn") {
            let after = trimmed.split("pub fn").nth(1).unwrap_or("").trim();
            fn_name = after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("")
                .to_string();
            visibility = "pub";
        } else if trimmed.contains("async fn") {
            let after = trimmed.split("async fn").nth(1).unwrap_or("").trim();
            fn_name = after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("")
                .to_string();
            asyncness = true;
        } else if trimmed.contains("fn ") {
            let after = trimmed.split("fn").nth(1).unwrap_or("").trim();
            fn_name = after
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("")
                .to_string();
            if trimmed.contains("pub") {
                visibility = "pub";
            }
            if trimmed.contains("async") {
                asyncness = true;
            }
        }
        if fn_name.is_empty() {
            return Ok(None);
        }
        let mut brace_count = 0;
        let mut found_brace = false;
        let mut end_line = start_idx + 1;
        for j in start_idx..lines.len() {
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
        if !found_brace {
            return Ok(None);
        }
        let block_body = lines[start_idx..end_line].join("\n");
        Ok(Some(CodeBlockInfo::new(
            file_path.to_path_buf(),
            relative_path.clone(),
            fn_name,
            block_body,
            start_idx + 1,
            end_line,
            total_lines,
            Some(modified_datetime),
            CodeBlockType::Function,
            visibility.to_string(),
            asyncness,
        )))
    }
}
struct Application {
    config: UnifiedConfig,
    show_line_numbers: bool,
    scanner: CodeBlockScanner,
}
impl Application {
    fn new() -> Result<Self> {
        let config = load_unified_config()?;
        let args: Vec<String> = env::args().collect();
        let mut show_line_numbers = config.wff.show_line_numbers;
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-n" => {
                    show_line_numbers = !config.wff.show_line_numbers;
                }
                _ => {}
            }
            i += 1;
        }
        Ok(Self {
            config,
            show_line_numbers,
            scanner: CodeBlockScanner::new(),
        })
    }
    fn run(&self) -> Result<()> {
        let args: Vec<String> = env::args().collect();
        let telescope_mode = args.iter().any(|arg| arg == "--telescope" || arg == "-t");
        let target_dir = if args.len() > 1 {
            let last_arg = args.last().unwrap();
            if last_arg == "-n" || last_arg == "--telescope" || last_arg == "-t" {
                env::current_dir()?
            } else {
                let mut dir = env::current_dir()?;
                for arg in &args[1..] {
                    if !arg.starts_with('-') {
                        dir = PathBuf::from(arg);
                        break;
                    }
                }
                dir
            }
        } else {
            env::current_dir()?
        };
        self.validate_directory(&target_dir)?;
        if telescope_mode {
            self.output_for_telescope(&target_dir)?;
        } else {
            self.interactive_mode(&target_dir)?;
        }
        Ok(())
    }
    fn output_for_telescope(&self, dir: &Path) -> Result<()> {
        let blocks = self.scanner.scan_directory(dir)?;
        if blocks.is_empty() {
            eprintln!("No code blocks found");
            return Ok(());
        }
        for block in blocks {
            println!("{}", block.to_json()?);
        }
        Ok(())
    }
    fn interactive_mode(&self, dir: &Path) -> Result<()> {
        eprintln!("🔍 Scanning directory: {}", dir.display());
        let blocks = self.scanner.scan_directory(dir)?;
        if blocks.is_empty() {
            bail!("No functions, structs, or impl blocks found in directory");
        }
        self.print_block_summary(&blocks);
        let selector = CodeBlockSelector::new();
        let selected = selector.select_block(&blocks)?;
        let payload = selected.to_clipboard_payload(&self.config, self.show_line_numbers)?;
        ClipboardManager::set_clipboard(&payload)?;
        selected.print_stats()?;
        eprintln!("\n\x1b[1;32m✓ Code block copied to clipboard!\x1b[0m");
        if self.show_line_numbers {
            eprintln!("\x1b[90mℹ Line numbers included (use -n to toggle off)\x1b[0m");
        } else {
            eprintln!("\x1b[90mℹ Line numbers hidden (use -n to toggle on)\x1b[0m");
        }
        Ok(())
    }
    fn validate_directory(&self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            bail!("Directory does not exist: {}", dir.display());
        }
        Ok(())
    }
    fn print_block_summary(&self, blocks: &[CodeBlockInfo]) {
        eprintln!("✓ Found {} code block(s)", blocks.len());
        for block in blocks.iter().take(10) {
            eprintln!("  • {}", block.get_display_string());
        }
        if blocks.len() > 10 {
            eprintln!("  ... and {} more", blocks.len() - 10);
        }
    }
}
struct CodeBlockSelector {
    fzf_command: String,
}
impl CodeBlockSelector {
    fn new() -> Self {
        Self {
            fzf_command: "fzf".to_string(),
        }
    }
    fn is_available(&self) -> bool {
        Command::new(&self.fzf_command)
            .arg("--version")
            .output()
            .is_ok()
    }
    fn select_block(&self, blocks: &[CodeBlockInfo]) -> Result<CodeBlockInfo> {
        if !self.is_available() {
            bail!("fzf not found. Please install fzf to select functions/impl blocks");
        }
        let preview_lines = self.build_preview_lines(blocks);
        let preview_text = preview_lines.join("\n");
        let mut fzf_child = Command::new(&self.fzf_command)
            .arg("--height")
            .arg("60%")
            .arg("--border")
            .arg("--ansi")
            .arg("--layout=reverse")
            .arg("--with-nth=2..")
            .arg("--delimiter=│")
            .arg("--preview")
            .arg(
                "bat --style=numbers --color=always --language=rust --line-range={5}:{6} {4} 2>/dev/null || (echo '--- Code Block ---' && sed -n '{5},{6}p' {4})",
            )
            .arg("--preview-window=right:50%:wrap")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn fzf")?;
        {
            let mut stdin = fzf_child.stdin.take().context("Failed to open fzf stdin")?;
            stdin.write_all(preview_text.as_bytes())?;
        }
        let output = fzf_child
            .wait_with_output()
            .context("Failed to read fzf output")?;
        if !output.status.success() {
            bail!("No code block selected");
        }
        let selected_line = String::from_utf8_lossy(&output.stdout);
        let first_line = selected_line.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.split('│').collect();
        if parts.is_empty() {
            bail!("Invalid selection format");
        }
        let idx_str = parts[0].trim();
        let idx: usize = idx_str.parse().unwrap_or(0);
        if idx == 0 || idx > blocks.len() {
            bail!("Invalid selection");
        }
        Ok(blocks[idx - 1].clone())
    }
    fn build_preview_lines(&self, blocks: &[CodeBlockInfo]) -> Vec<String> {
        blocks
            .iter()
            .enumerate()
            .map(|(idx, block)| {
                format!(
                    "{:3} │ {} {} │ {} │ {}-{}",
                    idx + 1,
                    block.get_type_icon(),
                    block.block_name,
                    block.relative_path.display(),
                    block.start_line,
                    block.end_line
                )
            })
            .collect()
    }
}
struct ClipboardManager;
impl ClipboardManager {
    fn set_clipboard(content: &str) -> Result<()> {
        set_clipboard(content)
    }
}
fn main() -> Result<()> {
    let app = Application::new()?;
    app.run()
}
