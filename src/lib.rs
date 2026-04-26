// src/lib.rs
pub mod common;
pub mod config;

// Re-export commonly used items
pub use config::{
    UnifiedConfig, WccConfig, WcnConfig, WcpConfig, WclConfig, WcfConfig,
    load_unified_config, get_config_path, save_config, update_cargo_mode, 
    show_config, init_config
};