use std::path::Path;
use anyhow::{Context, Result};
use arboard::Clipboard;
use std::process::Command;
use std::io::Write;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct TextStats {
    pub lines: usize,
    pub words: usize,
    pub chars: usize,
    pub bytes: usize,
}

pub fn calc_stats(s: &str) -> TextStats {
    TextStats {
        lines: s.bytes().filter(|b| *b == b'\n').count(),
        words: s.split_whitespace().count(),
        chars: s.chars().count(),
        bytes: s.as_bytes().len(),
    }
}

pub fn set_clipboard(payload: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return set_clipboard_wayland(payload);
        }
    }

    set_clipboard_arboard(payload)
}

#[cfg(target_os = "linux")]
fn set_clipboard_wayland(payload: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain;charset=utf-8")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn wl-copy")?;

    {
        let mut stdin = child.stdin.take().context("failed to open wl-copy stdin")?;
        stdin.write_all(payload.as_bytes())?;
        stdin.flush()?;
    }

    let _ = child.wait();
    Ok(())
}

fn set_clipboard_arboard(payload: &str) -> Result<()> {
    let mut cb = Clipboard::new().context("clipboard init failed")?;
    cb.set_text(payload.to_string())?;

    #[cfg(target_os = "linux")]
    {
        thread::sleep(Duration::from_millis(200));
    }

    Ok(())
}

pub fn get_clipboard_text() -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let out = Command::new("wl-paste")
                .arg("--no-newline")
                .output()
                .context("failed to spawn wl-paste")?;
            if out.status.success() {
                return Ok(String::from_utf8_lossy(&out.stdout).to_string());
            }
        }
        
        let out = Command::new("xclip")
            .arg("-selection")
            .arg("clipboard")
            .arg("-o")
            .output()
            .context("failed to spawn xclip")?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).to_string());
        }
    }
    
    let mut cb = Clipboard::new().context("clipboard init failed")?;
    cb.get_text().context("failed to read clipboard text")
}

pub fn comment_prefix(path: &Path, ext: &str) -> &'static str {
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(
        ext,
        "rs" | "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "js" | "jsx" | "ts" | "tsx"
            | "java" | "kt" | "kts" | "go" | "swift" | "scala" | "cs" | "dart"
    ) {
        "//"
    } else if matches!(
        ext,
        "py" | "sh" | "bash" | "zsh" | "fish" | "nix" | "yaml" | "yml" | "toml" | "ini"
            | "conf" | "rb" | "pl" | "mk" | "make" | "env"
    ) || filename == "dockerfile"
        || filename == "makefile"
    {
        "#"
    } else {
        "#"
    }
}

pub fn regex_escape(s: &str) -> String {
    let special_chars = r".*+?^${}()|[]\";
    let mut escaped = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if special_chars.contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

pub fn heatmap_color_lines(value: usize) -> String {
    let (r, g, b) = get_bright_color(value, 2000);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, value)
}

pub fn heatmap_color_words(value: usize) -> String {
    let (r, g, b) = get_bright_color(value, 200000);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, value)
}

pub fn heatmap_color_chars(value: usize) -> String {
    let (r, g, b) = get_bright_color(value, 50000);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, value)
}

pub fn heatmap_color_bytes(value: usize) -> String {
    let (r, g, b) = get_bright_color(value, 50000);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, value)
}

fn get_bright_color(value: usize, max: usize) -> (u8, u8, u8) {
    if value == 0 {
        return (0, 200, 200);
    }
    
    let ratio = (value as f64 / max as f64).min(1.0);
    
    if ratio < 0.2 {
        let t = ratio / 0.2;
        (0, 150 + (t * 105.0) as u8, 200)
    } else if ratio < 0.4 {
        let t = (ratio - 0.2) / 0.2;
        ((t * 100.0) as u8, 255, 200 - (t * 100.0) as u8)
    } else if ratio < 0.6 {
        let t = (ratio - 0.4) / 0.2;
        (100 + (t * 155.0) as u8, 255, 100 - (t * 100.0) as u8)
    } else if ratio < 0.8 {
        let t = (ratio - 0.6) / 0.2;
        (255, 255 - (t * 100.0) as u8, 0)
    } else {
        let t = (ratio - 0.8) / 0.2;
        (255, 155 - (t * 155.0) as u8, 0)
    }
}

pub fn color_filename(filename: &str) -> String {
    let hash = filename.chars().fold(0u64, |acc, c| {
        acc.wrapping_add(c as u64).wrapping_mul(31)
    });
    
    let hue = (hash % 360) as f64;
    let saturation = 0.8;
    let lightness = 0.7;
    
    let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, filename)
}

pub fn color_function_name(name: &str) -> String {
    let hash = name.chars().fold(0u64, |acc, c| {
        acc.wrapping_add(c as u64).wrapping_mul(17)
    });
    
    let hue = (hash % 360) as f64;
    let saturation = 0.6;
    let lightness = 0.75;
    
    let (r, g, b) = hsl_to_rgb(hue, saturation, lightness);
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, name)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    
    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    
    let m = l - c / 2.0;
    let r = ((r1 + m) * 255.0) as u8;
    let g = ((g1 + m) * 255.0) as u8;
    let b = ((b1 + m) * 255.0) as u8;
    
    (r, g, b)
}

pub fn print_colored_stats(stats: &TextStats, label: &str) {
    println!(
        "  \x1b[33m{}:\x1b[0m \x1b[36mlines\x1b[0m {}  \x1b[36mwords\x1b[0m {}  \x1b[36mchars\x1b[0m {}  \x1b[36mbytes\x1b[0m {}",
        label,
        heatmap_color_lines(stats.lines),
        heatmap_color_words(stats.words),
        heatmap_color_chars(stats.chars),
        heatmap_color_bytes(stats.bytes)
    );
}

pub fn print_summary(stats: &TextStats, filename: &str, operation: &str) {
    println!("\n\x1b[1;32m✓ {}\x1b[0m", operation);
    println!("  \x1b[36mfile:\x1b[0m {}", color_filename(filename));
    print_colored_stats(stats, "stats");
}