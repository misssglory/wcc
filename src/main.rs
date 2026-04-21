use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Watch command output, store compact history, and copy command/stdout/stderr to clipboard"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Mode>,

    /// Command to run: wcc -- cmd args...
    #[arg(trailing_var_arg = true)]
    cmd: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Mode {
    /// Ratatui interface with history and live stats
    Gui {
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },
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

#[derive(Debug, Clone)]
struct StreamTail {
    content: String,
    stats: TextStats,
}

impl StreamTail {
    fn new() -> Self {
        Self {
            content: String::new(),
            stats: TextStats::default(),
        }
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

#[derive(Debug)]
struct TuiState {
    history: Vec<HistoryEntry>,
    selected: usize,
}

fn trim_tail(input: &str, retain: &RetainPolicy) -> String {
    match retain.mode {
        RetainMode::Bytes => {
            let bytes = input.as_bytes();
            if bytes.len() <= retain.limit {
                return input.to_string();
            }
            String::from_utf8_lossy(&bytes[bytes.len() - retain.limit..]).to_string()
        }
        RetainMode::Chars => {
            let chars: Vec<char> = input.chars().collect();
            if chars.len() <= retain.limit {
                return input.to_string();
            }
            chars[chars.len() - retain.limit..].iter().collect()
        }
        RetainMode::Lines => {
            let lines: Vec<&str> = input.lines().collect();
            if lines.len() <= retain.limit {
                return input.to_string();
            }
            let mut s = lines[lines.len() - retain.limit..].join("\n");
            if input.ends_with('\n') {
                s.push('\n');
            }
            s
        }
        RetainMode::Words => {
            let words: Vec<&str> = input.split_whitespace().collect();
            if words.len() <= retain.limit {
                return input.to_string();
            }
            words[words.len() - retain.limit..].join(" ")
        }
    }
}

fn load_config() -> Result<Config> {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("wcc/config.toml");

    if path.exists() {
        let data =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&data).context("parsing config")?)
    } else {
        let cfg = Config::default();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        Ok(cfg)
    }
}

fn compress_if_needed(s: &str, threshold: usize) -> Result<Option<String>> {
    if s.as_bytes().len() < threshold {
        return Ok(None);
    }

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
    if !dir.exists() {
        return Ok(vec![]);
    }

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
        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&txt) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

fn history_path_for(dir: &Path, entry: &HistoryEntry) -> PathBuf {
    dir.join(format!(
        "{}-{}.json",
        entry.timestamp.format("%Y%m%dT%H%M%SZ"),
        entry.id
    ))
}

fn save_history(cfg: &Config, entry: &HistoryEntry) -> Result<()> {
    fs::create_dir_all(&cfg.history_dir)?;
    let path = history_path_for(&cfg.history_dir, entry);
    fs::write(path, serde_json::to_vec_pretty(entry)?)?;
    Ok(())
}

fn set_clipboard(command: &[String], stdout: &str, stderr: &str) -> Result<()> {
    let mut cb = Clipboard::new().context("clipboard init failed")?;
    let payload = format!(
        "$ {}\n\n[stdout]\n{}\n\n[stderr]\n{}",
        command.join(" "),
        stdout,
        stderr
    );
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
                    let _ = tx.send(if is_err {
                        Msg::Stderr(s)
                    } else {
                        Msg::Stdout(s)
                    });
                }
                Err(_) => break,
            }
        }
    });
}

fn run_command(command: Vec<String>, cfg: &Config, tui: bool) -> Result<HistoryEntry> {
    anyhow::ensure!(
        !command.is_empty(),
        "usage: wcc -- cmd args   or   wcc gui -- cmd args"
    );

    let term = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&term))?;
    flag::register(SIGTERM, Arc::clone(&term))?;

    let started = Instant::now();
    let timestamp = Utc::now();
    let id = timestamp.timestamp_millis().to_string();

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {}", command[0]))?;

    let stdout = child.stdout.take().context("missing child stdout")?;
    let stderr = child.stderr.take().context("missing child stderr")?;

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, tx.clone(), false);
    spawn_reader(stderr, tx, true);

    let mut out = StreamTail::new();
    let mut err = StreamTail::new();

    if tui {
        run_tui_loop(&command, &mut child, rx, &mut out, &mut err, started, &term, cfg)?;
    } else {
        run_cli_loop(&mut child, rx, &mut out, &mut err, &term, &cfg.retain)?;
    }

    let status = child.wait().ok();
    let killed = term.load(Ordering::Relaxed)
        && status.as_ref().and_then(|s| s.code()).is_none();
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
    Ok(entry)
}

fn run_cli_loop(
    child: &mut Child,
    rx: Receiver<Msg>,
    out: &mut StreamTail,
    err: &mut StreamTail,
    term: &Arc<AtomicBool>,
    retain: &RetainPolicy,
) -> Result<()> {
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Stdout(s) => {
                    print!("{s}");
                    io::stdout().flush()?;
                    out.push(&s, retain);
                }
                Msg::Stderr(s) => {
                    eprint!("{s}");
                    io::stderr().flush()?;
                    err.push(&s, retain);
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
                        print!("{s}");
                        io::stdout().flush()?;
                        out.push(&s, retain);
                    }
                    Msg::Stderr(s) => {
                        eprint!("{s}");
                        io::stderr().flush()?;
                        err.push(&s, retain);
                    }
                }
            }
            break;
        }

        thread::sleep(Duration::from_millis(30));
    }

    Ok(())
}

fn run_tui_loop(
    command: &[String],
    child: &mut Child,
    rx: Receiver<Msg>,
    out: &mut StreamTail,
    err: &mut StreamTail,
    started: Instant,
    term: &Arc<AtomicBool>,
    cfg: &Config,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = TuiState {
        history: load_history(&cfg.history_dir)?,
        selected: 0,
    };

    let result = loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Stdout(s) => out.push(&s, &cfg.retain),
                Msg::Stderr(s) => err.push(&s, &cfg.retain),
            }
        }

        terminal.draw(|f| draw_ui(f, &state, command, out, err, started))?;

        if term.load(Ordering::Relaxed) {
            let _ = child.kill();
            break Ok(());
        }

        if child.try_wait()?.is_some() {
            break Ok(());
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') => {
                        let _ = child.kill();
                        break Ok(());
                    }
                    KeyCode::Down => {
                        if state.selected + 1 < state.history.len() {
                            state.selected += 1;
                        }
                    }
                    KeyCode::Up => {
                        if state.selected > 0 {
                            state.selected -= 1;
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some(entry) = state.history.get(state.selected) {
                            let path = history_path_for(&cfg.history_dir, entry);
                            let _ = fs::remove_file(path);
                            state.history = load_history(&cfg.history_dir)?;
                            if state.history.is_empty() {
                                state.selected = 0;
                            } else {
                                state.selected =
                                    state.selected.min(state.history.len().saturating_sub(1));
                            }
                        }
                    }
                    KeyCode::Char('c') => {
                        if let Some(entry) = state.history.get(state.selected) {
                            let stdout = entry
                                .stdout_compressed_b64
                                .as_ref()
                                .and_then(|s| decompress_b64(s).ok())
                                .unwrap_or_else(|| entry.stdout_tail.clone());

                            let stderr = entry
                                .stderr_compressed_b64
                                .as_ref()
                                .and_then(|s| decompress_b64(s).ok())
                                .unwrap_or_else(|| entry.stderr_tail.clone());

                            let _ = set_clipboard(&entry.command, &stdout, &stderr);
                        }
                    }
                    _ => {}
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn draw_ui(
    f: &mut Frame,
    state: &TuiState,
    command: &[String],
    out: &StreamTail,
    err: &StreamTail,
    started: Instant,
) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Min(10),
            Constraint::Length(10),
        ])
        .split(f.size());

    let header = Paragraph::new(format!(
        "running: {} | q quit | d delete | c copy | elapsed: {:.1?}",
        command.join(" "),
        started.elapsed()
    ))
    .block(Block::default().borders(Borders::ALL).title("wcc --gui"));
    f.render_widget(header, layout[0]);

    let rows = vec![
        Row::new(vec![
            Cell::from("stdout"),
            Cell::from(out.stats.lines.to_string()),
            Cell::from(out.stats.words.to_string()),
            Cell::from(out.stats.chars.to_string()),
            Cell::from(out.stats.bytes.to_string()),
        ]),
        Row::new(vec![
            Cell::from("stderr"),
            Cell::from(err.stats.lines.to_string()),
            Cell::from(err.stats.words.to_string()),
            Cell::from(err.stats.chars.to_string()),
            Cell::from(err.stats.bytes.to_string()),
        ]),
    ];

    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
        ],
    )
    .header(Row::new(vec![
        Cell::from("stream"),
        Cell::from("lines"),
        Cell::from("words"),
        Cell::from("chars"),
        Cell::from("bytes"),
    ]))
    .block(Block::default().borders(Borders::ALL).title("live stats"));
    f.render_widget(table, layout[1]);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(layout[2]);

    let hist_rows: Vec<Row> = state
        .history
        .iter()
        .enumerate()
        .take(200)
        .map(|(i, h)| {
            let style = if i == state.selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(h.timestamp.format("%m-%d %H:%M:%S").to_string()),
                Cell::from(h.command.join(" ")),
                Cell::from(format!("{} ms", h.duration_ms)),
            ])
            .style(style)
        })
        .collect();

    let hist = Table::new(
        hist_rows,
        [
            Constraint::Length(15),
            Constraint::Percentage(60),
            Constraint::Length(12),
        ],
    )
    .block(Block::default().borders(Borders::ALL).title("history"));
    f.render_widget(hist, mid[0]);

    let current = Paragraph::new(format!(
        "[stdout]\n{}\n[stderr]\n{}",
        out.content, err.content
    ))
    .wrap(Wrap { trim: false })
    .block(Block::default().borders(Borders::ALL).title("current tail"));
    f.render_widget(current, mid[1]);

    let selected_text = state
        .history
        .get(state.selected)
        .map(|h| {
            format!(
                "command: {}\nexit: {:?} killed: {}\nstdout lines:{} words:{} chars:{} bytes:{}\nstderr lines:{} words:{} chars:{} bytes:{}\n\nstdout tail:\n{}\n\nstderr tail:\n{}",
                h.command.join(" "),
                h.exit_code,
                h.killed,
                h.stdout_stats.lines,
                h.stdout_stats.words,
                h.stdout_stats.chars,
                h.stdout_stats.bytes,
                h.stderr_stats.lines,
                h.stderr_stats.words,
                h.stderr_stats.chars,
                h.stderr_stats.bytes,
                h.stdout_tail,
                h.stderr_tail
            )
        })
        .unwrap_or_else(|| "no history".to_string());

    let details = Paragraph::new(selected_text)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("selected history"));
    f.render_widget(Clear, layout[3]);
    f.render_widget(details, layout[3]);
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = load_config()?;

    match cli.command {
        Some(Mode::Gui { cmd }) => {
            let cmd = if cmd.is_empty() { cli.cmd } else { cmd };
            if cmd.is_empty() {
                return Err(WccError::NoCommand.into());
            }
            let entry = run_command(cmd, &cfg, true)?;
            eprintln!(
                "copied to clipboard after completion, exit={:?}",
                entry.exit_code
            );
        }
        None => {
            if cli.cmd.is_empty() {
                return Err(WccError::NoCommand.into());
            }
            let entry = run_command(cli.cmd, &cfg, false)?;
            eprintln!(
                "copied to clipboard after completion, exit={:?}",
                entry.exit_code
            );
        }
    }

    Ok(())
}
