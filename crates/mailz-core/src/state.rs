use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub project_slug: String,
    pub project_id: i64,
    pub agent_id: i64,
    pub agent_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DraftMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub project_slug: String,
    pub agent_name: String,
    pub started_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    pub poll_interval_seconds: u64,
}

pub fn load_agent_identity(paths: &AppPaths) -> Result<Option<AgentIdentity>> {
    load_json(paths.agent_identity_file().as_path())
}

pub fn save_agent_identity(paths: &AppPaths, identity: &AgentIdentity) -> Result<()> {
    save_json(paths.agent_identity_file().as_path(), identity)
}

pub fn load_draft(paths: &AppPaths) -> Result<Option<DraftMessage>> {
    load_json(paths.draft_file().as_path())
}

pub fn save_draft(paths: &AppPaths, draft: &DraftMessage) -> Result<()> {
    save_json(paths.draft_file().as_path(), draft)
}

pub fn clear_draft(paths: &AppPaths) -> Result<()> {
    if paths.draft_file().exists() {
        fs::remove_file(paths.draft_file())
            .with_context(|| format!("removing draft at {}", paths.draft_file().display()))?;
    }
    Ok(())
}

pub fn load_daemon_state(paths: &AppPaths) -> Result<Option<DaemonState>> {
    load_json(paths.daemon_state_file().as_path())
}

pub fn save_daemon_state(paths: &AppPaths, state: &DaemonState) -> Result<()> {
    save_json(paths.daemon_state_file().as_path(), state)
}

pub fn clear_daemon_state(paths: &AppPaths) -> Result<()> {
    if paths.daemon_state_file().exists() {
        fs::remove_file(paths.daemon_state_file()).with_context(|| {
            format!(
                "removing daemon state at {}",
                paths.daemon_state_file().display()
            )
        })?;
    }
    Ok(())
}

pub fn load_admin_key(paths: &AppPaths) -> Result<Option<String>> {
    load_text(paths.admin_key_file().as_path())
}

pub fn save_admin_key(paths: &AppPaths, key: &str) -> Result<()> {
    save_text(paths.admin_key_file().as_path(), key)
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(path)
        .with_context(|| format!("reading state file {}", path.display()))?;
    let parsed = serde_json::from_str(&data)
        .with_context(|| format!("parsing state file {}", path.display()))?;
    Ok(Some(parsed))
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(value)
        .with_context(|| format!("serializing state file {}", path.display()))?;
    fs::write(path, data).with_context(|| format!("writing state file {}", path.display()))
}

fn load_text(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = fs::read_to_string(path)
        .with_context(|| format!("reading state file {}", path.display()))?;
    Ok(Some(data.trim().to_string()))
}

fn save_text(path: &Path, value: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    fs::write(path, value).with_context(|| format!("writing state file {}", path.display()))
}
