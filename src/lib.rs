// src/lib.rs
pub mod common;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WccConfig {
    #[serde(default)]
    pub wcc: WccSection,
    #[serde(default)]
    pub wcl: WclSection,
    #[serde(default)]
    pub wcn: WcnSection,
    #[serde(default)]
    pub wcp: WcpSection,
    #[serde(default)]
    pub wcf: WcfSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WccSection {
    pub history_dir: PathBuf,
    pub compress_above_bytes: usize,
    pub retain: RetainPolicy,
    pub time_format: String,
    pub default_cargo_mode: String,
}

impl Default for WccSection {
    fn default() -> Self {
        let mut dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        dir.push(".local/state/wcc/history");
        Self {
            history_dir: dir,
            compress_above_bytes: 16384,
            retain: RetainPolicy {
                mode: "bytes".to_string(),
                limit: 131072,
            },
            time_format: "%H:%M:%S %d.%m.%Y".to_string(),
            default_cargo_mode: "debug".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainPolicy {
    pub mode: String,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WclSection {
    pub max_file_size_kb: usize,
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
}

impl Default for WclSection {
    fn default() -> Self {
        Self {
            max_file_size_kb: 50,
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
                ".docx".to_string(), ".bkp".to_string(), ".bkp.d".to_string(),
            ],
            skip_dirs: vec![
                "target".to_string(), "target.bkp.d".to_string(), "node_modules".to_string(),
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WcnSection {
    pub default_head_lines: Option<usize>,
    pub default_tail_lines: Option<usize>,
    pub show_time_in_header: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WcpSection {
    pub auto_backup: bool,
    pub backup_suffix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WcfSection {
    pub auto_format: bool,
    pub backup_before_replace: bool,
    pub show_buffer_preview: bool,  // Add this line
}

impl Default for WcfSection {
    fn default() -> Self {
        Self {
            auto_format: true,
            backup_before_replace: true,
            show_buffer_preview: true, 
        }
    }
}

impl Default for WccConfig {
    fn default() -> Self {
        Self {
            wcc: WccSection::default(),
            wcl: WclSection::default(),
            wcn: WcnSection::default(),
            wcp: WcpSection::default(),
            wcf: WcfSection::default(),
        }
    }
}

pub fn load_unified_config() -> Result<WccConfig> {
    use anyhow::Context;
    
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("wcc/config.toml");
    
    if path.exists() {
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let config: WccConfig = toml::from_str(&data).context("parsing config")?;
        Ok(config)
    } else {
        let cfg = WccConfig::default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        eprintln!("\x1b[36m✓ Created default config at: {}\x1b[0m", path.display());
        Ok(cfg)
    }
}
