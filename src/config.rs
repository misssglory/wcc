// src/config.rs
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedConfig {
    #[serde(default)]
    pub wcc: WccConfig,
    #[serde(default)]
    pub wcn: WcnConfig,
    #[serde(default)]
    pub wcp: WcpConfig,
    #[serde(default)]
    pub wcl: WclConfig,
    #[serde(default)]
    pub wcf: WcfConfig,
    #[serde(default)]
    pub wff: WffConfig,
    #[serde(default)]
    pub wcg: WcgConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WccConfig {
    pub default_cargo_mode: String,
    pub time_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WcnConfig {
    pub show_time_in_header: bool,
    pub use_file_modification_time: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WcpConfig {
    pub auto_backup: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WclConfig {
    pub max_file_size_kb: usize,
    pub max_file_words_to_copy: usize,
    pub max_clipboard_bytes: usize,
    pub skip_patterns: Vec<String>,
    pub skip_dirs: Vec<String>,
    pub show_empty_files: bool,
    pub show_stats_per_file: bool,
    pub show_function_details: bool,
    pub show_class_details: bool,
    pub show_usage_stats: bool,
    pub max_files_to_display: usize,
    pub min_function_lines: usize,
    pub min_class_lines: usize,
    pub max_functions_per_file: usize,
    pub max_classes_per_file: usize,
    pub parallel_processing: bool,
    pub max_threads: usize,
    pub copy_file_contents: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WcfConfig {
    pub auto_format: bool,
    pub show_buffer_preview: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WffConfig {
    pub show_line_numbers: bool,
    pub show_time_in_header: bool,
    pub use_file_modification_time: bool,
    pub time_format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WcgConfig {
    pub show_line_numbers: bool,
    pub show_calls: bool,
    pub show_fields: bool,
}

impl Default for WffConfig {
    fn default() -> Self {
        Self {
            show_line_numbers: true,
            show_time_in_header: true,
            use_file_modification_time: true,
            time_format: "%H:%M:%S %d.%m.%Y".to_string(),
        }
    }
}

impl Default for WcgConfig {
    fn default() -> Self {
        Self {
            show_line_numbers: true,
            show_calls: true,
            show_fields: true,
        }
    }
}

impl Default for UnifiedConfig {
    fn default() -> Self {
        Self {
            wcc: WccConfig {
                default_cargo_mode: "debug".to_string(),
                time_format: "%H:%M:%S %d.%m.%Y".to_string(),
            },
            wcn: WcnConfig {
                show_time_in_header: true,
                use_file_modification_time: true,
            },
            wcp: WcpConfig {
                auto_backup: true,
            },
            wcl: WclConfig {
                max_file_size_kb: 50,
                max_file_words_to_copy: 10000,
                max_clipboard_bytes: 40 * 1024,
                skip_patterns: vec![
                    ".o".to_string(), ".pyc".to_string(), ".pyo".to_string(),
                    ".so".to_string(), ".dll".to_string(), ".dylib".to_string(),
                    ".exe".to_string(), ".class".to_string(), ".jar".to_string(),
                    ".war".to_string(), ".ear".to_string(), ".zip".to_string(),
                    ".tar".to_string(), ".gz".to_string(), ".bz2".to_string(),
                    ".xz".to_string(), ".7z".to_string(), ".rar".to_string(),
                    ".png".to_string(), ".jpg".to_string(), ".jpeg".to_string(),
                    ".gif".to_string(), ".bmp".to_string(), ".ico".to_string(),
                    ".mp3".to_string(), ".mp4".to_string(), ".avi".to_string(),
                    ".mov".to_string(), ".pdf".to_string(), ".doc".to_string(),
                    ".docx".to_string(), ".bkp".to_string(),
                ],
                skip_dirs: vec![
                    "target".to_string(), "node_modules".to_string(),
                    ".git".to_string(), ".svn".to_string(), ".hg".to_string(),
                    "build".to_string(), "dist".to_string(), "__pycache__".to_string(),
                    ".cache".to_string(), ".cargo".to_string(), ".idea".to_string(),
                    ".vscode".to_string(),
                ],
                show_empty_files: false,
                show_stats_per_file: true,
                show_function_details: true,
                show_class_details: true,
                show_usage_stats: false,
                max_files_to_display: 500,
                min_function_lines: 1,
                min_class_lines: 1,
                max_functions_per_file: 100,
                max_classes_per_file: 50,
                parallel_processing: true,
                max_threads: 8,
                copy_file_contents: true,
            },
            wcf: WcfConfig {
                auto_format: true,
                show_buffer_preview: true,
            },
            wff: WffConfig::default(),
            wcg: WcgConfig::default(),
        }
    }
}

pub fn get_config_path() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("wcc/config.toml");
    path
}

pub fn load_unified_config() -> Result<UnifiedConfig> {
    let path = get_config_path();
    
    if path.exists() {
        let data = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(toml::from_str(&data).context("parsing config")?)
    } else {
        let cfg = UnifiedConfig::default();
        save_config(&cfg)?;
        eprintln!("\x1b[36m✓ Created default config at: {}\x1b[0m", path.display());
        Ok(cfg)
    }
}

pub fn save_config(config: &UnifiedConfig) -> Result<()> {
    let path = get_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml_str = toml::to_string_pretty(config)?;
    fs::write(&path, toml_str)?;
    Ok(())
}

pub fn update_cargo_mode(mode: &str) -> Result<()> {
    let mut config = load_unified_config()?;
    config.wcc.default_cargo_mode = mode.to_string();
    save_config(&config)?;
    println!("\x1b[32m✓ Updated default cargo mode to: {}\x1b[0m", mode);
    Ok(())
}

pub fn show_config() -> Result<()> {
    let config = load_unified_config()?;
    
    println!("\x1b[36m📋 Current wcc configuration:\x1b[0m");
    println!("  [wcc]");
    println!("    default_cargo_mode: \x1b[33m{}\x1b[0m", config.wcc.default_cargo_mode);
    println!("    time_format: {}", config.wcc.time_format);
    println!("  [wcn]");
    println!("    show_time_in_header: {}", config.wcn.show_time_in_header);
    println!("    use_file_modification_time: {}", config.wcn.use_file_modification_time);
    println!("  [wcp]");
    println!("    auto_backup: {}", config.wcp.auto_backup);
    println!("  [wcl]");
    println!("    max_file_size_kb: {}", config.wcl.max_file_size_kb);
    println!("    max_file_words_to_copy: {}", config.wcl.max_file_words_to_copy);
    println!("    max_clipboard_bytes: {}", config.wcl.max_clipboard_bytes);
    println!("    skip_patterns: {:?}", config.wcl.skip_patterns);
    println!("    skip_dirs: {:?}", config.wcl.skip_dirs);
    println!("    show_empty_files: {}", config.wcl.show_empty_files);
    println!("    show_stats_per_file: {}", config.wcl.show_stats_per_file);
    println!("    show_function_details: {}", config.wcl.show_function_details);
    println!("    show_class_details: {}", config.wcl.show_class_details);
    println!("    show_usage_stats: {}", config.wcl.show_usage_stats);
    println!("    max_files_to_display: {}", config.wcl.max_files_to_display);
    println!("    min_function_lines: {}", config.wcl.min_function_lines);
    println!("    min_class_lines: {}", config.wcl.min_class_lines);
    println!("    max_functions_per_file: {}", config.wcl.max_functions_per_file);
    println!("    max_classes_per_file: {}", config.wcl.max_classes_per_file);
    println!("    parallel_processing: {}", config.wcl.parallel_processing);
    println!("    max_threads: {}", config.wcl.max_threads);
    println!("    copy_file_contents: {}", config.wcl.copy_file_contents);
    println!("  [wcf]");
    println!("    auto_format: {}", config.wcf.auto_format);
    println!("    show_buffer_preview: {}", config.wcf.show_buffer_preview);
    println!("  [wff]");
    println!("    show_line_numbers: {}", config.wff.show_line_numbers);
    println!("    show_time_in_header: {}", config.wff.show_time_in_header);
    println!("    use_file_modification_time: {}", config.wff.use_file_modification_time);
    println!("    time_format: {}", config.wff.time_format);
    println!("  [wcg]");
    println!("    show_line_numbers: {}", config.wcg.show_line_numbers);
    println!("    show_calls: {}", config.wcg.show_calls);
    println!("    show_fields: {}", config.wcg.show_fields);
    println!();
    println!("  Config file: \x1b[90m{}\x1b[0m", get_config_path().display());
    
    Ok(())
}

pub fn init_config() -> Result<()> {
    let path = get_config_path();
    
    if path.exists() {
        use std::io::Write;
        print!("\x1b[33m⚠ Config already exists at: {}\x1b[0m", path.display());
        print!("\n❓ Overwrite? (y/n): ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim().to_lowercase() != "y" {
            println!("❌ Aborted.");
            return Ok(());
        }
    }
    
    let config = UnifiedConfig::default();
    save_config(&config)?;
    
    println!("\x1b[32m✓ Created default config at: {}\x1b[0m", path.display());
    println!("\nDefault configuration includes:");
    println!("  [wcc] - cargo wrapper settings");
    println!("  [wcn] - file copy settings");
    println!("  [wcp] - paste settings");
    println!("  [wcl] - analyzer settings with {} skip patterns", config.wcl.skip_patterns.len());
    println!("  [wcf] - function replacement settings");
    println!("  [wff] - wff (function finder) settings with time and line number options");
    println!("  [wcg] - code graph analyzer settings");
    
    Ok(())
}