//! SQLite storage layer with FTS5 for message search.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rand::RngCore;
use rusqlite::{Connection, params, params_from_iter};
use sha2::Digest;

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

        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let storage = Self { conn };
        storage.init_schema()?;
        Ok(storage)
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
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

            -- API keys table
            CREATE TABLE IF NOT EXISTS api_keys (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER NOT NULL REFERENCES agents(id),
                key_hash TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_used_at TEXT
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
            CREATE INDEX IF NOT EXISTS idx_api_keys_agent ON api_keys(agent_id);
            CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);
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
        self.create_project(path, None)
    }

    /// Create a project if it does not exist.
    pub fn create_project(&self, path: &str, slug: Option<&str>) -> Result<Project> {
        if let Some(existing) = self.get_project_by_path(path)? {
            if let Some(name) = slug {
                if existing.slug != name {
                    return Err(anyhow::anyhow!(
                        "project already exists with slug '{}'",
                        existing.slug
                    ));
                }
            }
            return Ok(existing);
        }

        let slug = slug
            .map(str::to_string)
            .unwrap_or_else(|| generate_slug(path));
        if self.get_project_by_slug(&slug)?.is_some() {
            return Err(anyhow::anyhow!("project slug '{slug}' already exists"));
        }

        self.conn.execute(
            "INSERT INTO projects (slug, path) VALUES (?1, ?2)",
            params![slug, path],
        )?;

        self.get_project_by_path(path)?
            .ok_or_else(|| anyhow::anyhow!("failed to create project"))
    }

    /// Get a project by path.
    pub fn get_project_by_path(&self, path: &str) -> Result<Option<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, path, created_at FROM projects WHERE path = ?1")?;

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

    /// Get a project by slug or name prefix.
    ///
    /// First tries exact slug match, then falls back to prefix match
    /// (e.g., "mailz" matches "mailz-bf820b21" if unambiguous).
    pub fn get_project_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        // Try exact match first
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, path, created_at FROM projects WHERE slug = ?1")?;

        let result = stmt.query_row(params![slug], |row| {
            Ok(Project {
                id: row.get(0)?,
                slug: row.get(1)?,
                path: row.get(2)?,
                created_at: parse_datetime(&row.get::<_, String>(3)?),
            })
        });

        match result {
            Ok(p) => return Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(e.into()),
        }

        // Try prefix match (name-*) for convenience
        let pattern = format!("{slug}-%");
        let mut stmt = self.conn.prepare(
            "SELECT id, slug, path, created_at FROM projects WHERE slug LIKE ?1",
        )?;

        let rows: Vec<Project> = stmt
            .query_map(params![pattern], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    path: row.get(2)?,
                    created_at: parse_datetime(&row.get::<_, String>(3)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some(rows.into_iter().next().unwrap())),
            _ => Err(anyhow::anyhow!(
                "ambiguous project name '{}', matches: {}",
                slug,
                rows.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// Get a project by id.
    pub fn get_project_by_id(&self, project_id: i64) -> Result<Option<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, path, created_at FROM projects WHERE id = ?1")?;

        let result = stmt.query_row(params![project_id], |row| {
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

    /// List all projects.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, slug, path, created_at FROM projects ORDER BY created_at DESC")?;

        let rows = stmt.query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                slug: row.get(1)?,
                path: row.get(2)?,
                created_at: parse_datetime(&row.get::<_, String>(3)?),
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Count agents per project.
    pub fn agent_counts_by_project(&self) -> Result<std::collections::HashMap<i64, usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT project_id, COUNT(*) FROM agents GROUP BY project_id")?;
        let rows = stmt.query_map([], |row| {
            let project_id: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((project_id, count as usize))
        })?;

        let mut counts = std::collections::HashMap::new();
        for row in rows {
            let (project_id, count) = row?;
            counts.insert(project_id, count);
        }
        Ok(counts)
    }

    /// Delete a project and related data.
    pub fn delete_project(&self, slug: &str) -> Result<bool> {
        let Some(project) = self.get_project_by_slug(slug)? else {
            return Ok(false);
        };

        self.conn.execute(
            "DELETE FROM message_recipients WHERE message_id IN (SELECT id FROM messages WHERE project_id = ?1)",
            params![project.id],
        )?;
        self.conn.execute(
            "DELETE FROM messages WHERE project_id = ?1",
            params![project.id],
        )?;
        self.conn.execute(
            "DELETE FROM file_reservations WHERE project_id = ?1",
            params![project.id],
        )?;
        self.conn.execute(
            "DELETE FROM agents WHERE project_id = ?1",
            params![project.id],
        )?;
        self.conn
            .execute("DELETE FROM projects WHERE id = ?1", params![project.id])?;

        Ok(true)
    }

    /// Delete a project by id and related data.
    pub fn delete_project_by_id(&self, project_id: i64) -> Result<bool> {
        let Some(project) = self.get_project_by_id(project_id)? else {
            return Ok(false);
        };
        self.delete_project(&project.slug)
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
            params![
                project_id,
                name,
                input.program,
                input.model,
                input.task_description,
                now
            ],
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

    /// Get an agent by ID.
    pub fn get_agent_by_id(&self, agent_id: i64) -> Result<Option<Agent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, name, program, model, task_description, created_at, last_active_at 
             FROM agents WHERE id = ?1"
        )?;

        let result = stmt.query_row(params![agent_id], |row| {
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
    // API Keys
    // =========================================================================

    /// Issue a new API key for an agent.
    pub fn issue_api_key(&self, agent_id: i64) -> Result<ApiKeyIssued> {
        let key = generate_api_key();
        let key_hash = hash_api_key(&key);
        let key_prefix = api_key_prefix(&key);
        let now = Utc::now().to_rfc3339();

        let updated = self.conn.execute(
            "INSERT INTO api_keys (agent_id, key_hash, created_at) VALUES (?1, ?2, ?3)",
            params![agent_id, key_hash, now],
        )?;
        if updated == 0 {
            return Err(anyhow::anyhow!("failed to create api key"));
        }

        let id = self.conn.last_insert_rowid();
        Ok(ApiKeyIssued {
            api_key: key,
            record: ApiKeyRecord {
                id,
                agent_id,
                key_prefix,
                created_at: parse_datetime(&now),
                last_used_at: None,
            },
        })
    }

    /// Verify an API key, returning the associated agent id if valid.
    pub fn verify_api_key(&self, key: &str) -> Result<Option<i64>> {
        let key_hash = hash_api_key(key);
        let mut stmt = self
            .conn
            .prepare("SELECT agent_id FROM api_keys WHERE key_hash = ?1")?;

        let result: rusqlite::Result<i64> = stmt.query_row(params![key_hash], |row| row.get(0));
        match result {
            Ok(agent_id) => {
                let now = Utc::now().to_rfc3339();
                self.conn.execute(
                    "UPDATE api_keys SET last_used_at = ?1 WHERE key_hash = ?2",
                    params![now, hash_api_key(key)],
                )?;
                Ok(Some(agent_id))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // =========================================================================
    // Messages
    // =========================================================================

    /// Send a message.
    pub fn send_message(&self, project_id: i64, input: &SendMessage) -> Result<Message> {
        // Get sender
        let sender = self
            .get_agent_by_name(project_id, &input.sender_name)?
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

    fn add_recipient(
        &self,
        project_id: i64,
        message_id: i64,
        name: &str,
        kind: RecipientKind,
    ) -> Result<()> {
        let agent = self
            .get_agent_by_name(project_id, name)?
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
    pub fn fetch_inbox(
        &self,
        project_id: i64,
        agent_name: &str,
        limit: usize,
        offset: usize,
        unread_only: bool,
    ) -> Result<Vec<MessageView>> {
        let agent = self
            .get_agent_by_name(project_id, agent_name)?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

        self.fetch_inbox_for_agent(
            agent.project_id,
            agent.id,
            limit,
            offset,
            unread_only,
            None,
            None,
        )
    }

    /// Fetch inbox for an agent by id with optional date filters.
    pub fn fetch_inbox_for_agent(
        &self,
        project_id: i64,
        agent_id: i64,
        limit: usize,
        offset: usize,
        unread_only: bool,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<MessageView>> {
        let mut sql = String::from(
            r#"
            SELECT m.id, a.name as sender, m.subject, m.body, m.importance, m.ack_required, 
                   m.thread_id, m.created_at, mr.read_at, mr.ack_at
            FROM messages m
            JOIN agents a ON m.sender_id = a.id
            JOIN message_recipients mr ON mr.message_id = m.id
            WHERE mr.agent_id = ? AND m.project_id = ?
            "#,
        );
        let mut params_vec: Vec<rusqlite::types::Value> = vec![agent_id.into(), project_id.into()];
        if unread_only {
            sql.push_str(" AND mr.read_at IS NULL");
        }
        if let Some(start) = start {
            sql.push_str(" AND m.created_at >= ?");
            params_vec.push(start.to_rfc3339().into());
        }
        if let Some(end) = end {
            sql.push_str(" AND m.created_at <= ?");
            params_vec.push(end.to_rfc3339().into());
        }
        sql.push_str(" ORDER BY m.created_at DESC LIMIT ? OFFSET ?");
        params_vec.push((limit as i64).into());
        params_vec.push((offset as i64).into());

        let mut stmt = self.conn.prepare(sql.as_str())?;

        let rows = stmt.query_map(params_from_iter(params_vec), |row| {
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
                read_at: row.get::<_, Option<String>>(8)?.map(|s| parse_datetime(&s)),
                ack_at: row.get::<_, Option<String>>(9)?.map(|s| parse_datetime(&s)),
            })
        })?;

        // Touch agent activity
        self.touch_agent(agent_id)?;

        let mut messages = rows.collect::<Result<Vec<_>, _>>()?;
        for message in &mut messages {
            message.recipients = self.list_message_recipients(message.id)?;
        }
        Ok(messages)
    }

    /// Summarize inbox counts for all agents in a project.
    pub fn list_inbox_summaries(&self, project_id: i64) -> Result<Vec<InboxSummary>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT a.name,
                   COALESCE(COUNT(mr.id), 0) AS total,
                   COALESCE(SUM(CASE WHEN mr.read_at IS NULL THEN 1 ELSE 0 END), 0) AS unread,
                   COALESCE(SUM(CASE WHEN m.importance = 'urgent' THEN 1 ELSE 0 END), 0) AS urgent,
                   COALESCE(SUM(CASE WHEN m.ack_required = 1 AND mr.ack_at IS NULL THEN 1 ELSE 0 END), 0) AS pending_acks
            FROM agents a
            LEFT JOIN message_recipients mr ON a.id = mr.agent_id
            LEFT JOIN messages m ON m.id = mr.message_id AND m.project_id = ?1
            WHERE a.project_id = ?1
            GROUP BY a.id
            ORDER BY a.last_active_at DESC
            "#,
        )?;

        let rows = stmt.query_map(params![project_id], |row| {
            Ok(InboxSummary {
                agent: row.get(0)?,
                total: row.get::<_, i64>(1)? as usize,
                unread: row.get::<_, i64>(2)? as usize,
                urgent: row.get::<_, i64>(3)? as usize,
                pending_acks: row.get::<_, i64>(4)? as usize,
            })
        })?;

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
    pub fn search_messages(
        &self,
        project_id: i64,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MessageView>> {
        self.search_messages_with_filters(project_id, query, limit, offset, None, false, None, None)
    }

    /// Search messages with optional filters.
    pub fn search_messages_with_filters(
        &self,
        project_id: i64,
        query: &str,
        limit: usize,
        offset: usize,
        agent_id: Option<i64>,
        unread_only: bool,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<MessageView>> {
        if unread_only && agent_id.is_none() {
            return Err(anyhow::anyhow!("unread filter requires agent_id"));
        }
        let mut sql = String::from(
            r#"
            SELECT m.id, a.name as sender, m.subject, m.body, m.importance, m.ack_required,
                   m.thread_id, m.created_at,
            "#,
        );
        if agent_id.is_some() {
            sql.push_str("mr.read_at, mr.ack_at ");
        } else {
            sql.push_str("NULL as read_at, NULL as ack_at ");
        }
        sql.push_str(
            r#"
            FROM messages m
            JOIN agents a ON m.sender_id = a.id
            JOIN messages_fts ON messages_fts.rowid = m.id
            "#,
        );
        let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(agent_id) = agent_id {
            sql.push_str(" JOIN message_recipients mr ON mr.message_id = m.id AND mr.agent_id = ?");
            params_vec.push(agent_id.into());
        }
        sql.push_str(" WHERE m.project_id = ? AND messages_fts MATCH ?");
        params_vec.push(project_id.into());
        params_vec.push(query.to_string().into());
        if unread_only {
            sql.push_str(" AND mr.read_at IS NULL");
        }
        if let Some(start) = start {
            sql.push_str(" AND m.created_at >= ?");
            params_vec.push(start.to_rfc3339().into());
        }
        if let Some(end) = end {
            sql.push_str(" AND m.created_at <= ?");
            params_vec.push(end.to_rfc3339().into());
        }
        sql.push_str(" ORDER BY bm25(messages_fts) LIMIT ? OFFSET ?");
        params_vec.push((limit as i64).into());
        params_vec.push((offset as i64).into());

        let mut stmt = self.conn.prepare(sql.as_str())?;

        let rows = stmt.query_map(params_from_iter(params_vec), |row| {
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
                read_at: row.get::<_, Option<String>>(8)?.map(|s| parse_datetime(&s)),
                ack_at: row.get::<_, Option<String>>(9)?.map(|s| parse_datetime(&s)),
            })
        })?;

        let mut messages = rows.collect::<Result<Vec<_>, _>>()?;
        for message in &mut messages {
            message.recipients = self.list_message_recipients(message.id)?;
        }
        Ok(messages)
    }

    /// Get a message view scoped to an agent.
    pub fn get_message_view(
        &self,
        project_id: i64,
        agent_id: i64,
        message_id: i64,
    ) -> Result<Option<MessageView>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT m.id, a.name as sender, m.subject, m.body, m.importance, m.ack_required,
                   m.thread_id, m.created_at, mr.read_at, mr.ack_at
            FROM messages m
            JOIN agents a ON m.sender_id = a.id
            JOIN message_recipients mr ON mr.message_id = m.id
            WHERE m.project_id = ?1 AND m.id = ?2 AND mr.agent_id = ?3
            "#,
        )?;

        let result = stmt.query_row(params![project_id, message_id, agent_id], |row| {
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
                read_at: row.get::<_, Option<String>>(8)?.map(|s| parse_datetime(&s)),
                ack_at: row.get::<_, Option<String>>(9)?.map(|s| parse_datetime(&s)),
            })
        });

        match result {
            Ok(mut message) => {
                message.recipients = self.list_message_recipients(message.id)?;
                Ok(Some(message))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get a message view without per-agent state.
    pub fn get_message_view_unscoped(&self, message_id: i64) -> Result<Option<MessageView>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT m.id, a.name as sender, m.subject, m.body, m.importance, m.ack_required,
                   m.thread_id, m.created_at
            FROM messages m
            JOIN agents a ON m.sender_id = a.id
            WHERE m.id = ?1
            "#,
        )?;

        let result = stmt.query_row(params![message_id], |row| {
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
        });

        match result {
            Ok(mut message) => {
                message.recipients = self.list_message_recipients(message.id)?;
                Ok(Some(message))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Mark a message as unread.
    pub fn mark_unread(&self, agent_id: i64, message_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE message_recipients SET read_at = NULL WHERE agent_id = ?1 AND message_id = ?2",
            params![agent_id, message_id],
        )?;
        Ok(())
    }

    fn list_message_recipients(&self, message_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT a.name
            FROM message_recipients mr
            JOIN agents a ON mr.agent_id = a.id
            WHERE mr.message_id = ?1
            ORDER BY mr.id ASC
            "#,
        )?;

        let rows = stmt.query_map(params![message_id], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // =========================================================================
    // File Reservations
    // =========================================================================

    /// Create file reservations.
    pub fn create_file_reservations(
        &self,
        project_id: i64,
        input: &CreateFileReservation,
    ) -> Result<FileReservationResult> {
        let agent = self
            .get_agent_by_name(project_id, &input.agent_name)?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", input.agent_name))?;

        let ttl = input.ttl_seconds.unwrap_or(1800);
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
    fn get_active_reservations_for_path(
        &self,
        project_id: i64,
        path: &str,
    ) -> Result<Vec<FileReservation>> {
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
    pub fn release_reservations(
        &self,
        project_id: i64,
        agent_name: &str,
        paths: Option<&[String]>,
    ) -> Result<usize> {
        let agent = self
            .get_agent_by_name(project_id, agent_name)?
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

    /// Release a reservation by ID for an agent.
    pub fn release_reservation_by_id(
        &self,
        project_id: i64,
        agent_name: &str,
        reservation_id: i64,
    ) -> Result<bool> {
        let agent = self
            .get_agent_by_name(project_id, agent_name)?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE file_reservations SET released_at = ?1 
             WHERE project_id = ?2 AND agent_id = ?3 AND id = ?4 AND released_at IS NULL",
            params![now, project_id, agent.id, reservation_id],
        )?;

        Ok(updated > 0)
    }

    /// Release a reservation by ID without agent scoping.
    pub fn release_reservation_by_id_any(&self, reservation_id: i64) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE file_reservations SET released_at = ?1 WHERE id = ?2 AND released_at IS NULL",
            params![now, reservation_id],
        )?;
        Ok(updated > 0)
    }

    /// Get a reservation by ID.
    pub fn get_reservation_by_id(&self, reservation_id: i64) -> Result<Option<FileReservation>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, project_id, agent_id, path_pattern, exclusive, reason, created_at, expires_at, released_at
            FROM file_reservations
            WHERE id = ?1
            "#,
        )?;

        let result = stmt.query_row(params![reservation_id], Self::row_to_reservation);
        match result {
            Ok(reservation) => Ok(Some(reservation)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List active file reservations in a project.
    pub fn list_active_reservations(
        &self,
        project_id: i64,
        agent_id: Option<i64>,
        path: Option<&str>,
    ) -> Result<Vec<FileReservation>> {
        let now = Utc::now().to_rfc3339();
        let mut sql = String::from(
            r#"
            SELECT id, project_id, agent_id, path_pattern, exclusive, reason, created_at, expires_at, released_at
            FROM file_reservations
            WHERE project_id = ?1 AND released_at IS NULL AND expires_at > ?2
            "#,
        );

        if agent_id.is_some() {
            sql.push_str(" AND agent_id = ?3");
        }
        if path.is_some() {
            sql.push_str(
                " AND (path_pattern = ?4 OR ?4 GLOB path_pattern OR path_pattern GLOB ?4)",
            );
        }
        sql.push_str(" ORDER BY created_at DESC");

        let mut stmt = self.conn.prepare(sql.as_str())?;

        let mut params_vec: Vec<rusqlite::types::Value> = vec![project_id.into(), now.into()];
        if let Some(agent_id) = agent_id {
            params_vec.push(agent_id.into());
        }
        if let Some(path) = path {
            params_vec.push(path.to_string().into());
        }

        let rows = stmt.query_map(params_from_iter(params_vec), Self::row_to_reservation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Check reservations for a list of paths.
    pub fn check_reservations(
        &self,
        project_id: i64,
        paths: &[String],
    ) -> Result<Vec<FileReservation>> {
        let mut conflicts = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            for reservation in self.get_active_reservations_for_path(project_id, path)? {
                if seen.insert(reservation.id) {
                    conflicts.push(reservation);
                }
            }
        }
        Ok(conflicts)
    }

    /// Expire reservations that have passed their TTL.
    pub fn expire_reservations(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let updated = self.conn.execute(
            "UPDATE file_reservations SET released_at = ?1 WHERE released_at IS NULL AND expires_at <= ?2",
            params![now, now],
        )?;
        Ok(updated)
    }

    /// Delete messages created before the cutoff timestamp.
    pub fn cleanup_messages_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let cutoff_str = cutoff.to_rfc3339();
        self.conn.execute(
            "DELETE FROM message_recipients WHERE message_id IN (SELECT id FROM messages WHERE created_at < ?1)",
            params![cutoff_str],
        )?;
        let deleted = self.conn.execute(
            "DELETE FROM messages WHERE created_at < ?1",
            params![cutoff_str],
        )?;
        Ok(deleted)
    }

    /// Run GC for expired reservations and old messages.
    pub fn run_gc(&self, retention_days: u64) -> Result<GcSummary> {
        let expired = self.expire_reservations()?;
        let cutoff = Utc::now() - chrono::Duration::days(retention_days as i64);
        let deleted = self.cleanup_messages_before(cutoff)?;
        Ok(GcSummary {
            expired_reservations: expired,
            deleted_messages: deleted,
            message_cutoff: cutoff,
        })
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

fn generate_api_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("mk_{}", hex::encode(bytes))
}

fn hash_api_key(key: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

fn api_key_prefix(key: &str) -> String {
    key.chars().take(8).collect()
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
        let agent = storage.register_agent(
            project.id,
            &CreateAgent {
                name: Some("TestAgent".to_string()),
                program: "test".to_string(),
                model: "gpt-4".to_string(),
                task_description: None,
            },
        )?;
        assert_eq!(agent.name, "TestAgent");

        // Register another agent
        storage.register_agent(
            project.id,
            &CreateAgent {
                name: Some("OtherAgent".to_string()),
                program: "test".to_string(),
                model: "gpt-4".to_string(),
                task_description: None,
            },
        )?;

        // Send message
        let msg = storage.send_message(
            project.id,
            &SendMessage {
                sender_name: "TestAgent".to_string(),
                to: vec!["OtherAgent".to_string()],
                cc: None,
                bcc: None,
                subject: "Hello".to_string(),
                body: "Test message body".to_string(),
                importance: None,
                ack_required: None,
                thread_id: None,
            },
        )?;
        assert_eq!(msg.subject, "Hello");

        // Fetch inbox
        let inbox = storage.fetch_inbox(project.id, "OtherAgent", 10, 0, false)?;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].subject, "Hello");

        // Search
        let results = storage.search_messages(project.id, "test", 10, 0)?;
        assert_eq!(results.len(), 1);

        Ok(())
    }

    #[test]
    fn test_file_reservations() -> Result<()> {
        let storage = Storage::open_memory()?;
        let project = storage.ensure_project("/tmp/test")?;

        storage.register_agent(
            project.id,
            &CreateAgent {
                name: Some("Agent1".to_string()),
                program: "test".to_string(),
                model: "gpt-4".to_string(),
                task_description: None,
            },
        )?;

        storage.register_agent(
            project.id,
            &CreateAgent {
                name: Some("Agent2".to_string()),
                program: "test".to_string(),
                model: "gpt-4".to_string(),
                task_description: None,
            },
        )?;

        // Create exclusive reservation
        let result = storage.create_file_reservations(
            project.id,
            &CreateFileReservation {
                agent_name: "Agent1".to_string(),
                paths: vec!["src/*.rs".to_string()],
                ttl_seconds: Some(3600),
                exclusive: Some(true),
                reason: Some("refactoring".to_string()),
            },
        )?;
        assert_eq!(result.granted.len(), 1);
        assert!(result.conflicts.is_empty());

        // Try to create conflicting reservation
        let result2 = storage.create_file_reservations(
            project.id,
            &CreateFileReservation {
                agent_name: "Agent2".to_string(),
                paths: vec!["src/*.rs".to_string()],
                ttl_seconds: Some(3600),
                exclusive: Some(true),
                reason: None,
            },
        )?;
        assert_eq!(result2.granted.len(), 1); // Still granted (advisory)
        assert_eq!(result2.conflicts.len(), 1); // But conflict reported

        // Release
        let released = storage.release_reservations(project.id, "Agent1", None)?;
        assert_eq!(released, 1);

        Ok(())
    }

    #[test]
    fn test_projects_crud() -> Result<()> {
        let storage = Storage::open_memory()?;

        let created = storage.create_project("/tmp/mailz", Some("mailz"))?;
        assert_eq!(created.slug, "mailz");

        let fetched = storage
            .get_project_by_slug("mailz")?
            .expect("project missing");
        assert_eq!(fetched.id, created.id);

        let list = storage.list_projects()?;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].slug, "mailz");

        let deleted = storage.delete_project("mailz")?;
        assert!(deleted);
        assert!(storage.get_project_by_slug("mailz")?.is_none());

        Ok(())
    }

    #[test]
    fn test_gc_expiry_and_retention() -> Result<()> {
        let storage = Storage::open_memory()?;
        let project = storage.ensure_project("/tmp/test")?;

        storage.register_agent(
            project.id,
            &CreateAgent {
                name: Some("Agent1".to_string()),
                program: "test".to_string(),
                model: "gpt-4".to_string(),
                task_description: None,
            },
        )?;

        storage.create_file_reservations(
            project.id,
            &CreateFileReservation {
                agent_name: "Agent1".to_string(),
                paths: vec!["src/lib.rs".to_string()],
                ttl_seconds: Some(0),
                exclusive: Some(true),
                reason: None,
            },
        )?;

        let message = storage.send_message(
            project.id,
            &SendMessage {
                sender_name: "Agent1".to_string(),
                to: vec!["Agent1".to_string()],
                cc: None,
                bcc: None,
                subject: "Old".to_string(),
                body: "Stale".to_string(),
                importance: None,
                ack_required: None,
                thread_id: None,
            },
        )?;

        let cutoff = Utc::now() - chrono::Duration::days(60);
        storage.conn.execute(
            "UPDATE messages SET created_at = ?1 WHERE id = ?2",
            params![cutoff.to_rfc3339(), message.id],
        )?;

        let summary = storage.run_gc(30)?;
        assert_eq!(summary.deleted_messages, 1);
        assert_eq!(summary.expired_reservations, 1);

        Ok(())
    }

    #[test]
    fn test_api_key_issue_and_verify() -> Result<()> {
        let storage = Storage::open_memory()?;
        let project = storage.ensure_project("/tmp/test")?;
        let agent = storage.register_agent(
            project.id,
            &CreateAgent {
                name: Some("Agent1".to_string()),
                program: "test".to_string(),
                model: "gpt-4".to_string(),
                task_description: None,
            },
        )?;

        let issued = storage.issue_api_key(agent.id)?;
        let verified = storage.verify_api_key(&issued.api_key)?;
        assert_eq!(verified, Some(agent.id));

        let invalid = storage.verify_api_key("invalid")?;
        assert!(invalid.is_none());

        Ok(())
    }
}
