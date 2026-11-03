use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use arboard::Clipboard;
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;
use thiserror::Error;
use wcc::config::init_config;
use wcc::load_unified_config;

#[derive(Parser, Debug)]
#[command(author, version, about = "Run commands and copy output to clipboard")]
struct Cli {
    #[command(subcommand)]
    command: Option<Mode>,

    #[arg(trailing_var_arg = true)]
    cmd: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// Build a package (wraps cargo build)
    Build {
        #[arg(short, long)]
        release: bool,
        #[arg(long)]
        debug: bool,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Run a binary (wraps cargo run)
    Run {
        #[arg(short, long)]
        release: bool,
        #[arg(long)]
        debug: bool,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Configure wcc settings
    Config {
        #[arg(long)]
        set_cargo_mode: Option<String>,
        #[arg(long)]
        show: bool,
        #[arg(long)]
        init: bool,
    },
}

#[derive(Error, Debug)]
enum WccError {
    #[error("no command specified")]
    NoCommand,
}

#[derive(Debug)]
struct StreamTail {
    content: String,
}

impl StreamTail {
    fn new() -> Self {
        Self {
            content: String::new(),
        }
    }

    fn push(&mut self, chunk: &str) {
        self.content.push_str(chunk);
    }
}

#[derive(Debug)]
enum Msg {
    Stdout(String),
    Stderr(String),
}

fn set_clipboard(command: &[String], stdout: &str, stderr: &str, exit_code: Option<i32>) -> Result<()> {
    use chrono::Local;
    let timestamp = Local::now().format("%H:%M:%S %d.%m.%Y");
    let payload = format!(
        "$ {} # {}\n\n[stdout]\n{}\n\n[stderr]\n{}\n\n[exit code]\n{}",
        command.join(" "),
        timestamp,
        stdout,
        stderr,
        exit_code.map(|code| code.to_string()).unwrap_or_else(|| "terminated by signal".to_string())
    );
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let mut child = Command::new("wl-copy")
                .arg("--type")
                .arg("text/plain;charset=utf-8")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("failed to spawn wl-copy")?;
            {
                let mut stdin = child.stdin.take().context("failed to open wl-copy stdin")?;
                stdin.write_all(payload.as_bytes())?;
                stdin.flush()?;
            }
            let _ = child.wait();
            return Ok(());
        }
    }
    let mut cb = Clipboard::new().context("clipboard init failed")?;
    cb.set_text(payload)?;
    Ok(())
}

fn spawn_reader<R: io::Read + Send + 'static>(reader: R, tx: Sender<Msg>, is_err: bool) {
    thread::spawn(move || {
        let mut br = BufReader::new(reader);
        let mut buffer = String::new();
        loop {
            buffer.clear();
            match br.read_line(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = tx.send(if is_err {
                        Msg::Stderr(buffer.clone())
                    } else {
                        Msg::Stdout(buffer.clone())
                    });
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_stdin_forwarder(mut child_stdin: ChildStdin) {
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if child_stdin.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    if child_stdin.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

fn strip_ansi_codes(s: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[[0-9;]*[mK]").unwrap();
    re.replace_all(s, "").to_string()
}

fn wcc_history_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("wcc").join("history"))
}

fn append_wcc_history_entry(cmd: &str) {
    if let Some(path) = wcc_history_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{}", serde_json::to_string(cmd).unwrap_or_default());
        }
    }
}

fn read_wcc_history() -> Vec<String> {
    if let Some(path) = wcc_history_path() {
        if let Ok(content) = fs::read_to_string(path) {
            return content
                .lines()
                .filter_map(|line| {
                    // JSON strings keep multiline entries on one physical line;
                    // older history files used raw command lines.
                    let entry = serde_json::from_str::<String>(line)
                        .unwrap_or_else(|_| line.to_owned());
                    (!entry.trim().is_empty()).then_some(entry)
                })
                .collect();
        }
    }
    Vec::new()
}

fn run_cargo_build(release: bool, debug: bool, args: Vec<String>) -> Result<()> {
    let config = load_unified_config()?;

    let is_release = if release {
        true
    } else if debug {
        false
    } else {
        config.wcc.default_cargo_mode == "release"
    };

    let mut cargo_args = vec!["build".to_string()];
    if is_release {
        cargo_args.push("--release".to_string());
    }
    cargo_args.extend(args);

    let timestamp = chrono::Local::now().format("%H:%M:%S %d.%m.%Y");
    let command_str = format!("cargo {}", cargo_args.join(" "));

    eprintln!("\x1b[36m📦 Running {}\x1b[0m # {}", command_str, timestamp);

    let mut child = Command::new("cargo")
        .args(&cargo_args)
        .env("CARGO_TERM_COLOR", "always")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to run cargo build")?;

    let stdout = child.stdout.take().context("missing stdout")?;
    let stderr = child.stderr.take().context("missing stderr")?;

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone(), false);
    spawn_reader(stderr, tx, true);

    let mut out = StreamTail::new();
    let mut err = StreamTail::new();

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stdout(s) => {
                out.push(&s);
                print!("{s}");
                io::stdout().flush()?;
            }
            Msg::Stderr(s) => {
                err.push(&s);
                eprint!("{s}");
                io::stderr().flush()?;
            }
        }
    }

    let status = child.wait()?;

    let clean_stdout_str = strip_ansi_codes(&out.content);
    let clean_stderr_str = strip_ansi_codes(&err.content);

    let _ = set_clipboard(
        &vec![command_str.clone()],
        &clean_stdout_str,
        &clean_stderr_str,
        status.code(),
    );

    if status.success() {
        eprintln!("\n\x1b[1;32m✓ Build successful\x1b[0m");
    } else {
        eprintln!("\n\x1b[1;31m✗ Build failed\x1b[0m");
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

fn run_cargo_run(release: bool, debug: bool, args: Vec<String>) -> Result<()> {
    let config = load_unified_config()?;

    let is_release = if release {
        true
    } else if debug {
        false
    } else {
        config.wcc.default_cargo_mode == "release"
    };

    let mut cargo_args = vec!["run".to_string()];
    if is_release {
        cargo_args.push("--release".to_string());
    }
    cargo_args.extend(args);

    let timestamp = chrono::Local::now().format("%H:%M:%S %d.%m.%Y");
    let command_str = format!("cargo {}", cargo_args.join(" "));

    eprintln!("\x1b[36m🏃 Running {}\x1b[0m # {}", command_str, timestamp);

    let mut child = Command::new("cargo")
        .args(&cargo_args)
        .env("CARGO_TERM_COLOR", "always")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to run cargo run")?;

    let stdout = child.stdout.take().context("missing stdout")?;
    let stderr = child.stderr.take().context("missing stderr")?;

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone(), false);
    spawn_reader(stderr, tx, true);

    let mut out = StreamTail::new();
    let mut err = StreamTail::new();

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stdout(s) => {
                out.push(&s);
                print!("{s}");
                io::stdout().flush()?;
            }
            Msg::Stderr(s) => {
                err.push(&s);
                eprint!("{s}");
                io::stderr().flush()?;
            }
        }
    }

    let status = child.wait()?;

    let clean_stdout_str = strip_ansi_codes(&out.content);
    let clean_stderr_str = strip_ansi_codes(&err.content);

    let _ = set_clipboard(
        &vec![command_str.clone()],
        &clean_stdout_str,
        &clean_stderr_str,
        status.code(),
    );

    if status.success() {
        eprintln!("\n\x1b[1;32m✓ Run completed\x1b[0m");
    } else {
        eprintln!(
            "\n\x1b[1;31m✗ Run failed with exit code: {:?}\x1b[0m",
            status.code()
        );
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

fn update_cargo_mode(mode: &str) -> Result<()> {
    let config_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wcc/config.toml");

    let mut config: wcc::UnifiedConfig = if config_path.exists() {
        let data = fs::read_to_string(&config_path)?;
        toml::from_str(&data)?
    } else {
        wcc::UnifiedConfig::default()
    };

    config.wcc.default_cargo_mode = mode.to_string();

    let toml_str = toml::to_string_pretty(&config)?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&config_path, toml_str)?;

    println!("\x1b[32m✓ Updated default cargo mode to: {}\x1b[0m", mode);
    Ok(())
}

fn show_config() -> Result<()> {
    let config = load_unified_config()?;

    println!("\x1b[36m📋 Current wcc configuration:\x1b[0m");
    println!("  [wcc]");
    println!(
        "    default_cargo_mode: \x1b[33m{}\x1b[0m",
        config.wcc.default_cargo_mode
    );
    println!("  [wcn]");
    println!(
        "    show_time_in_header: {}",
        config.wcn.show_time_in_header
    );
    println!("  [wcp]");
    println!("    auto_backup: {}", config.wcp.auto_backup);
    println!("  [wcl]");
    println!(
        "    max_file_words_to_copy: {}",
        config.wcl.max_file_words_to_copy
    );
    println!("  [wcf]");
    println!("    auto_format: {}", config.wcf.auto_format);
    println!();
    println!("  Config file: \x1b[90m~/.config/wcc/config.toml\x1b[0m");

    Ok(())
}

fn run_shell_command(command: &str) -> Result<()> {
    use chrono::Local;

    append_wcc_history_entry(command);

    let timestamp = Local::now().format("%H:%M:%S %d.%m.%Y");
    eprintln!("\x1b[36m🔧 Running: {}\x1b[0m # {}", command, timestamp);

    // Bash + pipefail: a failure in any pipeline stage gives a nonzero status.
    // Fall back to POSIX sh if bash is not available.
    let shell = if Command::new("bash").arg("--version").stdout(Stdio::null())
        .stderr(Stdio::null()).status().is_ok() { "bash" } else { "sh" };
    let mut process = Command::new(shell);
    if shell == "bash" { process.arg("-o").arg("pipefail"); }
    let mut child = process.arg("-c")
        .arg(command)
        .env("CARGO_TERM_COLOR", "always")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run shell command: {}", command))?;

    let stdout = child.stdout.take().context("missing stdout")?;
    let stderr = child.stderr.take().context("missing stderr")?;

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone(), false);
    spawn_reader(stderr, tx, true);

    let mut out = StreamTail::new();
    let mut err = StreamTail::new();

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stdout(s) => {
                out.push(&s);
                print!("{s}");
                io::stdout().flush()?;
            }
            Msg::Stderr(s) => {
                err.push(&s);
                eprint!("{s}");
                io::stderr().flush()?;
            }
        }
    }

    let status = child.wait()?;
    let timestamp_local = Local::now().format("%H:%M:%S %d.%m.%Y");
    let clean_stdout = strip_ansi_codes(&out.content);
    let clean_stderr = strip_ansi_codes(&err.content);

    let _ = set_clipboard(&vec![command.to_string()], &clean_stdout, &clean_stderr, status.code());

    if status.success() {
        eprintln!(
            "\n\x1b[1;32m✓ Command completed\x1b[0m # {}",
            timestamp_local
        );
    } else {
        eprintln!(
            "\n\x1b[1;31m✗ Command failed with exit code: {}\x1b[0m # {}",
            status.code().map(|code| code.to_string()).unwrap_or_else(|| "terminated by signal".into()),
            timestamp_local
        );
        std::process::exit(status.code().unwrap_or(1));
    }

    eprintln!("  Copied to clipboard!");

    Ok(())
}

fn run_watch_command(command: Vec<String>) -> Result<()> {
    anyhow::ensure!(!command.is_empty(), "usage: wcc -- cmd args");
    let term = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&term))?;
    flag::register(SIGTERM, Arc::clone(&term))?;

    let started = Instant::now();

    let cmd_str = command.join(" ");
    append_wcc_history_entry(&cmd_str);

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", command[0]))?;

    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take().context("missing child stderr")?;
    if let Some(stdin) = child.stdin.take() {
        spawn_stdin_forwarder(stdin);
    }

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone(), false);
    spawn_reader(stderr, tx, true);

    let mut out = StreamTail::new();
    let mut err = StreamTail::new();

    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Stdout(s) => {
                    out.push(&s);
                    print!("{s}");
                    io::stdout().flush()?;
                }
                Msg::Stderr(s) => {
                    err.push(&s);
                    eprint!("{s}");
                    io::stderr().flush()?;
                }
            }
        }
        if term.load(Ordering::Relaxed) {
            let _ = child.kill();
            break;
        }
        if child.try_wait()?.is_some() {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Stdout(s) => {
                        out.push(&s);
                        print!("{s}");
                        io::stdout().flush()?;
                    }
                    Msg::Stderr(s) => {
                        err.push(&s);
                        eprint!("{s}");
                        io::stderr().flush()?;
                    }
                }
            }
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }

    let status = child.wait().ok();
    let duration_ms = started.elapsed().as_millis();

    let _ = set_clipboard(&command, &out.content, &err.content, status.as_ref().and_then(|s| s.code()));

    use chrono::Local;
    let timestamp_local = Local::now().format("%H:%M:%S %d.%m.%Y");
    eprintln!(
        "\n\x1b[1;32m✓ Command completed\x1b[0m # {}",
        timestamp_local
    );
    eprintln!("  Exit code: {:?}", status.and_then(|s| s.code()));
    eprintln!(
        "  Duration: {:.2?}",
        Duration::from_millis(duration_ms as u64)
    );
    eprintln!("  Copied to clipboard!");

    Ok(())
}

fn is_shell_builtin_or_alias(command: &str) -> bool {
    let shell_constructs = [
        "&&", "||", "|", ">", ">>", "<", ";", "&", "$", "`", "(", ")",
    ];
    for construct in shell_constructs {
        if command.contains(construct) {
            return true;
        }
    }
    false
}

fn newest_first(mut entries: Vec<String>) -> Vec<String> {
    entries.reverse();
    entries
}

fn read_shell_history_merged() -> Result<Vec<String>> {
    // The history file is append-only (oldest first), while the editor's
    // history index 0 represents the most recent command.
    let mut lines: Vec<String> = newest_first(read_wcc_history());

    let home = std::env::var("HOME").context("HOME not set")?;
    let candidates = [
        format!("{home}/.zsh_history"),
        format!("{home}/.bash_history"),
        format!("{home}/.local/share/fish/fish_history"),
    ];

    for path in candidates {
        let p = Path::new(&path);
        if !p.exists() {
            continue;
        }

        // An unreadable optional shell history must not hide wcc's own history.
        let Ok(content) = fs::read_to_string(p) else { continue; };

        let mut new_lines: Vec<String> = if path.ends_with(".zsh_history") {
            content
                .lines()
                .filter_map(|line| {
                    line.split_once(';')
                        .map(|(_, cmd)| cmd.trim().to_string())
                })
                .filter(|s| !s.is_empty())
                .collect()
        } else if path.ends_with("fish_history") {
            content
                .lines()
                .filter_map(|line| line.strip_prefix("- cmd: ").map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            content
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        };

        new_lines.reverse();
        lines.extend(new_lines);
    }

    let mut seen = HashSet::new();
    lines.retain(|cmd| seen.insert(cmd.clone()));

    Ok(lines)
}

// A built-in multiline editor is used when `wcc` has no arguments.
// We deliberately avoid readline/fzf as they cannot conveniently edit whole scripts.
struct CommandEditor {
    lines: Vec<String>,
    row: usize,
    col: usize, // character index, not byte index
    scroll: usize,
}

impl CommandEditor {
    fn new(text: &str) -> Self {
        Self {
            lines: text.split('\n').map(str::to_owned).collect(),
            row: 0,
            col: 0,
            scroll: 0,
        }
    }

    fn content(&self) -> String {
        self.lines.join("\n")
    }

    fn byte_index(s: &str, char_index: usize) -> usize {
        s.char_indices().nth(char_index).map(|(index, _)| index).unwrap_or(s.len())
    }

    fn line_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    fn insert(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' { self.newline(); }
            else if ch != '\r' && ch != '\0' && (ch == '\t' || !ch.is_control()) {
                let byte = Self::byte_index(&self.lines[self.row], self.col);
                self.lines[self.row].insert(byte, ch);
                self.col += 1;
            }
        }
    }

    fn newline(&mut self) {
        let byte = Self::byte_index(&self.lines[self.row], self.col);
        let rest = self.lines[self.row].split_off(byte);
        self.row += 1;
        self.lines.insert(self.row, rest);
        self.col = 0;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let byte = Self::byte_index(&self.lines[self.row], self.col - 1);
            self.lines[self.row].remove(byte);
            self.col -= 1;
        } else if self.row > 0 {
            let rest = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&rest);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let byte = Self::byte_index(&self.lines[self.row], self.col);
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let rest = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&rest);
        }
    }

    fn move_row(&mut self, offset: isize) {
        self.row = (self.row as isize + offset).clamp(0, self.lines.len() as isize - 1) as usize;
        self.col = self.col.min(self.line_len());
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}

// Terminal key bindings, independent of Zellij's keymap. "none" disables a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorKey { code: KeyCode, modifiers: KeyModifiers }

fn parse_editor_key(input: &str) -> Result<Option<EditorKey>> {
    let input = input.trim();
    if input.eq_ignore_ascii_case("none") || input.is_empty() { return Ok(None); }
    let pieces: Vec<_> = input.split('+').map(str::trim).collect();
    let mut modifiers = KeyModifiers::NONE;
    for token in &pieces[..pieces.len()-1] {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            other => anyhow::bail!("invalid modifier '{other}' in key binding '{input}'"),
        }
    }
    let last = pieces[pieces.len()-1].to_ascii_lowercase();
    let code = match last.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "up" => KeyCode::Up, "down" => KeyCode::Down,
        "left" => KeyCode::Left, "right" => KeyCode::Right,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab, "backspace" => KeyCode::Backspace,
        "home" => KeyCode::Home, "end" => KeyCode::End,
        "insert" => KeyCode::Insert, "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        _ if last.starts_with('f') && last[1..].parse::<u8>().is_ok() => {
            let n = last[1..].parse::<u8>()?;
            anyhow::ensure!((1..=24).contains(&n), "invalid function key: {input}");
            KeyCode::F(n)
        }
        _ if last.chars().count() == 1 => KeyCode::Char(last.chars().next().unwrap()),
        _ => anyhow::bail!("unsupported key binding: '{input}'"),
    };
    Ok(Some(EditorKey { code, modifiers }))
}

fn editor_key_matches(binding: &Option<EditorKey>, key: &crossterm::event::KeyEvent) -> bool {
    binding.as_ref().is_some_and(|bind| {
        bind.code == key.code && bind.modifiers ==
            (key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT))
    })
}

// Lightweight Bash lexical highlighting. No parser subprocess, startup download or runtime
// dependency; strings/comments/operators/variables/keywords remain readable while editing.
fn bash_highlight_line(text: &str, enabled: bool) -> Line<'static> {
    if !enabled { return Line::from(text.to_owned()); }
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut word_start = true;
    while i < chars.len() {
        let start = i;
        let ch = chars[i];
        let color = if ch == '#' && (i == 0 || chars[i-1].is_whitespace() || ";|&(".contains(chars[i-1])) {
            i = chars.len(); Color::DarkGray
        } else if ch == '\'' || ch == '"' {
            let quote = ch; i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && quote == '"' && i+1 < chars.len() { i += 2; continue; }
                if chars[i] == quote { i += 1; break; }
                i += 1;
            }
            word_start = false; Color::Green
        } else if ch == '$' {
            i += 1;
            if i < chars.len() && (chars[i] == '{' || chars[i] == '(') {
                let opening = chars[i]; let closing = if opening == '{' { '}' } else { ')' };
                i += 1;
                let mut depth = 1;
                while i < chars.len() && depth > 0 {
                    if chars[i] == opening { depth += 1; }
                    if chars[i] == closing { depth -= 1; }
                    i += 1;
                }
            } else {
                while i < chars.len() && (chars[i].is_alphanumeric() || "_@?#*!-$".contains(chars[i])) { i += 1; }
            }
            word_start = false; Color::Yellow
        } else if "|&;<>".contains(ch) {
            while i < chars.len() && "|&;<>".contains(chars[i]) { i += 1; }
            word_start = true; Color::Magenta
        } else if ch.is_whitespace() {
            while i < chars.len() && chars[i].is_whitespace() { i += 1; }
            Color::Reset
        } else {
            while i < chars.len() && !chars[i].is_whitespace() && !"'\"$|&;<>".contains(chars[i]) { i += 1; }
            let word: String = chars[start..i].iter().collect();
            let keyword = matches!(word.as_str(), "if"|"then"|"else"|"elif"|"fi"|"for"|"while"|"until"|"do"|"done"|"case"|"esac"|"in"|"function"|"select"|"time"|"return"|"export"|"local");
            let command = word_start;
            word_start = false;
            if keyword { Color::Magenta }
            else if word.starts_with('-') { Color::Yellow }
            else if word.contains('=') && !word.starts_with('=') { Color::LightBlue }
            else if command { Color::Cyan } else { Color::Reset }
        };
        spans.push(Span::styled(chars[start..i].iter().collect::<String>(), Style::default().fg(color)));
    }
    Line::from(spans)
}

fn prompt_multiline_command() -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("interactive wcc requires a terminal; use wcc 'script' or pipe a script into wcc -");
    }

    let config = load_unified_config()?;
    let run_key = parse_editor_key(&config.wcc.editor_run_key)?;
    let run_alt_key = parse_editor_key(&config.wcc.editor_run_alt_key)?;
    let history_prev_key = parse_editor_key(&config.wcc.editor_history_prev_key)?;
    let history_next_key = parse_editor_key(&config.wcc.editor_history_next_key)?;
    let history = read_shell_history_merged().unwrap_or_else(|err| {
        eprintln!("wcc: unable to read shell history: {err:#}");
        newest_first(read_wcc_history())
    });
    let mut editor = CommandEditor::new("");
    let mut history_index: Option<usize> = None;
    let mut draft = String::new();
    let mut editor_message = String::new();
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    loop {
        terminal.draw(|frame| {
            let area = frame.size();
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Min(3), Constraint::Length(2)])
                .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("wcc ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::raw("multiline shell command (bash, pipefail enabled)"),
                ])),
                layout[0],
            );
            let visible_height = layout[1].height.saturating_sub(2).max(1) as usize;
            if editor.row < editor.scroll {
                editor.scroll = editor.row;
            }
            if editor.row >= editor.scroll + visible_height {
                editor.scroll = editor.row + 1 - visible_height;
            }
            let lines: Vec<Line> = editor.lines.iter().enumerate().skip(editor.scroll)
                .take(visible_height)
                .map(|(i, text)| {
                    let mut spans = vec![Span::styled(format!("{:>3}  ", i + 1), Style::default().fg(Color::DarkGray))];
                    spans.extend(bash_highlight_line(text, config.wcc.editor_syntax_highlighting).spans);
                    Line::from(spans)
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::default().title(" Script ").borders(Borders::ALL)),
                layout[1],
            );
            let x = layout[1].x + 1 + 5 + editor.lines[editor.row].chars().take(editor.col)
                .map(|ch| if ch == '\t' { 1 } else { unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) })
                .sum::<usize>() as u16;
            let y = layout[1].y + 1 + (editor.row - editor.scroll) as u16;
            frame.set_cursor(x.min(layout[1].right().saturating_sub(2)), y);
            frame.render_widget(
                Paragraph::new(format!("Enter: newline  {} / {}: run  {} / {}: history ({})  Esc: cancel{}",
                    config.wcc.editor_run_key, config.wcc.editor_run_alt_key,
                    config.wcc.editor_history_prev_key, config.wcc.editor_history_next_key,
                    history.len(),
                    if editor_message.is_empty() { String::new() } else { format!("  |  {editor_message}") })),
                layout[2],
            );
        })?;

        match event::read()? {
            Event::Paste(text) => { editor.insert(&text); history_index = None; editor_message.clear(); }
            Event::Key(key) if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                if editor_key_matches(&run_key, &key) || editor_key_matches(&run_alt_key, &key) {
                    if editor.content().trim().is_empty() {
                        editor_message = "Nothing to run; enter a command or recall history".into();
                        continue;
                    }
                    break;
                }
                if key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && ctrl) { return Ok(None); }
                if editor_key_matches(&history_prev_key, &key) {
                        if history_index.is_none() { draft = editor.content(); }
                        let index = history_index.map_or(0, |i| i.saturating_add(1));
                        if let Some(command) = history.get(index) {
                            history_index = Some(index);
                            editor = CommandEditor::new(command);
                            editor_message = format!("History {}/{}", index + 1, history.len());
                        } else if history.is_empty() {
                            editor_message = "History empty: run a command first".into();
                        } else {
                            editor_message = "Oldest history entry reached".into();
                        }
                } else if editor_key_matches(&history_next_key, &key) {
                        match history_index {
                            Some(0) => { editor = CommandEditor::new(&draft); history_index = None; editor_message.clear(); }
                            Some(i) => {
                                let index = i - 1;
                                editor = CommandEditor::new(&history[index]);
                                history_index = Some(index);
                                editor_message = format!("History {}/{}", index + 1, history.len());
                            }
                            None => {}
                        }
                } else {
                match key.code {
                    KeyCode::Enter => editor.newline(),
                    KeyCode::Backspace => editor.backspace(),
                    KeyCode::Delete => editor.delete(),
                    KeyCode::Left => {
                        if editor.col > 0 { editor.col -= 1; }
                        else if editor.row > 0 { editor.row -= 1; editor.col = editor.line_len(); }
                    }
                    KeyCode::Right => {
                        if editor.col < editor.line_len() { editor.col += 1; }
                        else if editor.row + 1 < editor.lines.len() { editor.row += 1; editor.col = 0; }
                    }
                    KeyCode::Up => editor.move_row(-1),
                    KeyCode::Down => editor.move_row(1),
                    KeyCode::Home => editor.col = 0,
                    KeyCode::End => editor.col = editor.line_len(),
                    KeyCode::Tab => editor.insert("    "),
                    KeyCode::Char('a') if ctrl => editor.col = 0,
                    KeyCode::Char('e') if ctrl => editor.col = editor.line_len(),
                    KeyCode::Char(ch) if !ctrl && !alt => editor.insert(&ch.to_string()),
                    _ => {}
                }
                }
            }
            _ => {}
        }
    }
    let script = editor.content();
    Ok((!script.trim().is_empty()).then_some(script))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Mode::Build {
            release,
            debug,
            args,
        }) => {
            run_cargo_build(release, debug, args)?;
        }
        Some(Mode::Run {
            release,
            debug,
            args,
        }) => {
            run_cargo_run(release, debug, args)?;
        }
        Some(Mode::Config {
            set_cargo_mode,
            show,
            init,
        }) => {
            if init {
                init_config()?;
            }
            if let Some(ref mode) = set_cargo_mode {
                let mode_lower = mode.to_lowercase();
                if mode_lower == "debug" || mode_lower == "release" {
                    update_cargo_mode(&mode_lower)?;
                } else {
                    eprintln!(
                        "\x1b[31mError: Invalid mode '{}'. Use 'debug' or 'release'.\x1b[0m",
                        mode
                    );
                    std::process::exit(1);
                }
            }
            if show {
                show_config()?;
            }
            if set_cargo_mode.is_none() && !show && !init {
                println!("\x1b[36m🔧 wcc config commands:\x1b[0m");
                println!("  wcc config --init                     Create default config file");
                println!("  wcc config --show                    Show current configuration");
                println!("  wcc config --set-cargo-mode debug    Set default cargo mode to debug");
                println!(
                    "  wcc config --set-cargo-mode release  Set default cargo mode to release"
                );
            }
        }
        None => {
            if cli.cmd.is_empty() {
                if !io::stdin().is_terminal() {
                    let mut script = String::new();
                    io::stdin().read_to_string(&mut script)?;
                    if !script.trim().is_empty() { run_shell_command(&script)?; }
                } else if let Some(command_str) = prompt_multiline_command()? {
                    run_shell_command(&command_str)?;
                }
            } else if cli.cmd.len() == 1 && cli.cmd[0] == "-" {
                let mut script = String::new();
                io::stdin().read_to_string(&mut script)?;
                anyhow::ensure!(!script.trim().is_empty(), "no script supplied on stdin");
                run_shell_command(&script)?;
            } else {
                let command_str = cli.cmd.join(" ");
                if is_shell_builtin_or_alias(&command_str) {
                    run_shell_command(&command_str)?;
                } else {
                    run_watch_command(cli.cmd)?;
                }
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod multiline_editor_tests {
    #[test]
    fn shortcut_override_parsing() {
        let key = super::parse_editor_key("Ctrl+Enter").unwrap().unwrap();
        assert_eq!(key.code, crossterm::event::KeyCode::Enter);
        assert_eq!(key.modifiers, crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(super::parse_editor_key("F7").unwrap().unwrap().code, crossterm::event::KeyCode::F(7));
        assert!(super::parse_editor_key("none").unwrap().is_none());
        assert!(super::parse_editor_key("Ctrl+Bogus").is_err());
    }

    #[test]
    fn shell_highlight_preserves_text() {
        for line in ["echo $HOME | grep foo # hello", "if test -f x; then echo 'yes'; fi", "printf \"%s\" \"hello\""] {
            let rendered = super::bash_highlight_line(line, true);
            let reconstructed: String = rendered.spans.iter().map(|span| span.content.as_ref()).collect();
            assert_eq!(reconstructed, line);
        }
    }

    use super::*;

    #[test]
    fn history_entries_are_loaded_newest_first() {
        // Wcc stores JSON strings, including whole multiline scripts, one per line.
        let old = "echo first".to_string();
        let recent = "echo second\nprintf ok | cat".to_string();
        let stored = vec![old.clone(), recent.clone()];
        let newest_first = super::newest_first(stored);
        assert_eq!(newest_first[0], recent);
        assert_eq!(newest_first[1], old);
    }

    #[test]
    fn multiline_insert_backspace_join_and_delete() {
        let mut editor = CommandEditor::new("");
        editor.insert("echo café\nprintf 'hello' | wc -c");
        assert_eq!(editor.lines.len(), 2);
        assert_eq!(editor.content(), "echo café\nprintf 'hello' | wc -c");
        editor.row = 1;
        editor.col = 0;
        editor.backspace();
        assert_eq!(editor.content(), "echo caféprintf 'hello' | wc -c");
        editor.col = 0;
        editor.delete();
        assert!(editor.content().starts_with("cho café"));
    }
}
