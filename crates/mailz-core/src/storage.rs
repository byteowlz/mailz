//! SQLite storage layer with FTS5 for message search.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::models::*;

/// Database connection wrapper.
pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Open or create a database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;
        
        let storage = Self { conn };
        storage.init_schema()?;
        Ok(storage)
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let storage = Self { conn };
        storage.init_schema()?;
        Ok(storage)
    }

    /// Initialize the database schema.
    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            -- Projects table
            CREATE TABLE IF NOT EXISTS projects (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL UNIQUE,
                path TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            -- Agents table
            CREATE TABLE IF NOT EXISTS agents (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL REFERENCES projects(id),
                name TEXT NOT NULL,
                program TEXT NOT NULL,
                model TEXT NOT NULL,
                task_description TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_active_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(project_id, name)
            );

            -- Messages table
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL REFERENCES projects(id),
                sender_id INTEGER NOT NULL REFERENCES agents(id),
                thread_id TEXT,
                subject TEXT NOT NULL,
                body TEXT NOT NULL,
                importance TEXT NOT NULL DEFAULT 'normal',
                ack_required INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            -- Message recipients
            CREATE TABLE IF NOT EXISTS message_recipients (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL REFERENCES messages(id),
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                kind TEXT NOT NULL DEFAULT 'to',
                read_at TEXT,
                ack_at TEXT
            );

            -- File reservations
            CREATE TABLE IF NOT EXISTS file_reservations (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL REFERENCES projects(id),
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                path_pattern TEXT NOT NULL,
                exclusive INTEGER NOT NULL DEFAULT 1,
                reason TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                expires_at TEXT NOT NULL,
                released_at TEXT
            );

            -- Indexes
            CREATE INDEX IF NOT EXISTS idx_agents_project ON agents(project_id);
            CREATE INDEX IF NOT EXISTS idx_messages_project ON messages(project_id);
            CREATE INDEX IF NOT EXISTS idx_messages_sender ON messages(sender_id);
            CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id);
            CREATE INDEX IF NOT EXISTS idx_recipients_message ON message_recipients(message_id);
            CREATE INDEX IF NOT EXISTS idx_recipients_agent ON message_recipients(agent_id);
            CREATE INDEX IF NOT EXISTS idx_reservations_project ON file_reservations(project_id);
            CREATE INDEX IF NOT EXISTS idx_reservations_agent ON file_reservations(agent_id);

            -- FTS5 virtual table for message search
            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                subject,
                body,
                content=messages,
                content_rowid=id
            );

            -- Triggers to keep FTS in sync
            CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, subject, body) VALUES (new.id, new.subject, new.body);
            END;

            CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, subject, body) VALUES('delete', old.id, old.subject, old.body);
            END;

            CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, subject, body) VALUES('delete', old.id, old.subject, old.body);
                INSERT INTO messages_fts(rowid, subject, body) VALUES (new.id, new.subject, new.body);
            END;
            "#,
        )?;
        Ok(())
    }

    // =========================================================================
    // Projects
    // =========================================================================

    /// Get or create a project by path.
    pub fn ensure_project(&self, path: &str) -> Result<Project> {
        let slug = generate_slug(path);
        
        self.conn.execute(
            "INSERT OR IGNORE INTO projects (slug, path) VALUES (?1, ?2)",
            params![slug, path],
        )?;

        self.get_project_by_path(path)?
            .ok_or_else(|| anyhow::anyhow!("failed to create project"))
    }

    /// Get a project by path.
    pub fn get_project_by_path(&self, path: &str) -> Result<Option<Project>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, slug, path, created_at FROM projects WHERE path = ?1"
        )?;

        let result = stmt.query_row(params![path], |row| {
            Ok(Project {
                id: row.get(0)?,
                slug: row.get(1)?,
                path: row.get(2)?,
                created_at: parse_datetime(&row.get::<_, String>(3)?),
            })
        });

        match result {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // =========================================================================
    // Agents
    // =========================================================================

    /// Register or update an agent.
    pub fn register_agent(&self, project_id: i64, input: &CreateAgent) -> Result<Agent> {
        let name = input.name.clone().unwrap_or_else(generate_agent_name);
        let now = Utc::now().to_rfc3339();

        self.conn.execute(
            r#"
            INSERT INTO agents (project_id, name, program, model, task_description, last_active_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(project_id, name) DO UPDATE SET
                program = excluded.program,
                model = excluded.model,
                task_description = COALESCE(excluded.task_description, task_description),
                last_active_at = excluded.last_active_at
            "#,
            params![project_id, name, input.program, input.model, input.task_description, now],
        )?;

        self.get_agent_by_name(project_id, &name)?
            .ok_or_else(|| anyhow::anyhow!("failed to register agent"))
    }

    /// Get an agent by name within a project.
    pub fn get_agent_by_name(&self, project_id: i64, name: &str) -> Result<Option<Agent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, program, model, task_description, created_at, last_active_at 
             FROM agents WHERE project_id = ?1 AND name = ?2"
        )?;

        let result = stmt.query_row(params![project_id, name], |row| {
            Ok(Agent {
                id: row.get(0)?,
                project_id: row.get(1)?,
                name: row.get(2)?,
                program: row.get(3)?,
                model: row.get(4)?,
                task_description: row.get(5)?,
                created_at: parse_datetime(&row.get::<_, String>(6)?),
                last_active_at: parse_datetime(&row.get::<_, String>(7)?),
            })
        });

        match result {
            Ok(a) => Ok(Some(a)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List agents in a project.
    pub fn list_agents(&self, project_id: i64) -> Result<Vec<Agent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, program, model, task_description, created_at, last_active_at 
             FROM agents WHERE project_id = ?1 ORDER BY last_active_at DESC"
        )?;

        let rows = stmt.query_map(params![project_id], |row| {
            Ok(Agent {
                id: row.get(0)?,
                project_id: row.get(1)?,
                name: row.get(2)?,
                program: row.get(3)?,
                model: row.get(4)?,
                task_description: row.get(5)?,
                created_at: parse_datetime(&row.get::<_, String>(6)?),
                last_active_at: parse_datetime(&row.get::<_, String>(7)?),
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Update agent's last active timestamp.
    pub fn touch_agent(&self, agent_id: i64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE agents SET last_active_at = ?1 WHERE id = ?2",
            params![now, agent_id],
        )?;
        Ok(())
    }

    // =========================================================================
    // Messages
    // =========================================================================

    /// Send a message.
    pub fn send_message(&self, project_id: i64, input: &SendMessage) -> Result<Message> {
        // Get sender
        let sender = self.get_agent_by_name(project_id, &input.sender_name)?
            .ok_or_else(|| anyhow::anyhow!("sender '{}' not found", input.sender_name))?;

        let importance = input.importance.unwrap_or_default();
        let ack_required = input.ack_required.unwrap_or(false);
        let now = Utc::now().to_rfc3339();

        self.conn.execute(
            "INSERT INTO messages (project_id, sender_id, thread_id, subject, body, importance, ack_required, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                project_id,
                sender.id,
                input.thread_id,
                input.subject,
                input.body,
                importance.as_str(),
                ack_required,
                now
            ],
        )?;

        let message_id = self.conn.last_insert_rowid();

        // Add recipients
        for name in &input.to {
            self.add_recipient(project_id, message_id, name, RecipientKind::To)?;
        }
        if let Some(cc) = &input.cc {
            for name in cc {
                self.add_recipient(project_id, message_id, name, RecipientKind::Cc)?;
            }
        }
        if let Some(bcc) = &input.bcc {
            for name in bcc {
                self.add_recipient(project_id, message_id, name, RecipientKind::Bcc)?;
            }
        }

        // Touch sender activity
        self.touch_agent(sender.id)?;

        self.get_message(message_id)?
            .ok_or_else(|| anyhow::anyhow!("failed to send message"))
    }

    fn add_recipient(&self, project_id: i64, message_id: i64, name: &str, kind: RecipientKind) -> Result<()> {
        let agent = self.get_agent_by_name(project_id, name)?
            .ok_or_else(|| anyhow::anyhow!("recipient '{}' not found", name))?;

        let kind_str = match kind {
            RecipientKind::To => "to",
            RecipientKind::Cc => "cc",
            RecipientKind::Bcc => "bcc",
        };

        self.conn.execute(
            "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (?1, ?2, ?3)",
            params![message_id, agent.id, kind_str],
        )?;

        Ok(())
    }

    /// Get a message by ID.
    pub fn get_message(&self, id: i64) -> Result<Option<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, sender_id, thread_id, subject, body, importance, ack_required, created_at
             FROM messages WHERE id = ?1"
        )?;

        let result = stmt.query_row(params![id], |row| {
            Ok(Message {
                id: row.get(0)?,
                project_id: row.get(1)?,
                sender_id: row.get(2)?,
                thread_id: row.get(3)?,
                subject: row.get(4)?,
                body: row.get(5)?,
                importance: row.get::<_, String>(6)?.parse().unwrap_or_default(),
                ack_required: row.get(7)?,
                created_at: parse_datetime(&row.get::<_, String>(8)?),
            })
        });

        match result {
            Ok(m) => Ok(Some(m)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch inbox for an agent.
    pub fn fetch_inbox(&self, project_id: i64, agent_name: &str, limit: usize) -> Result<Vec<MessageView>> {
        let agent = self.get_agent_by_name(project_id, agent_name)?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

        let mut stmt = self.conn.prepare(
            r#"
            SELECT m.id, a.name as sender, m.subject, m.body, m.importance, m.ack_required, 
                   m.thread_id, m.created_at, mr.read_at, mr.ack_at
            FROM messages m
            JOIN agents a ON m.sender_id = a.id
            JOIN message_recipients mr ON mr.message_id = m.id
            WHERE mr.agent_id = ?1 AND m.project_id = ?2
            ORDER BY m.created_at DESC
            LIMIT ?3
            "#
        )?;

        let rows = stmt.query_map(params![agent.id, project_id, limit as i64], |row| {
            Ok(MessageView {
                id: row.get(0)?,
                sender: row.get(1)?,
                recipients: vec![], // Would need a subquery to fill
                subject: row.get(2)?,
                body: row.get(3)?,
                importance: row.get::<_, String>(4)?.parse().unwrap_or_default(),
                ack_required: row.get(5)?,
                thread_id: row.get(6)?,
                created_at: parse_datetime(&row.get::<_, String>(7)?),
                read_at: row.get::<_, Option<String>>(8)?.map(|s| parse_datetime(&s)),
                ack_at: row.get::<_, Option<String>>(9)?.map(|s| parse_datetime(&s)),
            })
        })?;

        // Touch agent activity
        self.touch_agent(agent.id)?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Mark a message as read.
    pub fn mark_read(&self, agent_id: i64, message_id: i64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE message_recipients SET read_at = ?1 WHERE agent_id = ?2 AND message_id = ?3 AND read_at IS NULL",
            params![now, agent_id, message_id],
        )?;
        Ok(())
    }

    /// Acknowledge a message.
    pub fn acknowledge(&self, agent_id: i64, message_id: i64) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE message_recipients SET ack_at = ?1, read_at = COALESCE(read_at, ?1) 
             WHERE agent_id = ?2 AND message_id = ?3",
            params![now, agent_id, message_id],
        )?;
        Ok(())
    }

    /// Search messages using FTS5.
    pub fn search_messages(&self, project_id: i64, query: &str, limit: usize) -> Result<Vec<MessageView>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT m.id, a.name as sender, m.subject, m.body, m.importance, m.ack_required,
                   m.thread_id, m.created_at
            FROM messages m
            JOIN agents a ON m.sender_id = a.id
            JOIN messages_fts ON messages_fts.rowid = m.id
            WHERE m.project_id = ?1 AND messages_fts MATCH ?2
            ORDER BY bm25(messages_fts) 
            LIMIT ?3
            "#
        )?;

        let rows = stmt.query_map(params![project_id, query, limit as i64], |row| {
            Ok(MessageView {
                id: row.get(0)?,
                sender: row.get(1)?,
                recipients: vec![],
                subject: row.get(2)?,
                body: row.get(3)?,
                importance: row.get::<_, String>(4)?.parse().unwrap_or_default(),
                ack_required: row.get(5)?,
                thread_id: row.get(6)?,
                created_at: parse_datetime(&row.get::<_, String>(7)?),
                read_at: None,
                ack_at: None,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // =========================================================================
    // File Reservations
    // =========================================================================

    /// Create file reservations.
    pub fn create_file_reservations(&self, project_id: i64, input: &CreateFileReservation) -> Result<FileReservationResult> {
        let agent = self.get_agent_by_name(project_id, &input.agent_name)?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", input.agent_name))?;

        let ttl = input.ttl_seconds.unwrap_or(3600);
        let exclusive = input.exclusive.unwrap_or(true);
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl as i64);

        let mut granted = Vec::new();
        let mut conflicts = Vec::new();

        for path in &input.paths {
            // Check for conflicts if exclusive
            if exclusive {
                let existing = self.get_active_reservations_for_path(project_id, path)?;
                for res in existing {
                    if res.agent_id != agent.id && res.exclusive {
                        conflicts.push(res);
                    }
                }
            }

            // Create the reservation anyway (advisory)
            self.conn.execute(
                "INSERT INTO file_reservations (project_id, agent_id, path_pattern, exclusive, reason, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    project_id,
                    agent.id,
                    path,
                    exclusive,
                    input.reason,
                    expires_at.to_rfc3339()
                ],
            )?;

            let id = self.conn.last_insert_rowid();
            granted.push(FileReservation {
                id,
                project_id,
                agent_id: agent.id,
                path_pattern: path.clone(),
                exclusive,
                reason: input.reason.clone(),
                created_at: now,
                expires_at,
                released_at: None,
            });
        }

        self.touch_agent(agent.id)?;

        Ok(FileReservationResult { granted, conflicts })
    }

    /// Get active reservations that might conflict with a path.
    fn get_active_reservations_for_path(&self, project_id: i64, path: &str) -> Result<Vec<FileReservation>> {
        let now = Utc::now().to_rfc3339();
        
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, project_id, agent_id, path_pattern, exclusive, reason, created_at, expires_at, released_at
            FROM file_reservations
            WHERE project_id = ?1 AND released_at IS NULL AND expires_at > ?2
              AND (path_pattern = ?3 OR ?3 GLOB path_pattern OR path_pattern GLOB ?3)
            "#
        )?;

        let rows = stmt.query_map(params![project_id, now, path], Self::row_to_reservation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Release file reservations for an agent.
    pub fn release_reservations(&self, project_id: i64, agent_name: &str, paths: Option<&[String]>) -> Result<usize> {
        let agent = self.get_agent_by_name(project_id, agent_name)?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

        let now = Utc::now().to_rfc3339();

        let count = if let Some(paths) = paths {
            let mut total = 0;
            for path in paths {
                total += self.conn.execute(
                    "UPDATE file_reservations SET released_at = ?1 
                     WHERE project_id = ?2 AND agent_id = ?3 AND path_pattern = ?4 AND released_at IS NULL",
                    params![now, project_id, agent.id, path],
                )?;
            }
            total
        } else {
            self.conn.execute(
                "UPDATE file_reservations SET released_at = ?1 
                 WHERE project_id = ?2 AND agent_id = ?3 AND released_at IS NULL",
                params![now, project_id, agent.id],
            )?
        };

        Ok(count)
    }

    /// List active file reservations in a project.
    pub fn list_active_reservations(&self, project_id: i64) -> Result<Vec<FileReservation>> {
        let now = Utc::now().to_rfc3339();

        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, project_id, agent_id, path_pattern, exclusive, reason, created_at, expires_at, released_at
            FROM file_reservations
            WHERE project_id = ?1 AND released_at IS NULL AND expires_at > ?2
            ORDER BY created_at DESC
            "#
        )?;

        let rows = stmt.query_map(params![project_id, now], Self::row_to_reservation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    fn row_to_reservation(row: &rusqlite::Row) -> rusqlite::Result<FileReservation> {
        Ok(FileReservation {
            id: row.get(0)?,
            project_id: row.get(1)?,
            agent_id: row.get(2)?,
            path_pattern: row.get(3)?,
            exclusive: row.get(4)?,
            reason: row.get(5)?,
            created_at: parse_datetime(&row.get::<_, String>(6)?),
            expires_at: parse_datetime(&row.get::<_, String>(7)?),
            released_at: row.get::<_, Option<String>>(8)?.map(|s| parse_datetime(&s)),
        })
    }
}

/// Generate a slug from a path.
fn generate_slug(path: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let hash = hasher.finish();

    format!("{}-{:08x}", name, hash as u32)
}

/// Parse an RFC3339 datetime string.
fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_roundtrip() -> Result<()> {
        let storage = Storage::open_memory()?;

        // Create project
        let project = storage.ensure_project("/tmp/test")?;
        assert_eq!(project.path, "/tmp/test");

        // Register agent
        let agent = storage.register_agent(project.id, &CreateAgent {
            name: Some("TestAgent".to_string()),
            program: "test".to_string(),
            model: "gpt-4".to_string(),
            task_description: None,
        })?;
        assert_eq!(agent.name, "TestAgent");

        // Register another agent
        storage.register_agent(project.id, &CreateAgent {
            name: Some("OtherAgent".to_string()),
            program: "test".to_string(),
            model: "gpt-4".to_string(),
            task_description: None,
        })?;

        // Send message
        let msg = storage.send_message(project.id, &SendMessage {
            sender_name: "TestAgent".to_string(),
            to: vec!["OtherAgent".to_string()],
            cc: None,
            bcc: None,
            subject: "Hello".to_string(),
            body: "Test message body".to_string(),
            importance: None,
            ack_required: None,
            thread_id: None,
        })?;
        assert_eq!(msg.subject, "Hello");

        // Fetch inbox
        let inbox = storage.fetch_inbox(project.id, "OtherAgent", 10)?;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].subject, "Hello");

        // Search
        let results = storage.search_messages(project.id, "test", 10)?;
        assert_eq!(results.len(), 1);

        Ok(())
    }

    #[test]
    fn test_file_reservations() -> Result<()> {
        let storage = Storage::open_memory()?;
        let project = storage.ensure_project("/tmp/test")?;

        storage.register_agent(project.id, &CreateAgent {
            name: Some("Agent1".to_string()),
            program: "test".to_string(),
            model: "gpt-4".to_string(),
            task_description: None,
        })?;

        storage.register_agent(project.id, &CreateAgent {
            name: Some("Agent2".to_string()),
            program: "test".to_string(),
            model: "gpt-4".to_string(),
            task_description: None,
        })?;

        // Create exclusive reservation
        let result = storage.create_file_reservations(project.id, &CreateFileReservation {
            agent_name: "Agent1".to_string(),
            paths: vec!["src/*.rs".to_string()],
            ttl_seconds: Some(3600),
            exclusive: Some(true),
            reason: Some("refactoring".to_string()),
        })?;
        assert_eq!(result.granted.len(), 1);
        assert!(result.conflicts.is_empty());

        // Try to create conflicting reservation
        let result2 = storage.create_file_reservations(project.id, &CreateFileReservation {
            agent_name: "Agent2".to_string(),
            paths: vec!["src/*.rs".to_string()],
            ttl_seconds: Some(3600),
            exclusive: Some(true),
            reason: None,
        })?;
        assert_eq!(result2.granted.len(), 1); // Still granted (advisory)
        assert_eq!(result2.conflicts.len(), 1); // But conflict reported

        // Release
        let released = storage.release_reservations(project.id, "Agent1", None)?;
        assert_eq!(released, 1);

        Ok(())
    }
}
