use anyhow::{bail, Context, Result};
use clap::Parser;
use regex::Regex;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use walkdir::{DirEntry, WalkDir};
use wcc::common::color_filename;
use wcc::config::load_unified_config;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Debug, Parser)]
#[command(
    name = "wcz",
    version,
    about = "Archive one or more folders while respecting wcc ignore settings"
)]
struct Args {
    /// Folders whose contents should be added to the archive.
    #[arg(required = true, value_name = "FOLDER")]
    folders: Vec<PathBuf>,

    /// Output directory or explicit .zip file. Overrides [wcz].default_folder.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,

    /// Explicit archive filename (without changing the destination directory).
    #[arg(short = 'n', long, value_name = "NAME")]
    name: Option<String>,
}

#[derive(Debug)]
struct IgnoreMatcher {
    globs: Vec<Regex>,
    regexes: Vec<Regex>,
}

impl IgnoreMatcher {
    fn new(ignore: &[String], ignore_regexes: &[String]) -> Result<Self> {
        let globs = ignore
            .iter()
            .map(|pattern| {
                let regex = glob_to_regex(pattern);
                Regex::new(&regex)
                    .with_context(|| format!("invalid wcz ignore pattern {pattern:?}"))
            })
            .collect::<Result<Vec<_>>>()?;

        let regexes = ignore_regexes
            .iter()
            .map(|pattern| {
                Regex::new(pattern)
                    .with_context(|| format!("invalid [wcz].ignore_regexes entry {pattern:?}"))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self { globs, regexes })
    }

    fn matches(&self, rel: &Path) -> bool {
        let normalized = normalize_path(rel);
        let basename = rel
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");

        self.globs.iter().any(|rule| {
            rule.is_match(&normalized)
                || rule.is_match(basename)
                || rel.components().any(|component| match component {
                    Component::Normal(value) => rule.is_match(&value.to_string_lossy()),
                    _ => false,
                })
        }) || self.regexes.iter().any(|rule| rule.is_match(&normalized))
    }
}

#[derive(Debug)]
struct ArchiveEntry {
    source: PathBuf,
    archive_path: String,
    is_dir: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = load_unified_config()?;
    let matcher = IgnoreMatcher::new(&config.wcz.ignore, &config.wcz.ignore_regexes)?;

    let roots = validate_roots(&args.folders)?;
    let output_path = resolve_output_path(&args, &config.wcz.default_folder, &roots)?;

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }

    let output_abs = absolute_path(&output_path)?;
    let entries = collect_entries(&roots, &matcher, &output_abs)?;

    println!(
        "\x1b[36marchive\x1b[0m {}",
        color_filename(&output_path.display().to_string())
    );

    write_archive(&output_path, &entries)?;

    let file_count = entries.iter().filter(|entry| !entry.is_dir).count();
    let dir_count = entries.iter().filter(|entry| entry.is_dir).count();
    let size = fs::metadata(&output_path).map(|meta| meta.len()).unwrap_or(0);

    println!("\n\x1b[1;32m✓ Archive created\x1b[0m");
    println!("  \x1b[36mfolders:\x1b[0m {dir_count}");
    println!("  \x1b[36mfiles:\x1b[0m {file_count}");
    println!("  \x1b[36msize:\x1b[0m {} bytes", size);
    println!(
        "  \x1b[36moutput:\x1b[0m {}",
        color_filename(&output_path.display().to_string())
    );

    Ok(())
}

fn validate_roots(folders: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut roots = Vec::with_capacity(folders.len());

    for folder in folders {
        if !folder.exists() {
            bail!("folder does not exist: {}", folder.display());
        }
        if !folder.is_dir() {
            bail!("not a folder: {}", folder.display());
        }

        let canonical = folder
            .canonicalize()
            .with_context(|| format!("resolving {}", folder.display()))?;
        if seen.insert(canonical.clone()) {
            roots.push(canonical);
        }
    }

    Ok(roots)
}

fn resolve_output_path(
    args: &Args,
    configured_folder: &Option<String>,
    roots: &[PathBuf],
) -> Result<PathBuf> {
    let default_name = args
        .name
        .as_deref()
        .map(ensure_zip_extension)
        .unwrap_or_else(|| default_archive_name(roots));

    if let Some(output) = &args.output {
        let expanded = expand_tilde_path(output);
        if looks_like_zip_file(&expanded) {
            if args.name.is_some() {
                bail!("--name cannot be combined with --output pointing to a .zip file");
            }
            return Ok(expanded);
        }
        return Ok(expanded.join(default_name));
    }

    let output_dir = configured_folder
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| expand_tilde_path(Path::new(value)))
        .unwrap_or(std::env::current_dir().context("getting current directory")?);

    Ok(output_dir.join(default_name))
}

fn default_archive_name(roots: &[PathBuf]) -> String {
    if roots.len() == 1 {
        let name = roots[0]
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("archive");
        ensure_zip_extension(name)
    } else {
        let names = roots
            .iter()
            .filter_map(|root| root.file_name().and_then(|value| value.to_str()))
            .map(sanitize_filename_component)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();

        let stem = if names.is_empty() {
            "archive".to_string()
        } else {
            names.join("+")
        };
        ensure_zip_extension(&stem)
    }
}

fn sanitize_filename_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn ensure_zip_extension(name: &str) -> String {
    if name.to_ascii_lowercase().ends_with(".zip") {
        name.to_string()
    } else {
        format!("{name}.zip")
    }
}

fn looks_like_zip_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("zip"))
        .unwrap_or(false)
}

fn expand_tilde_path(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn collect_entries(
    roots: &[PathBuf],
    matcher: &IgnoreMatcher,
    output_abs: &Path,
) -> Result<Vec<ArchiveEntry>> {
    let multiple_roots = roots.len() > 1;
    let mut entries = Vec::new();

    for root in roots {
        let prefix = if multiple_roots {
            root.file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("archive"))
        } else {
            PathBuf::new()
        };

        let walker = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| should_descend(entry, root, matcher));

        for result in walker {
            let entry = result.with_context(|| format!("walking {}", root.display()))?;
            let source = entry.path();

            if source == root {
                continue;
            }

            let rel = source
                .strip_prefix(root)
                .with_context(|| format!("making {} relative", source.display()))?;

            if matcher.matches(rel) {
                print_skipped(source, entry.file_type().is_dir());
                continue;
            }

            if entry.file_type().is_symlink() {
                println!(
                    "\x1b[90mskip symlink\x1b[0m {}",
                    color_filename(&source.display().to_string())
                );
                continue;
            }

            if !entry.file_type().is_dir() && !entry.file_type().is_file() {
                continue;
            }

            let source_abs = absolute_path(source)?;
            if source_abs == output_abs {
                continue;
            }

            let archive_rel = prefix.join(rel);
            let archive_path = normalize_path(&archive_rel);
            entries.push(ArchiveEntry {
                source: source.to_path_buf(),
                archive_path,
                is_dir: entry.file_type().is_dir(),
            });
        }
    }

    Ok(entries)
}

fn should_descend(entry: &DirEntry, root: &Path, matcher: &IgnoreMatcher) -> bool {
    if entry.path() == root {
        return true;
    }

    match entry.path().strip_prefix(root) {
        Ok(rel) if matcher.matches(rel) => {
            print_skipped(entry.path(), entry.file_type().is_dir());
            false
        }
        _ => true,
    }
}

fn print_skipped(path: &Path, is_dir: bool) {
    let kind = if is_dir { "dir " } else { "file" };
    println!(
        "\x1b[90mskip {kind}\x1b[0m {}",
        color_filename(&path.display().to_string())
    );
}

fn write_archive(output_path: &Path, entries: &[ArchiveEntry]) -> Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("creating {}", output_path.display()))?;
    let writer = BufWriter::new(file);
    let mut zip = ZipWriter::new(writer);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let dir_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .unix_permissions(0o755);

    let mut buffer = vec![0_u8; 128 * 1024];

    for entry in entries {
        if entry.is_dir {
            let directory_name = format!("{}/", entry.archive_path.trim_end_matches('/'));
            zip.add_directory(&directory_name, dir_options)
                .with_context(|| format!("adding directory {directory_name}"))?;
            println!(
                "  \x1b[36m📁\x1b[0m {}",
                color_filename(&entry.archive_path)
            );
            continue;
        }

        zip.start_file(&entry.archive_path, options)
            .with_context(|| format!("adding {}", entry.archive_path))?;

        let mut input = File::open(&entry.source)
            .with_context(|| format!("opening {}", entry.source.display()))?;
        loop {
            let read = input
                .read(&mut buffer)
                .with_context(|| format!("reading {}", entry.source.display()))?;
            if read == 0 {
                break;
            }
            zip.write_all(&buffer[..read])
                .with_context(|| format!("writing {}", entry.archive_path))?;
        }

        println!(
            "  \x1b[32m📄\x1b[0m {}",
            color_filename(&entry.archive_path)
        );
    }

    let mut writer = zip.finish().context("finalizing zip archive")?;
    writer.flush().context("flushing zip archive")?;
    Ok(())
}

fn glob_to_regex(pattern: &str) -> String {
    let mut result = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => result.push_str(".*"),
            '?' => result.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                result.push('\\');
                result.push(ch);
            }
            _ => result.push(ch),
        }
    }
    result.push('$');
    result
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_string()),
            Component::CurDir => None,
            Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("getting current directory")?
            .join(path))
    }
}
