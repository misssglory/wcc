use std::{
    env,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use arboard::Clipboard;

#[derive(Debug, Clone, Default)]
struct TextStats {
    lines: usize,
    words: usize,
    chars: usize,
    bytes: usize,
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 1 {
        bail!("usage: wcn file.ext");
    }

    let path = PathBuf::from(args.remove(0));
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("failed to extract filename")?
        .to_string();

    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let prefix = comment_prefix(&path, &ext);

    let payload = if header_present(&content, &filename) {
        content.clone()
    } else {
        format!("{prefix} {filename}\n{content}")
    };

    set_clipboard(&payload)?;
    let stats = calc_stats(&payload);

    print_stats(&path, &stats);

    Ok(())
}

fn header_present(content: &str, filename: &str) -> bool {
    content
        .lines()
        .take(2)
        .any(|line| line.contains(filename))
}

fn comment_prefix(path: &Path, ext: &str) -> &'static str {
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(
        ext,
        "rs"
            | "c"
            | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "java"
            | "kt"
            | "kts"
            | "go"
            | "swift"
            | "scala"
            | "cs"
            | "dart"
    ) {
        "//"
    } else if matches!(
        ext,
        "py"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "nix"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "conf"
            | "rb"
            | "pl"
            | "mk"
            | "make"
            | "env"
    ) || filename == "dockerfile"
        || filename == "makefile"
    {
        "#"
    } else {
        "#"
    }
}

fn calc_stats(s: &str) -> TextStats {
    TextStats {
        lines: s.bytes().filter(|b| *b == b'\n').count(),
        words: s.split_whitespace().count(),
        chars: s.chars().count(),
        bytes: s.as_bytes().len(),
    }
}

fn print_stats(path: &Path, stats: &TextStats) {
    let file = path.display();
    println!(
        "\x1b[36mfile\x1b[0m {}  \x1b[33mlines\x1b[0m {}  \x1b[33mwords\x1b[0m {}  \x1b[33mchars\x1b[0m {}  \x1b[33mbytes\x1b[0m {}",
        file, stats.lines, stats.words, stats.chars, stats.bytes
    );
}

fn set_clipboard(payload: &str) -> Result<()> {
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
