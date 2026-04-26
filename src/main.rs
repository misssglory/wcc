// src/main.rs
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
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

fn spawn_stdin_forwarder(mut child_stdin: std::process::ChildStdin) {
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

fn init_config() -> Result<()> {
    use wcc::UnifiedConfig;

    let config_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wcc/config.toml");

    if config_path.exists() {
        println!(
            "\x1b[33m⚠ Config already exists at: {}\x1b[0m",
            config_path.display()
        );
        print!("❓ Overwrite? (y/n): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            println!("❌ Aborted.");
            return Ok(());
        }
    }

    let config = UnifiedConfig::default();
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Create a pretty-printed TOML string with all defaults
    let toml_content = format!(
        r#"[wcc]
default_cargo_mode = "debug"
time_format = "%H:%M:%S %d.%m.%Y"

[wcn]
show_time_in_header = true
use_file_modification_time = true

[wcp]
auto_backup = true

[wcl]
max_file_size_kb = 50
max_file_words_to_copy = 10000
skip_patterns = [
    ".o", ".pyc", ".pyo", ".so", ".dll", ".dylib", ".exe",
    ".class", ".jar", ".war", ".ear", ".zip", ".tar", ".gz",
    ".bz2", ".xz", ".7z", ".rar", ".png", ".jpg", ".jpeg",
    ".gif", ".bmp", ".ico", ".mp3", ".mp4", ".avi", ".mov",
    ".pdf", ".doc", ".docx", ".bkp",
]
skip_dirs = [
    "target", "node_modules", ".git", ".svn", ".hg",
    "build", "dist", "__pycache__", ".cache", ".cargo",
    ".idea", ".vscode",
]
show_empty_files = false
show_stats_per_file = true
show_function_details = true
show_class_details = true
show_usage_stats = false
max_files_to_display = 500
min_function_lines = 1
min_class_lines = 1
max_functions_per_file = 100
max_classes_per_file = 50
parallel_processing = true
max_threads = 8
copy_file_contents = true

[wcf]
auto_format = true
show_buffer_preview = true
"#
    );

    fs::write(&config_path, toml_content)?;

    println!(
        "\x1b[32m✓ Created default config at: {}\x1b[0m",
        config_path.display()
    );
    println!("\nDefault configuration includes:");
    println!("  [wcc] - cargo wrapper settings");
    println!("  [wcn] - file copy settings");
    println!("  [wcp] - paste settings");
    println!("  [wcl] - analyzer settings with skip patterns");
    println!("  [wcf] - function replacement settings");

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
                return Err(WccError::NoCommand.into());
            }
            let command_str = cli.cmd.join(" ");

            if is_shell_builtin_or_alias(&command_str) {
                run_shell_command(&command_str)?;
            } else {
                run_watch_command(cli.cmd)?;
            }
        }
    }
    Ok(())
}
