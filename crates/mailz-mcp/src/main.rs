//! MCP server for mailz agent coordination.
//!
//! Provides tools for agent registration, messaging, and file reservations.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{Args, Parser};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::io::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use mailz_core::{AppConfig, AppPaths, CreateAgent, CreateFileReservation, SendMessage, Storage};

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover(cli.common.config)?;
    let _config = AppConfig::load(&paths, false)?;

    // Open database
    let db_path = paths.data_dir.join("mailz.db");
    std::fs::create_dir_all(&paths.data_dir)?;
    let storage = Storage::open(&db_path)?;

    let server = McpServer::new(storage);
    let transport = stdio();

    server
        .serve(transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    Ok(())
}

#[derive(Debug, Parser)]
#[command(author, version, about = "MCP server for mailz agent coordination")]
struct Cli {
    #[command(flatten)]
    common: CommonOpts,
}

#[derive(Debug, Clone, Args)]
struct CommonOpts {
    /// Override the config file path
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

// ============================================================================
// Tool Parameter Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct EnsureProjectParams {
    /// Absolute path to the workspace/repository
    project_path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct RegisterAgentParams {
    /// Absolute path to the project
    project_path: String,
    /// Program/tool name (e.g., 'opencode', 'cursor')
    program: String,
    /// Model identifier (e.g., 'claude-sonnet-4')
    model: String,
    /// Optional agent name (auto-generated if not provided)
    name: Option<String>,
    /// Optional task description
    task_description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ListAgentsParams {
    /// Absolute path to the project
    project_path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SendMessageParams {
    /// Absolute path to the project
    project_path: String,
    /// Name of the sending agent
    sender_name: String,
    /// List of recipient agent names
    to: Vec<String>,
    /// Message subject
    subject: String,
    /// Message body (markdown supported)
    body: String,
    /// Optional thread ID to continue a conversation
    thread_id: Option<String>,
    /// Message importance: low, normal, high, urgent
    importance: Option<String>,
    /// Whether acknowledgement is required
    ack_required: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FetchInboxParams {
    /// Absolute path to the project
    project_path: String,
    /// Name of the agent
    agent_name: String,
    /// Maximum number of messages to return
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct AcknowledgeMessageParams {
    /// Absolute path to the project
    project_path: String,
    /// Name of the agent
    agent_name: String,
    /// ID of the message to acknowledge
    message_id: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SearchMessagesParams {
    /// Absolute path to the project
    project_path: String,
    /// Search query
    query: String,
    /// Maximum number of results
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ReserveFilesParams {
    /// Absolute path to the project
    project_path: String,
    /// Name of the agent
    agent_name: String,
    /// List of file patterns to reserve (glob patterns supported)
    paths: Vec<String>,
    /// Time-to-live in seconds (default: 3600)
    ttl_seconds: Option<u64>,
    /// Whether reservation is exclusive (default: true)
    exclusive: Option<bool>,
    /// Reason for the reservation
    reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ReleaseReservationsParams {
    /// Absolute path to the project
    project_path: String,
    /// Name of the agent
    agent_name: String,
    /// Optional list of specific paths to release (all if not provided)
    paths: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ListReservationsParams {
    /// Absolute path to the project
    project_path: String,
}

// ============================================================================
// MCP Server
// ============================================================================

#[derive(Clone)]
struct McpServer {
    storage: Arc<Mutex<Storage>>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

impl McpServer {
    fn new(storage: Storage) -> Self {
        Self {
            storage: Arc::new(Mutex::new(storage)),
            tool_router: Self::tool_router(),
        }
    }

    fn with_storage<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Storage) -> Result<R>,
    {
        let storage = self
            .storage
            .lock()
            .map_err(|e| anyhow::anyhow!("lock error: {e}"))?;
        f(&storage)
    }
}

#[tool_router]
impl McpServer {
    /// Creates or ensures a project exists for the given workspace path
    #[tool(
        description = "Creates or ensures a project exists for the given workspace path. Returns the project slug."
    )]
    fn ensure_project(&self, params: Parameters<EnsureProjectParams>) -> String {
        match self.with_storage(|s| s.ensure_project(&params.0.project_path)) {
            Ok(project) => serde_json::json!({
                "id": project.id,
                "slug": project.slug,
                "path": project.path,
            })
            .to_string(),
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Registers an agent identity in a project
    #[tool(description = "Registers an agent identity in a project. Returns the agent profile.")]
    fn register_agent(&self, params: Parameters<RegisterAgentParams>) -> String {
        match self.with_storage(|s| {
            let project = s.ensure_project(&params.0.project_path)?;
            s.register_agent(
                project.id,
                &CreateAgent {
                    name: params.0.name.clone(),
                    program: params.0.program.clone(),
                    model: params.0.model.clone(),
                    task_description: params.0.task_description.clone(),
                },
            )
        }) {
            Ok(agent) => serde_json::json!({
                "id": agent.id,
                "name": agent.name,
                "program": agent.program,
                "model": agent.model,
                "task_description": agent.task_description,
            })
            .to_string(),
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Lists all agents registered in a project
    #[tool(description = "Lists all agents registered in a project.")]
    fn list_agents(&self, params: Parameters<ListAgentsParams>) -> String {
        match self.with_storage(|s| {
            let project = s
                .get_project_by_path(&params.0.project_path)?
                .ok_or_else(|| anyhow::anyhow!("project not found"))?;
            s.list_agents(project.id)
        }) {
            Ok(agents) => {
                let list: Vec<_> = agents
                    .iter()
                    .map(|a| {
                        serde_json::json!({
                            "name": a.name,
                            "program": a.program,
                            "model": a.model,
                        })
                    })
                    .collect();
                serde_json::json!({"agents": list}).to_string()
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Sends a message from one agent to others
    #[tool(description = "Sends a message from one agent to others. Returns the message ID.")]
    fn send_message(&self, params: Parameters<SendMessageParams>) -> String {
        match self.with_storage(|s| {
            let project = s.ensure_project(&params.0.project_path)?;
            s.send_message(
                project.id,
                &SendMessage {
                    sender_name: params.0.sender_name.clone(),
                    to: params.0.to.clone(),
                    cc: None,
                    bcc: None,
                    subject: params.0.subject.clone(),
                    body: params.0.body.clone(),
                    importance: params.0.importance.as_ref().and_then(|i| i.parse().ok()),
                    ack_required: params.0.ack_required,
                    thread_id: params.0.thread_id.clone(),
                },
            )
        }) {
            Ok(msg) => serde_json::json!({
                "id": msg.id,
                "subject": msg.subject,
                "thread_id": msg.thread_id,
            })
            .to_string(),
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Retrieves messages for an agent's inbox
    #[tool(description = "Retrieves messages for an agent's inbox.")]
    fn fetch_inbox(&self, params: Parameters<FetchInboxParams>) -> String {
        match self.with_storage(|s| {
            let project = s
                .get_project_by_path(&params.0.project_path)?
                .ok_or_else(|| anyhow::anyhow!("project not found"))?;
            s.fetch_inbox(
                project.id,
                &params.0.agent_name,
                params.0.limit.unwrap_or(20),
                0,
                false,
            )
        }) {
            Ok(messages) => {
                let list: Vec<_> = messages
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "sender": m.sender,
                            "subject": m.subject,
                            "body": m.body,
                            "importance": format!("{:?}", m.importance),
                            "thread_id": m.thread_id,
                            "created_at": m.created_at.to_rfc3339(),
                            "read_at": m.read_at.map(|t| t.to_rfc3339()),
                            "ack_required": m.ack_required,
                            "ack_at": m.ack_at.map(|t| t.to_rfc3339()),
                        })
                    })
                    .collect();
                serde_json::json!({"messages": list, "count": list.len()}).to_string()
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Acknowledges receipt of a message
    #[tool(
        description = "Acknowledges receipt of a message. Also marks it as read if not already."
    )]
    fn acknowledge_message(&self, params: Parameters<AcknowledgeMessageParams>) -> String {
        match self.with_storage(|s| {
            let project = s
                .get_project_by_path(&params.0.project_path)?
                .ok_or_else(|| anyhow::anyhow!("project not found"))?;
            let agent = s
                .get_agent_by_name(project.id, &params.0.agent_name)?
                .ok_or_else(|| anyhow::anyhow!("agent not found"))?;
            s.acknowledge(agent.id, params.0.message_id)
        }) {
            Ok(()) => serde_json::json!({"acknowledged": true, "message_id": params.0.message_id})
                .to_string(),
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Searches messages using full-text search
    #[tool(description = "Searches messages using full-text search.")]
    fn search_messages(&self, params: Parameters<SearchMessagesParams>) -> String {
        match self.with_storage(|s| {
            let project = s
                .get_project_by_path(&params.0.project_path)?
                .ok_or_else(|| anyhow::anyhow!("project not found"))?;
            s.search_messages(project.id, &params.0.query, params.0.limit.unwrap_or(20), 0)
        }) {
            Ok(messages) => {
                let list: Vec<_> = messages
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "sender": m.sender,
                            "subject": m.subject,
                            "body": m.body,
                            "thread_id": m.thread_id,
                            "created_at": m.created_at.to_rfc3339(),
                        })
                    })
                    .collect();
                serde_json::json!({"results": list, "count": list.len()}).to_string()
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Creates advisory file reservations to signal editing intent
    #[tool(
        description = "Creates advisory file reservations to signal editing intent. Returns granted reservations and any conflicts."
    )]
    fn reserve_files(&self, params: Parameters<ReserveFilesParams>) -> String {
        match self.with_storage(|s| {
            let project = s.ensure_project(&params.0.project_path)?;
            s.create_file_reservations(
                project.id,
                &CreateFileReservation {
                    agent_name: params.0.agent_name.clone(),
                    paths: params.0.paths.clone(),
                    ttl_seconds: params.0.ttl_seconds,
                    exclusive: params.0.exclusive,
                    reason: params.0.reason.clone(),
                },
            )
        }) {
            Ok(result) => {
                let granted: Vec<_> = result
                    .granted
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "path_pattern": r.path_pattern,
                            "exclusive": r.exclusive,
                            "expires_at": r.expires_at.to_rfc3339(),
                        })
                    })
                    .collect();
                let conflicts: Vec<_> = result
                    .conflicts
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "path_pattern": r.path_pattern,
                            "agent_id": r.agent_id,
                            "expires_at": r.expires_at.to_rfc3339(),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "granted": granted,
                    "conflicts": conflicts,
                })
                .to_string()
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Releases file reservations held by an agent
    #[tool(description = "Releases file reservations held by an agent.")]
    fn release_reservations(&self, params: Parameters<ReleaseReservationsParams>) -> String {
        match self.with_storage(|s| {
            let project = s
                .get_project_by_path(&params.0.project_path)?
                .ok_or_else(|| anyhow::anyhow!("project not found"))?;
            s.release_reservations(project.id, &params.0.agent_name, params.0.paths.as_deref())
        }) {
            Ok(count) => serde_json::json!({"released": count}).to_string(),
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Lists all active file reservations in a project
    #[tool(description = "Lists all active file reservations in a project.")]
    fn list_reservations(&self, params: Parameters<ListReservationsParams>) -> String {
        match self.with_storage(|s| {
            let project = s
                .get_project_by_path(&params.0.project_path)?
                .ok_or_else(|| anyhow::anyhow!("project not found"))?;
            let reservations = s.list_active_reservations(project.id, None, None)?;
            let agents = s.list_agents(project.id)?;
            Ok((reservations, agents))
        }) {
            Ok((reservations, agents)) => {
                let agent_map: std::collections::HashMap<i64, String> =
                    agents.into_iter().map(|a| (a.id, a.name)).collect();

                let list: Vec<_> = reservations
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "agent": agent_map.get(&r.agent_id).cloned().unwrap_or_default(),
                            "path_pattern": r.path_pattern,
                            "exclusive": r.exclusive,
                            "reason": r.reason,
                            "expires_at": r.expires_at.to_rfc3339(),
                        })
                    })
                    .collect();
                serde_json::json!({"reservations": list, "count": list.len()}).to_string()
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        }
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mailz".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Agent coordination server. Use ensure_project first, then register_agent, \
                 then use send_message/fetch_inbox for communication and reserve_files for \
                 coordinating file edits."
                    .to_string(),
            ),
        }
    }
}
