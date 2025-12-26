//! XDG-compliant path resolution for application directories.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::{APP_NAME, AppConfig};

/// Application paths for config, data, and state directories.
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl AppPaths {
    /// Discover application paths, optionally overriding the config file location.
    pub fn discover(override_path: Option<PathBuf>) -> Result<Self> {
        let config_file = match override_path {
            Some(path) => {
                let expanded = expand_path(path)?;
                if expanded.is_dir() {
                    expanded.join("config.toml")
                } else {
                    expanded
                }
            }
            None => default_config_dir()?.join("config.toml"),
        };

        if config_file.parent().is_none() {
            return Err(anyhow!("invalid config file path: {config_file:?}"));
        }

        let data_dir = default_data_dir()?;
        let state_dir = default_state_dir()?;

        Ok(Self {
            config_file,
            data_dir,
            state_dir,
        })
    }

    /// Apply path overrides from configuration.
    pub fn apply_overrides(mut self, cfg: &AppConfig) -> Result<Self> {
        if let Some(ref data_override) = cfg.paths.data_dir {
            self.data_dir = expand_str_path(data_override)?;
        }
        if let Some(ref state_override) = cfg.paths.state_dir {
            self.state_dir = expand_str_path(state_override)?;
        }
        Ok(self)
    }

    /// Ensure all required directories exist.
    pub fn ensure_directories(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("creating data directory {}", self.data_dir.display()))?;
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("creating state directory {}", self.state_dir.display()))?;
        Ok(())
    }

    /// Log directory creation in dry-run mode.
    pub fn log_dry_run(&self) {
        log::info!(
            "dry-run: would ensure data dir {} and state dir {}",
            self.data_dir.display(),
            self.state_dir.display()
        );
    }
}

impl std::fmt::Display for AppPaths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "config: {}, data: {}, state: {}",
            self.config_file.display(),
            self.data_dir.display(),
            self.state_dir.display()
        )
    }
}

/// Expand a PathBuf, resolving ~ and environment variables.
pub fn expand_path(path: PathBuf) -> Result<PathBuf> {
    if let Some(text) = path.to_str() {
        expand_str_path(text)
    } else {
        Ok(path)
    }
}

/// Expand a string path, resolving ~ and environment variables.
pub fn expand_str_path(text: &str) -> Result<PathBuf> {
    let expanded = shellexpand::full(text).context("expanding path")?;
    Ok(PathBuf::from(expanded.to_string()))
}

/// Get the default configuration directory (XDG_CONFIG_HOME or fallback).
pub fn default_config_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let mut path = PathBuf::from(dir);
        path.push(APP_NAME);
        return Ok(path);
    }

    if let Some(mut dir) = dirs::config_dir() {
        dir.push(APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".config").join(APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine configuration directory"))
}

/// Get the default data directory (XDG_DATA_HOME or fallback).
pub fn default_data_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(APP_NAME));
    }

    if let Some(mut dir) = dirs::data_dir() {
        dir.push(APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".local").join("share").join(APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine data directory"))
}

/// Get the default state directory (XDG_STATE_HOME or fallback).
pub fn default_state_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(APP_NAME));
    }

    if let Some(mut dir) = dirs::state_dir() {
        dir.push(APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".local").join("state").join(APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine state directory"))
}

/// Get the default cache directory (XDG_CACHE_HOME or fallback).
pub fn default_cache_dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(APP_NAME));
    }

    if let Some(mut dir) = dirs::cache_dir() {
        dir.push(APP_NAME);
        return Ok(dir);
    }

    dirs::home_dir()
        .map(|home| home.join(".cache").join(APP_NAME))
        .ok_or_else(|| anyhow!("unable to determine cache directory"))
}

/// Write the default configuration file to the specified path.
pub fn write_default_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config directory {parent:?}"))?;
    }

    let config = AppConfig::default();
    let toml_str = toml::to_string_pretty(&config).context("serializing default config to TOML")?;
    let mut body = default_config_header(path)?;
    body.push_str(&toml_str);
    fs::write(path, body).with_context(|| format!("writing config file to {}", path.display()))
}

fn default_config_header(path: &Path) -> Result<String> {
    let mut buffer = String::new();
    buffer.push_str("# Configuration for ");
    buffer.push_str(APP_NAME);
    buffer.push('\n');
    buffer.push_str("# File: ");
    buffer.push_str(&path.display().to_string());
    buffer.push('\n');
    buffer.push('\n');
    Ok(buffer)
}
