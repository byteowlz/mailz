//! Core data models for mailz agent coordination.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Importance level for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

impl Importance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Urgent => "urgent",
        }
    }
}

impl std::str::FromStr for Importance {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "normal" => Ok(Self::Normal),
            "high" => Ok(Self::High),
            "urgent" => Ok(Self::Urgent),
            _ => Err(format!("unknown importance: {s}")),
        }
    }
}

/// A project represents a workspace/repository context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub slug: String,
    pub path: String,
    pub created_at: DateTime<Utc>,
}

/// An agent registered in a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub program: String,
    pub model: String,
    pub task_description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

/// A message between agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub project_id: i64,
    pub sender_id: i64,
    pub thread_id: Option<String>,
    pub subject: String,
    pub body: String,
    pub importance: Importance,
    pub ack_required: bool,
    pub created_at: DateTime<Utc>,
}

/// Recipient of a message (to, cc, bcc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecipientKind {
    To,
    Cc,
    Bcc,
}

/// A message recipient record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecipient {
    pub id: i64,
    pub message_id: i64,
    pub agent_id: i64,
    pub kind: RecipientKind,
    pub read_at: Option<DateTime<Utc>>,
    pub ack_at: Option<DateTime<Utc>>,
}

/// An advisory file reservation (lease).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReservation {
    pub id: i64,
    pub project_id: i64,
    pub agent_id: i64,
    pub path_pattern: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

impl FileReservation {
    /// Check if the reservation is currently active.
    pub fn is_active(&self) -> bool {
        self.released_at.is_none() && self.expires_at > Utc::now()
    }
}

/// Input for creating a new agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgent {
    pub name: Option<String>,
    pub program: String,
    pub model: String,
    pub task_description: Option<String>,
}

/// Input for sending a new message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessage {
    pub sender_name: String,
    pub to: Vec<String>,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub subject: String,
    pub body: String,
    pub importance: Option<Importance>,
    pub ack_required: Option<bool>,
    pub thread_id: Option<String>,
}

/// Input for creating a file reservation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFileReservation {
    pub agent_name: String,
    pub paths: Vec<String>,
    pub ttl_seconds: Option<u64>,
    pub exclusive: Option<bool>,
    pub reason: Option<String>,
}

/// Result of a file reservation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReservationResult {
    pub granted: Vec<FileReservation>,
    pub conflicts: Vec<FileReservation>,
}

/// Summary of an agent's inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxSummary {
    pub agent: String,
    pub total: usize,
    pub unread: usize,
    pub urgent: usize,
    pub pending_acks: usize,
}

/// A message with sender and recipient info for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageView {
    pub id: i64,
    pub sender: String,
    pub recipients: Vec<String>,
    pub subject: String,
    pub body: String,
    pub importance: Importance,
    pub ack_required: bool,
    pub thread_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
    pub ack_at: Option<DateTime<Utc>>,
}

/// Stored API key metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: i64,
    pub agent_id: i64,
    pub key_prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Result of issuing a new API key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyIssued {
    pub api_key: String,
    pub record: ApiKeyRecord,
}

/// Summary of a GC run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcSummary {
    pub expired_reservations: usize,
    pub deleted_messages: usize,
    pub message_cutoff: DateTime<Utc>,
}

/// Generate a memorable agent name (adjective + noun).
pub fn generate_agent_name() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    const ADJECTIVES: &[&str] = &[
        "Blue", "Green", "Red", "Swift", "Calm", "Bright", "Silent", "Bold", "Wise", "Keen",
        "Quick", "Sharp", "Cool", "Warm", "Clear", "Deep",
    ];
    const NOUNS: &[&str] = &[
        "Lake", "Mountain", "River", "Forest", "Castle", "Tower", "Valley", "Mesa", "Storm",
        "Cloud", "Star", "Moon", "Eagle", "Wolf", "Bear", "Hawk",
    ];

    let id = Uuid::new_v4();
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    let hash = hasher.finish();

    let adj = ADJECTIVES[(hash as usize) % ADJECTIVES.len()];
    let noun = NOUNS[((hash >> 32) as usize) % NOUNS.len()];

    format!("{adj}{noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_importance_roundtrip() {
        for imp in [
            Importance::Low,
            Importance::Normal,
            Importance::High,
            Importance::Urgent,
        ] {
            let s = imp.as_str();
            let parsed: Importance = s.parse().unwrap();
            assert_eq!(imp, parsed);
        }
    }

    #[test]
    fn test_generate_agent_name() {
        let name = generate_agent_name();
        assert!(!name.is_empty());
        // Should be CamelCase with two words
        assert!(name.chars().next().unwrap().is_uppercase());
    }
}
