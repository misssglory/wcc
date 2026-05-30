use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
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

fn set_clipboard(command: &[String], stdout: &str, stderr: &str) -> Result<()> {
    use chrono::Local;
    let timestamp = Local::now().format("%H:%M:%S %d.%m.%Y");
    let payload = format!(
        "$ {} # {}\n\n[stdout]\n{}\n\n[stderr]\n{}",
        command.join(" "),
        timestamp,
        stdout,
        stderr
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
            let _ = writeln!(file, "{}", cmd);
        }
    }
}

fn read_wcc_history() -> Vec<String> {
    if let Some(path) = wcc_history_path() {
        if let Ok(content) = fs::read_to_string(path) {
            return content
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
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

    let mut child = Command::new("sh")
        .arg("-c")
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

    let _ = set_clipboard(&vec![command.to_string()], &clean_stdout, &clean_stderr);

    if status.success() {
        eprintln!(
            "\n\x1b[1;32m✓ Command completed\x1b[0m # {}",
            timestamp_local
        );
    } else {
        eprintln!(
            "\n\x1b[1;31m✗ Command failed with exit code: {:?}\x1b[0m # {}",
            status.code(),
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

    let _ = set_clipboard(&command, &out.content, &err.content);

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

fn read_shell_history_merged() -> Result<Vec<String>> {
    let mut lines: Vec<String> = read_wcc_history();

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

        let content =
            fs::read_to_string(p).with_context(|| format!("failed to read history file: {path}"))?;

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

fn prompt_command_from_history() -> Result<Option<String>> {
    let history = read_shell_history_merged().unwrap_or_default();

    let mut child = Command::new("fzf")
        .args([
            "--scheme=history",
            "--height=40%",
            "--layout=reverse",
            "--border",
            "--prompt=command> ",
            "--print-query",
            "--bind=enter:replace-query+print-query",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn fzf")?;

    if let Some(mut stdin) = child.stdin.take() {
        for line in &history {
            writeln!(stdin, "{line}")?;
        }
    }

    let output = child.wait_with_output()?;

    if output.status.code() == Some(130) {
        return Ok(None);
    }

    let out = String::from_utf8(output.stdout)?;
    let command = out
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string();

    if command.is_empty() {
        Ok(None)
    } else {
        Ok(Some(command))
    }
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
                if let Some(command_str) = prompt_command_from_history()? {
                    run_shell_command(&command_str)?;
                } else {
                    return Ok(());
                }
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