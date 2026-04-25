// src/main.rs
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use arboard::Clipboard;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(author, version, about = "Watch command output, keep history, and copy command/stdout/stderr to clipboard")]
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
        /// Build with release optimizations
        #[arg(short, long)]
        release: bool,
        
        /// Build with debug (default)
        #[arg(long)]
        debug: bool,
        
        /// Additional cargo build arguments
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    
    /// Run a binary (wraps cargo run)
    Run {
        /// Run with release optimizations
        #[arg(short, long)]
        release: bool,
        
        /// Run with debug (default)
        #[arg(long)]
        debug: bool,
        
        /// Additional cargo run arguments
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum BuildMode {
    Debug,
    Release,
}

impl Default for BuildMode {
    fn default() -> Self {
        Self::Debug
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CargoConfig {
    default_mode: BuildMode,
}

impl Default for CargoConfig {
    fn default() -> Self {
        Self {
            default_mode: BuildMode::Debug,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    history_dir: PathBuf,
    compress_above_bytes: usize,
    retain: RetainPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetainPolicy {
    mode: RetainMode,
    limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetainMode {
    Lines,
    Words,
    Chars,
    Bytes,
}

impl Default for Config {
    fn default() -> Self {
        let mut dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        dir.push(".local/state/wcc/history");
        Self {
            history_dir: dir,
            compress_above_bytes: 16 * 1024,
            retain: RetainPolicy {
                mode: RetainMode::Bytes,
                limit: 128 * 1024,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TextStats {
    lines: usize,
    words: usize,
    chars: usize,
    bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    id: String,
    timestamp: DateTime<Utc>,
    command: Vec<String>,
    exit_code: Option<i32>,
    duration_ms: u128,
    killed: bool,
    stdout_stats: TextStats,
    stderr_stats: TextStats,
    stdout_tail: String,
    stderr_tail: String,
    stdout_compressed_b64: Option<String>,
    stderr_compressed_b64: Option<String>,
}

#[derive(Debug)]
struct StreamTail {
    content: String,
    stats: TextStats,
}

impl StreamTail {
    fn new() -> Self {
        Self { content: String::new(), stats: TextStats::default() }
    }

    fn push(&mut self, chunk: &str, retain: &RetainPolicy) {
        self.stats.bytes += chunk.as_bytes().len();
        self.stats.chars += chunk.chars().count();
        self.stats.words += chunk.split_whitespace().count();
        self.stats.lines += chunk.bytes().filter(|b| *b == b'\n').count();
        self.content.push_str(chunk);
        self.content = trim_tail(&self.content, retain);
    }
}

#[derive(Debug)]
enum Msg {
    Stdout(String),
    Stderr(String),
}

#[derive(Error, Debug)]
enum WccError {
    #[error("no command specified")]
    NoCommand,
}

fn trim_tail(input: &str, retain: &RetainPolicy) -> String {
    match retain.mode {
        RetainMode::Bytes => {
            let bytes = input.as_bytes();
            if bytes.len() <= retain.limit { return input.to_string(); }
            String::from_utf8_lossy(&bytes[bytes.len() - retain.limit..]).to_string()
        }
        RetainMode::Chars => {
            let chars: Vec<char> = input.chars().collect();
            if chars.len() <= retain.limit { return input.to_string(); }
            chars[chars.len() - retain.limit..].iter().collect()
        }
        RetainMode::Lines => {
            let lines: Vec<&str> = input.lines().collect();
            if lines.len() <= retain.limit { return input.to_string(); }
            let mut s = lines[lines.len() - retain.limit..].join("\n");
            if input.ends_with('\n') { s.push('\n'); }
            s
        }
        RetainMode::Words => {
            let words: Vec<&str> = input.split_whitespace().collect();
            if words.len() <= retain.limit { return input.to_string(); }
            words[words.len() - retain.limit..].join(" ")
        }
    }
}

fn load_cargo_config() -> Result<CargoConfig> {
    let mut path = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push(".cargo/wcc-config.toml");
    
    if path.exists() {
        let data = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&data).context("parsing cargo config")?)
    } else {
        let cfg = CargoConfig::default();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        eprintln!("\x1b[36m✓ Created default cargo config at: {}\x1b[0m", path.display());
        Ok(cfg)
    }
}

fn load_config() -> Result<Config> {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("wcc/config.toml");
    if path.exists() {
        let data = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&data).context("parsing config")?)
    } else {
        let cfg = Config::default();
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
        fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        Ok(cfg)
    }
}

fn compress_if_needed(s: &str, threshold: usize) -> Result<Option<String>> {
    if s.as_bytes().len() < threshold { return Ok(None); }
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(s.as_bytes())?;
    Ok(Some(B64.encode(enc.finish()?)))
}

fn decompress_b64(s: &str) -> Result<String> {
    let bytes = B64.decode(s)?;
    let mut d = GzDecoder::new(&bytes[..]);
    let mut out = String::new();
    d.read_to_string(&mut out)?;
    Ok(out)
}

fn history_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() { return Ok(vec![]); }
    let mut files: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    files.sort();
    files.reverse();
    Ok(files)
}

fn load_history(dir: &Path) -> Result<Vec<HistoryEntry>> {
    let mut entries = Vec::new();
    for p in history_files(dir)? {
        let txt = fs::read_to_string(&p)?;
        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&txt) { entries.push(entry); }
    }
    Ok(entries)
}

fn history_path_for(dir: &Path, entry: &HistoryEntry) -> PathBuf {
    dir.join(format!("{}-{}.json", entry.timestamp.format("%Y%m%dT%H%M%SZ"), entry.id))
}

fn save_history(cfg: &Config, entry: &HistoryEntry) -> Result<()> {
    fs::create_dir_all(&cfg.history_dir)?;
    let path = history_path_for(&cfg.history_dir, entry);
    fs::write(path, serde_json::to_vec_pretty(entry)?)?;
    Ok(())
}

fn set_clipboard(command: &[String], stdout: &str, stderr: &str) -> Result<()> {
    let payload = format!("$ {}\n\n[stdout]\n{}\n\n[stderr]\n{}", command.join(" "), stdout, stderr);
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
        loop {
            let mut buf = Vec::new();
            match br.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let s = String::from_utf8_lossy(&buf).to_string();
                    let _ = tx.send(if is_err { Msg::Stderr(s) } else { Msg::Stdout(s) });
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_stdin_forwarder(mut child_stdin: std::process::ChildStdin) {
    thread::spawn(move || {
        let mut input = io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if child_stdin.write_all(&buf[..n]).is_err() { break; }
                    if child_stdin.flush().is_err() { break; }
                }
                Err(_) => break,
            }
        }
    });
}

fn run_cargo_build(release: bool, debug: bool, args: Vec<String>) -> Result<()> {
    let cargo_config = load_cargo_config()?;
    
    // Determine build mode
    let is_release = if release {
        true
    } else if debug {
        false
    } else {
        cargo_config.default_mode == BuildMode::Release
    };
    
    let mut cargo_args = vec!["build".to_string()];
    if is_release {
        cargo_args.push("--release".to_string());
    }
    cargo_args.extend(args);
    
    eprintln!("\x1b[36m📦 Running cargo {}\x1b[0m", cargo_args.join(" "));
    
    let status = Command::new("cargo")
        .args(&cargo_args)
        .status()
        .context("Failed to run cargo build")?;
    
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    
    Ok(())
}

fn run_cargo_run(release: bool, debug: bool, args: Vec<String>) -> Result<()> {
    let cargo_config = load_cargo_config()?;
    
    // Determine build mode
    let is_release = if release {
        true
    } else if debug {
        false
    } else {
        cargo_config.default_mode == BuildMode::Release
    };
    
    let mut cargo_args = vec!["run".to_string()];
    if is_release {
        cargo_args.push("--release".to_string());
    }
    cargo_args.extend(args);
    
    eprintln!("\x1b[36m🏃 Running cargo {}\x1b[0m", cargo_args.join(" "));
    
    let status = Command::new("cargo")
        .args(&cargo_args)
        .status()
        .context("Failed to run cargo run")?;
    
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    
    Ok(())
}

fn run_watch_command(command: Vec<String>, cfg: &Config) -> Result<HistoryEntry> {
    anyhow::ensure!(!command.is_empty(), "usage: wcc -- cmd args");
    let term = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&term))?;
    flag::register(SIGTERM, Arc::clone(&term))?;

    let started = Instant::now();
    let timestamp = Utc::now();
    let id = timestamp.timestamp_millis().to_string();

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", command[0]))?;

    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take().context("missing child stderr")?;
    if let Some(stdin) = child.stdin.take() { spawn_stdin_forwarder(stdin); }

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone(), false);
    spawn_reader(stderr, tx, true);

    let mut out = StreamTail::new();
    let mut err = StreamTail::new();

    // Simple CLI loop without TUI
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Stdout(s) => { out.push(&s, &cfg.retain); print!("{s}"); io::stdout().flush()?; }
                Msg::Stderr(s) => { err.push(&s, &cfg.retain); eprint!("{s}"); io::stderr().flush()?; }
            }
        }
        if term.load(Ordering::Relaxed) { let _ = child.kill(); break; }
        if child.try_wait()?.is_some() {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Stdout(s) => { out.push(&s, &cfg.retain); print!("{s}"); io::stdout().flush()?; }
                    Msg::Stderr(s) => { err.push(&s, &cfg.retain); eprint!("{s}"); io::stderr().flush()?; }
                }
            }
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }

    let status = child.wait().ok();
    let killed = term.load(Ordering::Relaxed) && status.as_ref().and_then(|s| s.code()).is_none();
    let duration_ms = started.elapsed().as_millis();

    let _ = set_clipboard(&command, &out.content, &err.content);

    let entry = HistoryEntry {
        id,
        timestamp,
        command,
        exit_code: status.and_then(|s| s.code()),
        duration_ms,
        killed,
        stdout_stats: out.stats.clone(),
        stderr_stats: err.stats.clone(),
        stdout_tail: out.content.clone(),
        stderr_tail: err.content.clone(),
        stdout_compressed_b64: compress_if_needed(&out.content, cfg.compress_above_bytes)?,
        stderr_compressed_b64: compress_if_needed(&err.content, cfg.compress_above_bytes)?,
    };
    save_history(cfg, &entry)?;
    
    // Print final stats
    eprintln!("\n\x1b[1;32m✓ Command completed\x1b[0m");
    eprintln!("  Exit code: {:?}", entry.exit_code);
    eprintln!("  Duration: {:.2?}", Duration::from_millis(duration_ms as u64));
    eprintln!("  Copied to clipboard!");
    
    Ok(entry)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = load_config()?;
    
    match cli.command {
        Some(Mode::Build { release, debug, args }) => {
            run_cargo_build(release, debug, args)?;
        }
        Some(Mode::Run { release, debug, args }) => {
            run_cargo_run(release, debug, args)?;
        }
        None => {
            if cli.cmd.is_empty() { 
                return Err(WccError::NoCommand.into()); 
            }
            run_watch_command(cli.cmd, &cfg)?;
        }
    }
    Ok(())
}