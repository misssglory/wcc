use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arboard::Clipboard;
use clap::{ArgAction, Parser};
use flate2::read::MultiGzDecoder;
use jwalk::WalkDir;
use lz4_flex::frame::FrameDecoder;
use rayon::prelude::*;
use regex::{Captures, Regex};
use strip_ansi_escapes::strip_str;

#[path = "../common.rs"]
mod common;

type DynRead = Box<dyn Read + Send>;

#[derive(Parser, Debug)]
#[command(name = "wcm", disable_help_flag = true)]
#[command(about = "Parallel recursive text finder with decompression, coloring and clipboard output")]
struct Args {
    /// Search string
    needle: String,

    /// Root directory
    #[arg(default_value = ".")]
    dir: PathBuf,

    /// Put only last N matched lines into clipboard
    #[arg(short = 't', long = "tail")]
    tail: Option<usize>,

    /// Put only first N matched lines into clipboard
    #[arg(short = 'h', long = "head")]
    head: Option<usize>,

    /// Search only in the most recently modified file
    #[arg(long = "latest-file-only", action = ArgAction::SetTrue)]
    latest_file_only: bool,

    /// List only filenames with occurrence count and mtime, sorted by mtime ascending
    #[arg(short = 'l', long = "list-files", action = ArgAction::SetTrue)]
    list_files: bool,

    /// Include INFO lines
    #[arg(long = "info", default_value_t = true, action = ArgAction::Set)]
    info: bool,

    /// Include WARN lines
    #[arg(long = "warn", default_value_t = true, action = ArgAction::Set)]
    warn: bool,

    /// Include ERROR lines
    #[arg(long = "error", default_value_t = true, action = ArgAction::Set)]
    error: bool,

    /// Exclude INFO lines
    #[arg(long = "no-info", action = ArgAction::SetTrue)]
    no_info: bool,

    /// Exclude WARN lines
    #[arg(long = "no-warn", action = ArgAction::SetTrue)]
    no_warn: bool,

    /// Exclude ERROR lines
    #[arg(long = "no-error", action = ArgAction::SetTrue)]
    no_error: bool,

    /// Case insensitive search
    #[arg(short = 'i', long = "ignore-case", action = ArgAction::SetTrue)]
    ignore_case: bool,

    /// Print only clipboard selection to terminal instead of full matched output
    #[arg(long = "selected-only", action = ArgAction::SetTrue)]
    selected_only: bool,

    /// Long help only because -h is used for head
    #[arg(long = "help", action = ArgAction::Help, help = "Print help")]
    help: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Other,
}

#[derive(Debug, Clone)]
struct MatchLine {
    file: PathBuf,
    line_no: usize,
    raw_line: String,
}

#[derive(Debug, Clone)]
struct FileInfo {
    path: PathBuf,
    modified: SystemTime,
}

fn main() -> io::Result<()> {
    let mut args = Args::parse();

    if args.no_info {
        args.info = false;
    }
    if args.no_warn {
        args.warn = false;
    }
    if args.no_error {
        args.error = false;
    }

    let allowed_levels = build_allowed_levels(&args);

    let mut files = collect_files(&args.dir);

    if args.latest_file_only {
        files = latest_only(files);
    }

    let needle_cmp = if args.ignore_case {
        args.needle.to_lowercase()
    } else {
        args.needle.clone()
    };

    let mut matches: Vec<MatchLine> = files
        .par_iter()
        .flat_map_iter(|fi| search_file(&fi.path, &needle_cmp, args.ignore_case, &allowed_levels))
        .collect();

    matches.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line_no.cmp(&b.line_no))
    });

    if args.list_files {
        print_list_mode(&matches, &files);
        return Ok(());
    }

    let full_plain = build_plain_output(&matches);
    let selected = select_lines(&matches, args.head, args.tail);
    let selected_plain = build_plain_output(&selected);

    let mut clipboard = Clipboard::new()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("clipboard init failed: {e}")))?;
    clipboard
        .set_text(strip_str(&selected_plain))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("clipboard set failed: {e}")))?;

    if args.selected_only {
        print_colored_chunked(&selected, &args.needle, args.ignore_case);
    } else {
        print_colored_chunked(&matches, &args.needle, args.ignore_case);
    }

    print_stats(&selected_plain, &full_plain);

    Ok(())
}

fn collect_files(root: &Path) -> Vec<FileInfo> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let path = e.path();
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            Some(FileInfo { path, modified })
        })
        .collect()
}

fn latest_only(files: Vec<FileInfo>) -> Vec<FileInfo> {
    files.into_iter()
        .max_by_key(|f| {
            f.modified
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
        })
        .into_iter()
        .collect()
}

fn build_allowed_levels(args: &Args) -> HashSet<Level> {
    let mut set = HashSet::new();
    set.insert(Level::Trace);
    set.insert(Level::Debug);
    if args.info {
        set.insert(Level::Info);
    }
    if args.warn {
        set.insert(Level::Warn);
    }
    if args.error {
        set.insert(Level::Error);
    }
    set.insert(Level::Other);
    set
}

fn search_file(
    path: &Path,
    needle_cmp: &str,
    ignore_case: bool,
    allowed_levels: &HashSet<Level>,
) -> Vec<MatchLine> {
    let Ok(reader) = open_maybe_compressed(path) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut line_no = 0usize;

    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        line_no += 1;

        let hay: Cow<'_, str> = if ignore_case {
            Cow::Owned(line.to_lowercase())
        } else {
            Cow::Borrowed(line.as_str())
        };

        if !hay.contains(needle_cmp) {
            continue;
        }

        let lvl = detect_level(&line);
        if !allowed_levels.contains(&lvl) {
            continue;
        }

        out.push(MatchLine {
            file: path.to_path_buf(),
            line_no,
            raw_line: line.trim_end_matches('\n').to_string(),
        });
    }

    out
}

fn open_maybe_compressed(path: &Path) -> io::Result<DynRead> {
    let file = File::open(path)?;
    match path.extension().and_then(|s| s.to_str()) {
        Some("lz4") => Ok(Box::new(FrameDecoder::new(file))),
        Some("gz") => Ok(Box::new(MultiGzDecoder::new(file))),
        _ => Ok(Box::new(file)),
    }
}

fn detect_level(line: &str) -> Level {
    if line.contains(" ERROR ") {
        Level::Error
    } else if line.contains(" WARN ") {
        Level::Warn
    } else if line.contains(" INFO ") {
        Level::Info
    } else if line.contains(" DEBUG ") {
        Level::Debug
    } else if line.contains(" TRACE ") {
        Level::Trace
    } else {
        Level::Other
    }
}

fn select_lines(lines: &[MatchLine], head: Option<usize>, tail: Option<usize>) -> Vec<MatchLine> {
    match (head, tail) {
        (Some(h), Some(t)) => {
            if h == 0 || t == 0 {
                return Vec::new();
            }
            if lines.len() <= h + t {
                return lines.to_vec();
            }
            let mut out = Vec::with_capacity(h + t);
            out.extend_from_slice(&lines[..h]);
            out.extend_from_slice(&lines[lines.len() - t..]);
            out
        }
        (Some(h), None) => lines.iter().take(h).cloned().collect(),
        (None, Some(t)) => lines.iter().rev().take(t).cloned().collect::<Vec<_>>().into_iter().rev().collect(),
        (None, None) => lines.to_vec(),
    }
}

fn build_plain_output(lines: &[MatchLine]) -> String {
    let mut out = String::new();
    let mut current_file: Option<&Path> = None;

    for m in lines {
        if current_file != Some(m.file.as_path()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("=== ");
            out.push_str(&m.file.display().to_string());
            out.push_str(" ===\n");
            current_file = Some(m.file.as_path());
        }

        out.push_str(&format!("{}:{}\n", m.line_no, m.raw_line));
    }

    out
}

fn print_colored_chunked(lines: &[MatchLine], needle: &str, ignore_case: bool) {
    let mut current_file: Option<&Path> = None;

    for m in lines {
        if current_file != Some(m.file.as_path()) {
            if current_file.is_some() {
                println!();
            }

            let file_text = m.file.display().to_string();
            let colored_file = common::color_filename(&file_text);
            println!("{}", bold(&format!("=== {} ===", colored_file)));

            current_file = Some(m.file.as_path());
        }

        let prefix = format!("{}:", cyan(&m.line_no.to_string()));
        let rendered = colorize_line(&m.raw_line, needle, ignore_case);
        println!("{}{}", prefix, rendered);
    }
}

fn print_list_mode(matches: &[MatchLine], files: &[FileInfo]) {
    let mut counts: HashMap<&Path, usize> = HashMap::new();
    for m in matches {
        *counts.entry(m.file.as_path()).or_insert(0) += 1;
    }

    let mut rows: Vec<(&Path, usize, SystemTime)> = files
        .iter()
        .filter_map(|fi| {
            let count = counts.get(fi.path.as_path())?;
            Some((fi.path.as_path(), *count, fi.modified))
        })
        .collect();

    rows.sort_by_key(|(_, _, modified)| modified.duration_since(UNIX_EPOCH).unwrap_or_default());

    for (path, count, modified) in rows {
        let ts = format_system_time(modified);
        let file_colored = common::color_filename(&path.display().to_string());
        println!(
            "{}  {}  {}",
            bright_black(&ts),
            yellow(&format!("count={count}")),
            file_colored
        );
    }
}

fn colorize_line(line: &str, needle: &str, ignore_case: bool) -> String {
    let mut s = line.to_string();

    s = colorize_log_timestamps(&s);
    s = colorize_level(&s);
    s = colorize_thread(&s);
    s = colorize_module_and_source(&s);
    s = colorize_json_keys(&s);
    s = colorize_kv_pairs(&s);
    s = colorize_hexes(&s);
    s = colorize_numbers(&s);
    s = highlight_match(&s, needle, ignore_case);

    s
}

fn colorize_log_timestamps(input: &str) -> String {
    let iso = Regex::new(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b").unwrap();
    let s = iso
        .replace_all(input, |caps: &Captures| bright_black(&caps[0]))
        .into_owned();

    let hms = Regex::new(r"\b\d{2}:\d{2}:\d{2}(?:\.\d+)?\b").unwrap();
    hms.replace_all(&s, |caps: &Captures| bright_black(&caps[0]))
        .into_owned()
}

fn colorize_level(input: &str) -> String {
    let re = Regex::new(r"(?P<pre>\s)(?P<lvl>TRACE|DEBUG|INFO|WARN|ERROR)(?P<post>\s)").unwrap();
    re.replace_all(input, |caps: &Captures| {
        let lvl = &caps["lvl"];
        let colored = match lvl {
            "TRACE" => bright_black(lvl),
            "DEBUG" => magenta(lvl),
            "INFO" => blue_bold(lvl),
            "WARN" => yellow_bold(lvl),
            "ERROR" => red_bold(lvl),
            _ => lvl.to_string(),
        };
        format!("{}{}{}", &caps["pre"], colored, &caps["post"])
    })
    .into_owned()
}

fn colorize_thread(input: &str) -> String {
    let re = Regex::new(r"\bThreadId\(\d+\)\b").unwrap();
    re.replace_all(input, |caps: &Captures| magenta(&caps[0])).into_owned()
}

fn colorize_module_and_source(input: &str) -> String {
    let re = Regex::new(r"(?P<mod>\b[a-zA-Z_][\w:]*::[\w:]+)(?P<rest>:\s+\d+:)").unwrap();
    re.replace_all(input, |caps: &Captures| {
        format!("{}{}", green(&caps["mod"]), bright_black(&caps["rest"]))
    })
    .into_owned()
}

fn colorize_json_keys(input: &str) -> String {
    let re = Regex::new(r#""([A-Za-z_][A-Za-z0-9_]*)"\s*:"#).unwrap();
    re.replace_all(input, |caps: &Captures| {
        let whole = &caps[0];
        let key = &caps[1];
        whole.replacen(
            &format!(r#""{}""#, key),
            &format!(r#""{}""#, color_key_name(key)),
            1,
        )
    })
    .into_owned()
}

fn colorize_kv_pairs(input: &str) -> String {
    let eq_re = Regex::new(r"\b([A-Za-z_][A-Za-z0-9_.-]*)=([^\s,}]+)").unwrap();
    let s = eq_re
        .replace_all(input, |caps: &Captures| {
            let key = color_key_name(&caps[1]);
            let val = color_value(&caps[2]);
            format!("{key}={val}")
        })
        .into_owned();

    let colon_re = Regex::new(r"(?m)(^|[\s(])([A-Za-z_][A-Za-z0-9_. @()/+-]{0,50}?):\s+([^\n]+?)$").unwrap();
    colon_re
        .replace_all(&s, |caps: &Captures| {
            let pre = &caps[1];
            let key = color_key_name(caps[2].trim());
            let val = color_freeform_value(caps[3].trim());
            format!("{pre}{key}: {val}")
        })
        .into_owned()
}

fn colorize_hexes(input: &str) -> String {
    let re = Regex::new(r"\b0x[a-fA-F0-9]{8,}\b").unwrap();
    re.replace_all(input, |caps: &Captures| cyan_bold(&caps[0])).into_owned()
}

fn colorize_numbers(input: &str) -> String {
    let re = Regex::new(r"\b\d+(?:\.\d+)?(?:e-?\d+)?\b").unwrap();
    re.replace_all(input, |caps: &Captures| bright_blue(&caps[0])).into_owned()
}

fn highlight_match(input: &str, needle: &str, ignore_case: bool) -> String {
    if needle.is_empty() {
        return input.to_string();
    }

    if ignore_case {
        let re = Regex::new(&format!("(?i){}", regex::escape(needle))).unwrap();
        re.replace_all(input, |caps: &Captures| highlight(&caps[0])).into_owned()
    } else {
        let re = Regex::new(&regex::escape(needle)).unwrap();
        re.replace_all(input, |caps: &Captures| highlight(&caps[0])).into_owned()
    }
}

fn color_key_name(key: &str) -> String {
    let hash = key
        .chars()
        .fold(0u64, |acc, c| acc.wrapping_add(c as u64).wrapping_mul(31));
    let hue = (hash % 360) as f64;
    let saturation = 0.75;
    let lightness = 0.68;
    let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, key)
}

fn color_value(value: &str) -> String {
    if matches!(value, "none" | "None" | "null") {
        bright_black(value)
    } else if matches!(value, "true" | "false") {
        yellow(value)
    } else if value.starts_with("0x") {
        cyan_bold(value)
    } else if value.parse::<f64>().is_ok() {
        bright_blue(value)
    } else if value.starts_with("Some(") || value.starts_with("Ok(") {
        green(value)
    } else if value.starts_with("Err(") {
        red(value)
    } else {
        white(value)
    }
}

fn color_freeform_value(value: &str) -> String {
    if value.starts_with("0x") {
        cyan_bold(value)
    } else if value.parse::<f64>().is_ok() {
        bright_blue(value)
    } else if value.contains("unauthorized") || value.contains("failed") || value.contains("Rejected") {
        red(value)
    } else if value.contains("Uniswap") {
        magenta(value)
    } else {
        white(value)
    }
}

fn print_stats(selected_plain: &str, full_plain: &str) {
    let selected_stats = text_stats(selected_plain);
    let full_stats = text_stats(full_plain);

    println!();
    println!(
        "{} {} {} {} {} {}",
        bold("clipboard:"),
        green(&format!("lines={}", selected_stats.lines)),
        cyan(&format!("words={}", selected_stats.words)),
        yellow(&format!("chars={}", selected_stats.chars)),
        bright_black("|"),
        bright_black("plain text, no ANSI"),
    );
    println!(
        "{} {} {} {}",
        bold("full match:"),
        green(&format!("lines={}", full_stats.lines)),
        cyan(&format!("words={}", full_stats.words)),
        yellow(&format!("chars={}", full_stats.chars)),
    );
}

#[derive(Debug, Clone, Copy)]
struct TextStats {
    lines: usize,
    words: usize,
    chars: usize,
}

fn text_stats(s: &str) -> TextStats {
    TextStats {
        lines: s.lines().count(),
        words: s.split_whitespace().count(),
        chars: s.chars().count(),
    }
}

fn format_system_time(t: SystemTime) -> String {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            format!("mtime={secs}")
        }
        Err(_) => "mtime=<before-epoch>".to_string(),
    }
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;

    let (r1, g1, b1) = match h as i32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    let r = ((r1 + m) * 255.0).round() as u8;
    let g = ((g1 + m) * 255.0).round() as u8;
    let b = ((b1 + m) * 255.0).round() as u8;
    (r, g, b)
}

fn ansi_rgb(r: u8, g: u8, b: u8, s: &str) -> String {
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, s)
}

fn bold(s: &str) -> String {
    format!("\x1b[1m{}\x1b[0m", s)
}

fn highlight(s: &str) -> String {
    format!("\x1b[48;2;80;60;0m\x1b[38;2;255;230;120m\x1b[1m{}\x1b[0m", s)
}

fn bright_black(s: &str) -> String {
    ansi_rgb(140, 140, 140, s)
}

fn white(s: &str) -> String {
    ansi_rgb(230, 230, 230, s)
}

fn red(s: &str) -> String {
    ansi_rgb(255, 107, 107, s)
}

fn red_bold(s: &str) -> String {
    format!("\x1b[1m{}\x1b[0m", red(s))
}

fn green(s: &str) -> String {
    ansi_rgb(114, 220, 120, s)
}

fn yellow(s: &str) -> String {
    ansi_rgb(240, 210, 90, s)
}

fn yellow_bold(s: &str) -> String {
    format!("\x1b[1m{}\x1b[0m", yellow(s))
}

fn blue(s: &str) -> String {
    ansi_rgb(110, 170, 255, s)
}

fn blue_bold(s: &str) -> String {
    format!("\x1b[1m{}\x1b[0m", blue(s))
}

fn bright_blue(s: &str) -> String {
    ansi_rgb(140, 190, 255, s)
}

fn cyan(s: &str) -> String {
    ansi_rgb(90, 220, 220, s)
}

fn cyan_bold(s: &str) -> String {
    format!("\x1b[1m{}\x1b[0m", cyan(s))
}

fn magenta(s: &str) -> String {
    ansi_rgb(210, 140, 255, s)
}