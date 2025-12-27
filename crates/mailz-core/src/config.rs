//! Configuration types and loading for the application.

use std::path::Path;

use anyhow::Result;
use config::{Config, Environment, File, FileFormat};
use serde::{Deserialize, Serialize};

use crate::paths::{expand_str_path, write_default_config};
use crate::{AppPaths, default_parallelism, env_prefix};

/// Main application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub profile: String,
    pub logging: LoggingConfig,
    pub runtime: RuntimeConfig,
    pub paths: PathsConfig,
    pub maintenance: MaintenanceConfig,
    pub api: ApiConfig,
    pub tui: TuiConfig,
    pub watch: WatchConfig,
}

impl AppConfig {
    /// Override the profile if a value is provided.
    pub fn with_profile_override(mut self, profile: Option<String>) -> Self {
        if let Some(profile) = profile {
            self.profile = profile;
        }
        self
    }

    /// Load configuration from file and environment, creating defaults if needed.
    pub fn load(paths: &AppPaths, dry_run: bool) -> Result<Self> {
        if !paths.config_file.exists() {
            if dry_run {
                log::info!(
                    "dry-run: would create default config at {}",
                    paths.config_file.display()
                );
            } else {
                write_default_config(&paths.config_file)?;
            }
        }

        Self::load_from_path(&paths.config_file)
    }

    /// Load configuration from a specific path.
    pub fn load_from_path(config_file: &Path) -> Result<Self> {
        let env_prefix = env_prefix();
        let built = Config::builder()
            .set_default("profile", "default")?
            .set_default("logging.level", "info")?
            .set_default("runtime.parallelism", default_parallelism() as i64)?
            .set_default("runtime.timeout", 60_i64)?
            .set_default("runtime.fail_fast", true)?
            .set_default("maintenance.gc_interval_seconds", 300_u64)?
            .set_default("maintenance.message_retention_days", 30_u64)?
            .set_default("api.rate_limit_per_minute", 60_u64)?
            .set_default("tui.poll_interval_seconds", 5_u64)?
            .set_default("watch.poll_interval_seconds", 5_u64)?
            .add_source(
                File::from(config_file)
                    .format(FileFormat::Toml)
                    .required(false),
            )
            .add_source(Environment::with_prefix(env_prefix.as_str()).separator("__"))
            .build()?;

        let mut config: AppConfig = built.try_deserialize()?;

        if let Some(ref file) = config.logging.file {
            let expanded = expand_str_path(file)?;
            config.logging.file = Some(expanded.display().to_string());
        }

        Ok(config)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            profile: "default".to_string(),
            logging: LoggingConfig::default(),
            runtime: RuntimeConfig::default(),
            paths: PathsConfig::default(),
            maintenance: MaintenanceConfig::default(),
            api: ApiConfig::default(),
            tui: TuiConfig::default(),
            watch: WatchConfig::default(),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub file: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file: None,
        }
    }
}

/// Runtime behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub parallelism: Option<usize>,
    pub timeout: Option<u64>,
    pub fail_fast: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            parallelism: None,
            timeout: Some(60),
            fail_fast: true,
        }
    }
}

/// Path override configuration.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    pub data_dir: Option<String>,
    pub state_dir: Option<String>,
}

/// Maintenance configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintenanceConfig {
    pub gc_interval_seconds: u64,
    pub message_retention_days: u64,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            gc_interval_seconds: 300,
            message_retention_days: 30,
        }
    }
}

/// API server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub admin_key: Option<String>,
    pub rate_limit_per_minute: u64,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            admin_key: None,
            rate_limit_per_minute: 60,
        }
    }
}

/// TUI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub poll_interval_seconds: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 5,
        }
    }
}

/// Watcher configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    pub poll_interval_seconds: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: 5,
        }
    }
}
